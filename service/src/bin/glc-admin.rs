//! `glc-admin` — reserve bridge operator CLI
//! (docs/07-implementation-plan.md Phase 5). Ported CLI shape and
//! mandatory `--note` audit discipline from the old bridge's `glc-admin`
//! (docs/01-reuse-inventory.md); governance/rotation/quorum-reassignment
//! subcommands (which depended on a P2P federation transport this bridge
//! does not have — see IMPLEMENTATION_LOG.md's Phase 5 entry) are
//! deliberately not ported. What's here: local status, this service's own
//! ledger-level directional pause and admission control (both independent
//! of the on-chain pause — see docs/09-runbook.md's "Admission control
//! (Solana->Goldcoin)" section for how the two local axes differ), and the
//! on-chain admin-gated `set_paused` instruction
//! (docs/12-management-decisions.md/IMPLEMENTATION_LOG.md's Phase 2
//! scoping decision: pause is admin-gated-immediate, not threshold-gated —
//! only attestation-key rotation gets that treatment).
//!
//! **Not yet built** (explicitly deferred, not silently missing): staged
//! multi-operator approval for attestation-key rotation, and the Goldcoin
//! vault sweep-to-fresh-vault compromise-response procedure
//! (docs/09-runbook.md's "Key compromise response"). Both need real
//! program/vault support this phase didn't build; see
//! IMPLEMENTATION_LOG.md.

use std::path::{Path, PathBuf};
use std::time::Duration;

use glc_reserve_bridge_service::admin_api::{
    audited_manual_review_hold, audited_manual_review_hold_release, audited_resume_manual_review,
    audited_set_admission, audited_set_local_pause, audited_set_robinhood_local_pause,
    audited_set_route_admission, audited_set_route_enabled,
};
use glc_reserve_bridge_service::config::Config;
use glc_reserve_bridge_service::goldcoin::coin::VaultUtxo;
use glc_reserve_bridge_service::goldcoin::payout_recovery::{
    recover_stuck_goldcoin_payout, RecoveryOutcome,
};
use glc_reserve_bridge_service::goldcoin::rpc::{
    RpcClient as GoldcoinRpcClient, RpcConfig as GoldcoinRpcConfig,
};
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::goldcoin::{hex, liquidity, split};
use glc_reserve_bridge_service::ledger::{
    CustodyTransitionKind, Direction, GoldcoinRefundState, Ledger, PendingVaultUtxoSplit,
    RebalanceKind, ReconcileUnmatchedDepositOutcome, RequestState, ReserveDirection,
    ResumeManualReviewOutcome,
};
use glc_reserve_bridge_service::ops::reserve_health;
use glc_reserve_bridge_service::rebalance;
use glc_reserve_bridge_service::solana::accounts;
use glc_reserve_bridge_service::solana::confirm::{confirm_transaction, ConfirmPolicy};
use glc_reserve_bridge_service::solana::instructions::{
    self, LimitField, PauseScope, RollingWindowDirection,
};
use glc_reserve_bridge_service::solana::manual_review_settle;
use glc_reserve_bridge_service::solana::refund::{self, RefundExecuteOutcome};
use glc_reserve_bridge_service::solana::rpc::{RealSolanaRpc, SolanaRpc};

use solana_sdk::signature::{read_keypair_file, Signer};
use solana_sdk::transaction::Transaction;

const USAGE: &str = "glc-admin — reserve bridge operator CLI

STATUS
  glc-admin status --db PATH

LOCAL LEDGER PAUSE (this service's own directional pause; independent of
the on-chain pause below and of admission control further down — see
docs/09-runbook.md)
  glc-admin pause   --db PATH --direction <goldcoin|solana> --note TEXT
  glc-admin unpause --db PATH --direction <goldcoin|solana> --note TEXT
      The GoldcoinReserve and SolanaReserve rows. There are THREE reserves:
      the third, RobinhoodReserve, has its own command with its own unpause
      guard — `robinhood-local-pause` under ROBINHOOD below. `--direction
      robinhood` is deliberately not accepted here.

LOCAL ADMISSION CONTROL (--direction goldcoin only: whether a NEWLY
observed inbound-to-Goldcoin deposit is admitted into normal processing,
versus parked to ManualReview — separate from the pause above, which keeps
working exactly as it did before this existed.

*** THIS GOVERNS BOTH INBOUND ROUTES: SolToGlc AND RhnToGlc. *** Both fold
against the same reserve_ledger row for GoldcoinReserve, so closing
admission parks new deposits on BOTH. The flag is named for the reserve,
not for a route. `robinhood-status` does NOT show it (it prints the
separate RobinhoodReserve, which backs GlcToRhn) — use `status`.

To close ONE of those two routes and leave the other running, use the
ROUTE-SCOPED ADMISSION commands below instead; this pair remains the
reserve-wide control and is unchanged by them.

Already-accepted obligations (anything already SourceFinalized or later) are
NEVER affected by this — payout processing has never been gated by either
flag and still isn't; this only ever blocks a NEW deposit from being
admitted. See docs/09-runbook.md 'Admission control (Solana->Goldcoin)'.)
  glc-admin close-admission --db PATH --direction goldcoin --note TEXT
      Always allowed. New SolToGlc AND RhnToGlc deposits fold into
      ManualReview instead of SourceFinalized until re-opened. Never
      automatic — only this command ever closes admission, and nothing ever
      auto-reopens it.
  glc-admin open-admission --db PATH --direction goldcoin --note TEXT
      Refuses unconditionally (no override) unless the GoldcoinReserve hard
      invariant currently holds (balance >= protected_minimum +
      reserved_liquidity) — never re-opens admission onto an already-broken
      reserve — and unless the automatic confirmed-liquidity gate has
      already reopened (confirmed headroom back at or above the configured
      reopen threshold). See docs/09-runbook.md 'Confirmed-liquidity
      admission safety buffer'; `status` prints both figures.

ROUTE-SCOPED ADMISSION (schema v25. The commands above are RESERVE-wide:
`pause` is the emergency stop for everything drawing on a reserve, and
`close-admission --direction goldcoin` closes SolToGlc AND RhnToGlc
together, because both fold against the same GoldcoinReserve row.

These close or open ONE inbound-to-Goldcoin route at a time, so SolToGlc
can run while RhnToGlc is shut, or the reverse.

*** BOTH AXES MUST BE OPEN. *** A route admits a new deposit only when its
own gate AND every reserve-wide gate say yes. Opening a route never
unpauses a reserve, and unpausing a reserve never opens a route whose own
gate an operator closed — reserve-wide pause remains the emergency stop
and nothing here weakens it.

SolToGlc and RhnToGlc have this gate (the two routes whose DESTINATION
reserve is Goldcoin), and since Phase H so do SolToRhn and RhnToSol (whose
source deposit is likewise observed on-chain and folded). GlcToSol and
GlcToRhn are refused: their destination reserves are Solana and Robinhood,
whose own pause is their control. This is a different axis from
`robinhood-route-enable` below, which sets ENABLEMENT.

Already-accepted obligations (anything already SourceFinalized or later)
are NEVER affected — payout processing has never been gated by any
admission flag and still isn't; this only ever blocks a NEW deposit from
being admitted.)
  glc-admin route-admission-show (--db PATH | --config PATH) [--json] [--porcelain]
      READ-ONLY. Each inbound-to-Goldcoin route's own admission gate, the
      reserve-wide pause and admission it is ANDed with, and whether the
      route would admit a deposit right now — from the SAME evaluator the
      folds and GET /chains use, so this listing and a fold cannot
      disagree. Resolves nothing: a ledger with no `route_admission` table
      (pre-v25) is reported as HAVING NO TABLE rather than as defaults.
      Writes nothing, contacts no chain, loads no keypair, reads no secret.
  glc-admin route-admission-close --db PATH --route <SolToGlc|RhnToGlc|SolToRhn|RhnToSol> --note TEXT
      Always allowed. New deposits on THAT ROUTE ONLY fold into
      ManualReview with `route_admission_closed_at_fold` instead of
      SourceFinalized, until re-opened. The other inbound route keeps
      running. Never automatic — only this command ever closes a route's
      admission, and nothing ever auto-reopens it.
      Parked requests stay recoverable (`resume-manual-review`,
      `manual-review-settle`) and refundable (`refund-manual-review`,
      `robinhood-refund`) exactly like any other fold-time park.
  glc-admin route-admission-open --db PATH --route <SolToGlc|RhnToGlc|SolToRhn|RhnToSol> --note TEXT
      Refuses unconditionally (no override) unless the route's DESTINATION
      reserve passes the same three checks `open-admission` requires: the
      hard reserve invariant holds, the mature-UTXO floor is satisfied, and
      the automatic confirmed-liquidity gate has already reopened. Opening
      a route is opening admission, so it cannot be a cheaper way around
      those checks.
      Opens ONE gate. The reserve-wide pause and admission control still
      apply on top — `route-admission-show` prints both axes together.

MANUAL REVIEW RECOVERY (Solana->Goldcoin only: resumes a request that
fold_sol_deposit itself parked in ManualReview because admission was
closed, the reserve was paused, or capacity was insufficient at that exact
moment — never a request in ManualReview for any other reason. Admission
may remain CLOSED; this never admits anything new, it only unblocks
something already accepted. Idempotent, and never creates a second
obligation — it transitions the existing request in place. See
docs/09-runbook.md 'Admission control (Solana->Goldcoin)'.)
  glc-admin resume-manual-review --db PATH --request-id N --note TEXT
      Refuses (no override) unless: the request is SolToGlc or RhnToGlc and
      currently ManualReview; its manual_review_note is one of the known
      fold-time reasons; its source deposit is already finalized; it has no
      Goldcoin payout row or destination transaction yet; NEITHER the
      Goldcoin destination address nor the source wallet is still inside its
      own rolling 24-hour window; and resuming it would not breach the
      GoldcoinReserve invariant. On success, moves the request
      ManualReview -> SourceFinalized and reserves its capacity, exactly as
      a successful fold would have — normal processing (unaffected by this
      command) picks it up from there. Refuses outright any request with a
      refund lifecycle (RefundPending/RefundBroadcast/Refunded, or any
      solana_refunds row / Refund robinhood_transactions row) — a refund,
      once begun, is permanent.
      The route is read from the request itself: one command covers both
      inbound-to-Goldcoin directions, and both run the SAME shared
      implementation, so no check can apply to one route and not the other.

MANUAL REVIEW REFUND (Solana->Goldcoin only: returns a fold-parked
deposit to the ORIGINAL Solana depositor via the on-chain
rebalance_withdraw instruction — admin signature + 2-of-3 threshold
attestation + on-chain global pause + protected minimum + per-request
nonce replay guard, none of it weakened. The destination is ALWAYS the
canonical Token-2022 ATA of the on-chain WithdrawalObligation.requester +
the configured reserve mint — derived, never accepted as input; there is
deliberately no --destination flag. The refund amount is exactly the
gross deposited amount (the SolToGlc bridge fee only accrues at
settlement, which a refunded request never reaches). Once the lifecycle
begins the request is permanently ineligible for resume-manual-review and
for any Goldcoin payout. See docs/09-runbook.md 'ManualReview refunds
(Solana->Goldcoin)'.)
  glc-admin refund-manual-review --config PATH --request-id N --note TEXT \\
      [--keypair ADMIN_KEYPAIR] [--execute]
      --config points at the same config file glc-bridge-daemon uses (the
      ledger path, Solana RPC URL, submitter keypair path, and attestation
      signer endpoints all come from it — --db alone is not enough).
      Without --execute: STRICT READ-ONLY DRY RUN — prints the request,
      the original deposit (obligation index/PDA — the bridge stores no
      deposit tx signature; the finalized obligation account IS the
      verified deposit record), the derived destination, amounts,
      reserve balance before/after, protected minimum, and every safety
      check individually. Contacts no signer, loads no keypair, writes
      nothing, broadcasts nothing.
      With --execute (requires --keypair, the on-chain admin keypair):
      re-runs every check against fresh state, requires the bridge
      ALREADY globally paused (on-chain enforced; re-checked immediately
      before simulation — this command never pauses or unpauses on its
      own), collects threshold attestations, ALWAYS simulates first,
      broadcasts only on simulation success, and confirms at finalized
      commitment before marking the request Refunded. Safe to re-run at
      any point: an already-Refunded request reports its transaction and
      exits successfully; a broadcast-but-unconfirmed refund is checked/
      finalized/rebuilt under the SAME nonce — a second transfer for the
      same request can never land (on-chain PDA replay guard).
      Eligible ManualReview reasons (conservative whitelist; everything
      else refused): admission_closed_at_fold, reserve_paused_at_fold,
      insufficient_capacity_at_fold, utxo_liquidity_low_at_fold,
      liquidity_buffer_low_at_fold, wallet_source_24h_limit,
      wallet_destination_24h_limit (and their pre-generalization
      spellings source_wallet_rate_limited / recipient_rate_limited).
  glc-admin refund-list --db PATH [--open-only]
      Read-only listing of every refund lifecycle (or only the not-yet-
      Confirmed ones with --open-only).

MANUAL REVIEW -> L1 SETTLEMENT RECOVERY (the opposite decision to a
refund: complete the user's ORIGINAL bridge request onto Goldcoin L1
instead of returning the deposit. Re-admits the parked request into the
EXISTING Goldcoin payout pipeline by transitioning it ManualReview ->
SourceFinalized; there is no second payout implementation. The bridge
keeps running throughout — no pause of any kind is required or taken.
Moves no funds and signs nothing itself: no keypair, no signer. The
destination Goldcoin address and the amount are columns on the existing
request row and cannot be supplied or changed by the operator. See
docs/09-runbook.md 'ManualReview -> L1 settlement recovery'.)
  glc-admin manual-review-settle --config PATH --request-id N --note TEXT [--execute]
      --config points at the same config file glc-bridge-daemon uses (the
      ledger path and Solana RPC come from it; the RPC is needed to
      re-prove the original deposit on-chain). No keypair is required,
      for either mode.
      Without --execute: STRICT READ-ONLY DRY RUN. Re-reads the original
      WithdrawalObligation at finalized commitment and proves it exists,
      is still Pending, and its requester and amount match the stored
      request; then trials the real re-admission and rolls it back, so
      the reported verdict is exactly what an execute would do. Writes
      nothing, broadcasts nothing.
      With --execute: proves the deposit on-chain, then performs the
      atomic audited re-admission, which independently re-runs every
      check under the write lock — state, the seven fold-time reasons,
      refund-lifecycle exclusion, existing payout, both 24h rate-limit
      windows, the mature-UTXO floor, and the Goldcoin reserve invariant.
      Reserves capacity via the SAME reserved_liquidity/pending_obligations
      mechanism normal admission uses, so it can never race a concurrent
      fold for the same capacity. Idempotent: re-running on an already
      recovered request is a safe no-op.
      Refuses any request that has entered a refund lifecycle; a
      recovered request can likewise never be refunded afterwards.
  glc-admin refund-glc-manual-review --config PATH --request-id N --note TEXT [--execute]
      Returns a GOLDCOIN deposit that was accepted on chain but can never
      settle (a GlcToSol request parked in ManualReview for
      deposit_amount_mismatch) to the wallet that sent it.
      Without --execute: STRICT READ-ONLY DRY RUN. Re-reads the deposit
      from Goldcoin RPC, traces the single spent input to derive the
      sender's address, independently checks Solana for an existing
      DepositClaim, and prints every safety check as PASS/FAIL. Contacts
      no signer, writes nothing, broadcasts nothing.
      With --execute: requires the local GoldcoinReserve pause
      (glc-admin pause --direction goldcoin), re-runs every check against
      fresh state, then builds/signs/broadcasts through the SAME 2-of-3
      vault path a normal payout uses.
      The refund pays the FULL observed deposit; the vault absorbs the
      miner fee. There is deliberately NO --destination and NO --amount:
      both come from verified chain data and cannot be set by an operator.
      See docs/09-runbook.md, section: GlcToSol ManualReview refunds.
  glc-admin glc-refund-list --db PATH [--open-only]
      Read-only listing of Goldcoin refunds. --open-only hides completed
      ones.
      A row in the Broadcast state means a refund transaction ALREADY
      EXISTS and is named by its txid — never that one still needs
      sending. The daemon reconciles each broadcast against the chain
      every tick and marks it Refunded once the transaction reaches the
      configured payout confirmation depth; the listing prints that, per
      row, so a long-open row can never be mistaken for an unsent refund.
  glc-admin manual-review-settle-list (--config PATH | --db PATH)
      Read-only listing of recovery candidates: SolToGlc requests parked
      in ManualReview for one of the seven recoverable fold-time reasons
      and not already in a refund lifecycle.
      Each candidate is shown with the verdict of the SAME dry run
      manual-review-settle performs on it, so the listing and that command
      can never disagree. Candidates that are currently refused are listed
      too, with the reason — a request waiting on a rate-limit window or
      on liquidity is exactly what this listing is for. Discovery applies
      no rate-limit or admission-time filter of its own; eligibility comes
      only from the trial.
      --config (preferred): full verdict, including the on-chain deposit
      proof — identical to running manual-review-settle on each candidate.
      --db: no RPC, so the ledger half of the verdict only; the chain half
      is not evaluated and each row says so.
  glc-admin manual-review-hold --db PATH --request-ids N[,N...] --hold-hours H --note TEXT
      Places an operator AUTO-RESUME HOLD (schema v29) on each listed
      request, one audited mutation per id. While held, the daemon's
      automatic ManualReview recovery pass skips the request entirely and
      `resume-manual-review` / `manual-review-settle` refuse it; refund
      commands are unaffected (a hold keeps a request FOR refunding).
      Per id, refuses unless the request is currently ManualReview with no
      destination txid and no destination payout row — a request already
      processing can never be held. Ids are explicit and nothing else is
      touched: a request folded after this command is exactly as it always
      was (folds never read or write the hold). `--hold-hours` records the
      moment the operator intends to act (`auto_resume_hold_until = now +
      H*3600`); it has no effect on the daemon and the hold does NOT
      expire on its own — release or refund is always an explicit act.
      Prints one verdict line per id; exits 1 if any id was refused
      (the others stay held).
  glc-admin manual-review-hold-release --db PATH --request-id N --note TEXT
      Clears one hold. No-op on an unheld request. Never changes state.
  glc-admin manual-review-hold-list --db PATH
      Read-only: every request carrying a hold, with state, route, gross,
      hold_until (and whether it has passed), note.

ROBINHOOD NETWORK (the four routes the custody contract models: GlcToRhn,
RhnToGlc and, since Phase H, SolToRhn and RhnToSol. All four ship DISABLED
at every gate. Two DIFFERENT commands open a route, on two different sides,
and both are required: `robinhood-route-enable` writes the LEDGER's
bridge_routes flag (this service's own gate, --db only, no chain contact),
while `robinhood-governance-route` submits the on-chain governance
transaction that sets the CONTRACT's flag. Neither substitutes for the
other, and neither touches the config file or the adapter.
`robinhood-preflight` READS both and reports them.)
  glc-admin robinhood-status (--db PATH | --config PATH)
      Read-only: indexer halt, scan cursor, retained anchors, observation
      counts, in-flight and stalled operations, the RhnToGlc ManualReview
      queue, and the Robinhood reserve if one is configured.
  glc-admin robinhood-manual-review-list (--db PATH | --config PATH)
      Every RhnToGlc request parked in ManualReview, with whether a refund
      or a settlement has already been begun for it. Those two are opposite,
      irreversible answers to the same question; at most one can exist.
  glc-admin robinhood-tx-show (--db PATH | --config PATH)
      [--request-id N | --stalled]
      Full state of Robinhood operations: authorization digest and how many
      of the required signatures were collected, submitter, nonce, whether
      the signed bytes are persisted, transaction hash, receipt status,
      confirmations and failure reason. Default: every in-flight operation.
      --stalled: only those reverted or moved to ManualReview, which are
      NEVER retried automatically.
  glc-admin robinhood-nonce-status --config PATH
      The submitter's nonce picture: highest allocated, last observed
      pending count, and every operation holding an unresolved nonce.
      READ-ONLY, and deliberately so — nothing in this binary sets, resets,
      skips or reallocates a nonce.
  glc-admin robinhood-treasury-withdraw --config PATH --rebalance-id N --note TEXT \\
      [--execute] [--json] [--wait-secs N]
      Executes an APPROVED `rebalance-propose --direction robinhood --kind
      withdraw` request on-chain: GlcRobinhoodBridge.executeTreasuryWithdraw,
      to the contract's immutable TREASURY, for the approved amount. There
      is no --destination and no --amount: the destination is read from the
      contract, the amount from the approved request (canonical 8dp,
      widened exactly to the contract's 18dp).
      REQUIRES depositsPaused AND payoutsPaused on the contract, read live
      by eth_call. No flag stands in for that read.
      Without --execute: dry run. Reads the ledger and the contract, prints
      every check PASS/FAIL, the amount in GLC / canonical / 18dp, the
      reserve before and after, the treasury, the pause state and the
      signer quorum — and writes, signs and broadcasts NOTHING.
      With --execute: re-runs every check against fresh state, collects the
      2-of-3 EIP-712 quorum, signs with the submitter key, broadcasts, and
      DRIVES THE RECEIPT to the configured confirmation depth (up to
      --wait-secs, default 600). Exit 0 ONLY when the operation is
      Finalized (mined, status 1, bridge event present, replay guard
      confirms, depth reached). A reverted receipt exits 1. An unresolved
      broadcast exits 1 and is resumed — same operation, same nonce, same
      bytes — by re-running the command.
      --json prints one machine-readable result object instead of prose.
  glc-admin robinhood-treasury-withdraw-status --db PATH [--rebalance-id N | --operation-id N]
      Read-only. Every treasury-withdrawal operation (or one), with its
      state, nonce, tx hash, receipt, confirmations and failure reason.
  glc-admin robinhood-recover-deposit --config PATH --tx 0xHASH [--tx 0xHASH ...] [--execute]
      Recovers ONE confirmed deposit per --tx that the scanner will never
      see: a deposit() made to the contract [robinhood.indexer] names in
      THIS config (e.g. a retired predecessor, from its own config-v1.toml)
      after the daemon moved to a successor. Fetches the receipt, refuses a
      reverted or unmined transaction, takes the single DepositCreated log
      emitted by that contract (any other address is ignored), decodes it
      with the scanner's own decoder, proves it final (confirmation_depth)
      and canonical (block hash), then — with --execute — records the
      observation as Final under (robinhood, contract, index) and folds it
      with the route CLOSED, so the request lands in ManualReview holding
      no capacity, refundable through robinhood-refund. Idempotent: a rerun
      finds the row and the request and writes nothing. Never pays out,
      never refunds, never touches another request, moves no scan cursor.
      Dry run by default: everything is verified and printed, nothing is
      written.
  glc-admin robinhood-refund --config PATH --request-id N --note TEXT
      [--execute]
      Returns a Robinhood depositor's exact principal when their deposit
      cannot safely complete to Goldcoin. The operator entry point for
      `robinhood::begin_refund`.
      Without --execute: STRICT READ-ONLY DRY RUN. Prints every ledger-side
      check individually as PASS/FAIL. Contacts no signer, reads no chain,
      writes nothing, broadcasts nothing.
      With --execute: runs the startup preflight against the deployed
      contracts, then re-runs every check against fresh state, reads the
      obligation back from the chain, collects the 2-of-3 EIP-712 quorum,
      and broadcasts. Then drives the receipt phase and reports the result.
      The broadcast phase advances every ALREADY-AUTHORIZED operation, not
      only this refund — each already had a legitimately minted quorum, and
      leaving one unbroadcast is the stall — and reports what it advanced.
      The RECIPIENT is the obligation's own on-chain `depositor` and the
      AMOUNT is its own on-chain `amount`. There is deliberately NO
      --destination and NO --amount: neither is an operator's choice, and
      the contract compares both exactly and reverts on any difference.
      There is no fee and there are no partial refunds.
      Refuses if a settlement already exists, if a Goldcoin payout
      transaction exists, if the request is not RhnToGlc in ManualReview, or
      if the obligation is anything but Pending on-chain — four independent
      checks against four independent sources of truth.
      Idempotent: re-running resumes the SAME operation under the SAME
      nonce and can never produce a second transfer.
  glc-admin robinhood-clear-halt (--config PATH | --db PATH) --note TEXT
      --expect-reason REASON [--acknowledge-orphaned-finality] [--execute]
      Clears a halted Robinhood indexer. Operator action only — nothing in
      the tick loop clears a halt, by design.
      --expect-reason is REQUIRED and must equal the stored halt reason
      (observation_conflict | post_finality_reorg |
      reorg_beyond_retained_anchors | chain_id_mismatch |
      unexpected_contract_route). Naming a different one is a refusal: a
      halt whose cause has not been diagnosed must not be cleared.
      Refuses while ANY Robinhood operation is in flight — an unresolved
      broadcast is verified against the indexer's view of the chain.
      A reorg halt additionally requires --acknowledge-orphaned-finality,
      after reviewing the finalized observations a reorg may have
      invalidated (the count is printed).
      A chain-id / wrong-contract halt additionally requires --config, and
      is cleared only if a live preflight against the configured deployment
      passes RIGHT NOW — re-verified from the chain, never asserted on the
      command line.
      Without --execute: prints the halt and every clearance check as
      PASS/FAIL and changes nothing.
  glc-admin robinhood-preflight --config PATH
      [--expect-route-enabled GlcToRhn,RhnToGlc]
      Operator preflight against the deployed contracts. Every check is
      reported PASS, FAIL or UNVERIFIED.
      UNVERIFIED is not PASS. It means either the check could not run (an
      earlier one failed and preflight stopped) or the property is not one
      an RPC read can establish at all. Every TOKEN SECURITY PROPERTY is
      permanently UNVERIFIED: mint authority, blocklist/freeze, transfer
      hooks, fee-on-transfer, pause and proxy upgradeability are properties
      of the token's CODE and governance, and a successful decimals() read
      says nothing about any of them.
      Route flags default to expecting all four CLOSED, which is how this
      ships; --expect-route-enabled names the ones a mid-rollout deployment
      expects open, so an UNEXPECTEDLY open route is a FAIL rather than
      something nobody looked at.
  glc-admin robinhood-routes (--db PATH | --config PATH) [--json] [--porcelain]
      READ-ONLY. The LEDGER gate (`bridge_routes`) for every route, exactly
      as recorded: enabled flag, updated_at, and any disabled_reason. It
      resolves nothing — a ledger with no `bridge_routes` table (pre-v24)
      is reported as HAVING NO TABLE rather than as a set of defaults,
      because `disabled` and `never recorded` have different remedies.
      Writes nothing, contacts no chain, loads no keypair, reads no secret.
      With --config it ALSO reports that config's own `[routes]` gate
      beside the ledger's, and the adapter-capability gate's static
      verdict, so the three service-side gates are read in one place and
      never mistaken for one switch.
      GlcToSol/SolToGlc appear but DO NOT USE this gate as their control:
      the migration seeds them enabled and nothing an operator does here
      changes them — their controls are `pause`/`unpause` and
      `close-admission`/`open-admission` above.
  glc-admin robinhood-route-enable  --db PATH --route <GlcToRhn|RhnToGlc|SolToRhn|RhnToSol> --note TEXT
  glc-admin robinhood-route-disable --db PATH --route <GlcToRhn|RhnToGlc|SolToRhn|RhnToSol> --note TEXT
      The LEDGER gate, and nothing else. Enabling is NECESSARY and NOT
      SUFFICIENT: the service config's own per-route flag, the chain
      adapters' capability, the contract's routeEnabled/depositsPaused/
      payoutsPaused, preflight, the signer quorum, reserve availability and
      the local pause all still stand in front of every transfer, each
      evaluated on every request and none of them touched by this command.
      Refuses GlcToSol/SolToGlc (their controls are the local pause and
      admission control above — never a second, divergent switch).
      Audited like every other mutation here; the refusals are audited too.
      Takes effect on the next request — nothing is cached, so no restart.
  glc-admin robinhood-reserve --config PATH
      The Robinhood reserve as a THIRD independent reserve: ledger balance,
      protected minimum, reserved liquidity, pending outbound obligations
      and available capacity in canonical 8dp; then the on-chain contract
      balance, encumbered reserve and both rolling-limit buckets in
      Robinhood-native 18dp. Never netted against the Goldcoin or Solana
      reserve. Prints RobinhoodReserve.paused, which is set by the command
      below and by nothing else.
  glc-admin robinhood-reserve-init --config PATH [--db PATH]
      Creates the RobinhoodReserve row in a ledger that has NEVER accounted
      a Robinhood operation, seeded with the token's live balanceOf(bridge)
      (widened exactly from 18dp to canonical 8dp) and [reserve.robinhood]'s
      protected minimum and bands. For an ISOLATED ledger pointed at a
      second deployment — the daemon is the only other thing that creates
      this row, and a daemon must never be started against a fresh ledger.
      Verifies the deployment through the same preflight the daemon uses
      before anything is written; refuses a ledger with any Robinhood
      history; refuses an existing row unless it already says exactly this
      (then a no-op). No --force exists. The ledger is [service].db_path
      unless --db names another file — be sure which one you name.
  glc-admin robinhood-local-pause --db PATH --paused <true|false> --note TEXT
      *** THE `GlcToRhn` LOCAL RESERVE GATE, AND NOTHING ELSE. ***

      Sets `reserve_ledger.paused` on the RobinhoodReserve row — the flag
      `robinhood-status` and `robinhood-reserve` print as `paused`, and one
      term of the SAME evaluator GET /chains publishes GlcToRhn's
      `available` from. It is a LOCAL, ledger-side gate: this command
      contacts no chain, contacts no signer, submits no transaction, reads
      no keypair and edits no config file.

      It is NOT, and never touches:
        - the GlcRobinhoodBridge contract's depositsPaused/payoutsPaused
          (governance, 2-of-3 quorum — `robinhood-governance-pause`)
        - the contract's routeEnabled flags (`robinhood-governance-route`)
        - ledger `bridge_routes` enablement (`robinhood-route-enable`)
        - the config file's own [routes] gate
        - GoldcoinReserve.paused or SolanaReserve.paused (`pause`/`unpause`)
        - reserve-wide or route-scoped admission (`close-admission`,
          `route-admission-close`)

      RhnToGlc IS NOT CONTROLLED BY THIS FLAG. That route settles out of
      the GOLDCOIN reserve, so its local gate is GoldcoinReserve's
      paused/admission_closed — see `status`.

      --paused true is an emergency stop and is always allowed.
      --paused false REFUSES (no override) unless the RobinhoodReserve hard
      invariant holds AND the route would actually be fundable once the
      flag clears — asked by re-running the same availability evaluator
      with the pause bit cleared, never by a second opinion about capacity.
      Idempotent in both directions, audited in both directions (refusals
      included), and prints before/after state plus the scope it affected.

ROBINHOOD GOVERNANCE (the three actions this tool may propose against the
deployed GlcRobinhoodBridge. DRY RUN unless --execute is passed. Each one
needs a 2-of-3 quorum of the PRODUCTION custody domains: this binary holds no
authorization key and cannot manufacture one, and a dev signer set is refused
outright. Verifies the chain id and the bridge contract against the config AND
the endpoint, reads governanceNonce and signerEpoch from the chain, simulates
before broadcasting, and re-reads the contract afterwards to prove it holds
what the proposal said. Never enables a route as a side effect of a limit or
pause change, never edits the config file, never restarts the daemon.)
  glc-admin robinhood-governance-set-limits --config PATH --note TEXT
      [--execute] [--inbound-min N] [--outbound-min N] [--protected-min N]
      Reconciles the contract's limit set to whatever [robinhood.policy] in
      this config says. inboundMax/outboundMax come from per_transfer_limit;
      the rolling limits are HALF of rolling_daily_limit, because the
      contract's window is a fixed bucket whose reachable worst case is 2x.
      There are NO figures in this command: change the policy with
      scripts/chain-policy.sh, then run this to make the chain match. The
      minimums and protectedMinReserve are PRESERVED from current on-chain
      state unless the explicit flags above change one (18dp atomic units).
  glc-admin robinhood-governance-pause --config PATH
      --scope <deposits|payouts> --paused <true|false> --note TEXT [--execute]
      Sets one direction's pause flag, carrying the other direction's current
      on-chain value across unchanged. Clearing a pause enables no route.
  glc-admin robinhood-governance-route --config PATH
      --route <GlcToRhn|RhnToGlc|SolToRhn|RhnToSol> --enabled <true|false> --note TEXT [--execute]
      Enables or disables ONE route's flag on the contract — any route the
      contract models (one with a route discriminator). GlcToSol and SolToGlc
      are refused: the contract never sees them, so there is no flag to set.
      Enabling a route does not unpause anything, and the service's own
      gates (config, bridge_routes, adapter capability) still stand.
  glc-admin robinhood-governance-commit-migration --config PATH
      --successor 0xADDRESS --note TEXT [--execute]
      Commits the contract to migrating its WHOLE reserve to `--successor`.
      Terminal for every route the moment it lands; there is no cancel from
      here, only a guardian's vetoMigration(). Refused unless BOTH directions
      are already paused on chain, unless the successor has code, custodies
      the same token and reports the same bridgeProtocolId(), and unless no
      migration is already committed. Verifying the successor's BYTECODE
      against this repository's build is a human step this cannot replace.
  glc-admin robinhood-governance-finalize-migration --config PATH
      --successor 0xADDRESS --note TEXT [--execute]
      Moves the ENTIRE remaining reserve to the committed successor and
      renders the contract terminal. `--successor` must equal what the chain
      holds as committed — the finalize quorum re-approves that address, it
      does not approve whatever happens to be committed. Refused while any obligation
      is still Pending (settle, refund or abandon every one first) and, on a
      deployment that carries a MIGRATION_DELAY, before migrationFinalizableAt
      — that delay is the deployed contract's own and nothing here shortens
      it. Afterwards: re-point [robinhood.indexer]/[robinhood.settlement]
      at the successor, re-point every custody domain's
      GLC_RHN_SIGNER_VERIFYING_CONTRACT, restart, and run robinhood-preflight.

PER-ROUTE FEES (the `[fees]` table: exactly one rate per EXECUTABLE route.
Every quote, every request and every fold prices from the route's own entry
— there is no global rate and no per-chain default behind it. Read-only
unless --execute is passed. Never enables a route, never reads a secret,
never restarts the daemon, and never touches an on-chain limit: the
GlcRobinhoodBridge contract stores no fee at all, so a fee change is a
config change and nothing else. See docs/20-bridge-fee.md.)
  glc-admin fees-show --config PATH [--route <GlcToSol|SolToGlc|GlcToRhn|RhnToGlc|SolToRhn|RhnToSol>]
      [--json] [--porcelain]
      Every executable route's configured rate, or one route's. Also
      reports where each rate CAME from: an explicit `[fees]` entry, or the
      documented migration fallback used when the file has no [fees]
      section (Solana routes -> the compiled-in BRIDGE_FEE_BPS, Robinhood
      routes -> [robinhood.policy].fee_bps). Names any disagreement between
      an effective rate and what [robinhood.policy] still states, because
      two numbers for the same thing is how the wrong one gets read.
  glc-admin fees-set --config PATH --route <ROUTE> (--fee-bps N | --fee-percent X)
      --note TEXT [--dry-run] [--execute]
      DRY RUN BY DEFAULT. Changes ONE route's rate and proves the others
      did not move: the candidate file is reloaded by the real config
      parser and every other route's resolved rate is compared against what
      it was before, so an edit that would disturb an unrelated route is
      refused rather than installed.
      --fee-percent takes what an operator types (6, 3, 1.5, 4); --fee-bps
      takes the exact machine value. ANY rate from 0 to 9999 bps is
      accepted — a fee is configuration, and changing one never requires
      rebuilding this binary. 0 makes the route free; 10000 bps (100%) and
      above are refused, because at 100% every transfer on that route
      would deliver nothing and above it the net entitlement would be
      negative.
      With --execute it takes a timestamped backup, installs an
      already-validated candidate with one atomic rename, and preserves
      every comment and unrelated section. It does NOT restart the daemon:
      the running process keeps pricing at the old rate until an operator
      restarts it deliberately.
      FIRST EDIT ON A CONFIG WITH NO [fees] SECTION: creating the section
      makes it authoritative, so it is created COMPLETE — seeded with the
      rates already in force — and the seeded keys are listed in the
      output.

CHAIN POLICY (the fee rate and transfer ceilings for one bridge network.
Read-only unless --execute is passed. NEVER enables a route, never reads or
writes a secret, never restarts the daemon, and never signs or submits an
on-chain governance transaction — for Robinhood it reports what the deployed
contract holds and what it WOULD need to hold, and stops there. The friendly
interactive wrapper is scripts/chain-policy.sh; these are the commands it
calls. See docs/09-runbook.md 'Chain policy management'.)
  glc-admin chain-policy-check-config --config PATH [--porcelain]
      Is that path the FULL bridge config the daemon loads? Answers with
      what the file actually is — a config, a policy FRAGMENT such as
      docs/robinhood/launch-policy.toml.example, an incomplete config, or
      not TOML at all — instead of leaving the config parser to say
      'missing field solana' about a file that was never a config. For a
      fragment it also prints, read-only, the policy that fragment states
      and the exact flags that would put it into a real config file.
      Reads one file; writes nothing. Exit 0 only for a usable config.
  glc-admin chain-policy-networks [--json] [--porcelain]
      The bridge networks that have a policy, derived from the route
      registry rather than a second list, with how each one is governed.
  glc-admin chain-policy-show --config PATH --network <solana|robinhood>
      [--json] [--porcelain] [--no-onchain]
      The CURRENT policy for one network: the configured backend values,
      how they are governed, and — for Robinhood, when [robinhood.indexer]
      permits an RPC read — the deployed contract's own limits() beside
      them with every disagreement named. --no-onchain skips the read.
  glc-admin chain-policy-validate --config PATH --network <name>
      (--fee-bps N | --fee-percent X) (--per-transfer-limit N | --per-transfer-glc X)
      (--rolling-daily-limit N | --rolling-glc X)
      Validates a candidate policy and writes NOTHING, ever. The bps/atomic
      flags take exact machine values; the percent/GLC flags take what an
      operator types (6, 1.5, 20000, 10000000) and convert exactly.
  glc-admin chain-policy-apply --config PATH --network <name> --note TEXT
      (same value flags as chain-policy-validate) [--dry-run] [--execute]
      DRY RUN BY DEFAULT. Prints the exact before/after values and the diff
      and changes nothing unless --execute is passed. With --execute it
      takes a timestamped backup, then installs a candidate file that has
      ALREADY been loaded by the real config parser, with one atomic
      rename — the config is never partially written. It does not restart
      anything: the daemon picks the change up when an operator restarts it.

UNMATCHED DEPOSIT RECONCILIATION (goldcoin::indexer recognizes an internal
vault-split output live going forward — see 'Vault UTXO splitting' below —
but a row already recorded as unmatched before that recognition existed
stays recorded until explicitly reconciled. Never deletes anything.)
  glc-admin reconcile-unmatched-deposit --db PATH --txid TXID --vout N --note TEXT
      Refuses (no override) unless (txid, vout, amount) exactly matches an
      expected output of a known Broadcast vault split — the identical
      check the indexer itself applies live. Marks the row reconciled;
      idempotent on an already-reconciled row.

ON-CHAIN (admin-gated-immediate; requires the BridgeConfig admin's keypair)
  glc-admin show-config    --rpc-url URL
  glc-admin onchain-pause   --rpc-url URL --keypair PATH --scope <global|release|deposit> --note TEXT
  glc-admin onchain-unpause --rpc-url URL --keypair PATH --scope <global|release|deposit> --note TEXT
  glc-admin set-limit --rpc-url URL --keypair PATH \\
      --field <min-transfer|per-transfer|protected-minimum|rolling-volume> \\
      --value N --note TEXT [--execute]
      Calls the on-chain set_limit instruction (admin-gated-immediate,
      same posture as onchain-pause above — see
      programs/glc-reserve-bridge/src/instructions/admin.rs module docs).
      --value is the new limit in atomic units of the Solana-side mint,
      NOT the canonical 8-decimal unit the ledger uses.
      DRY RUN unless --execute is passed: prints what the chain currently
      holds beside what you are proposing, refuses a no-op, and names the
      --value that rolls the change back. With --execute it broadcasts,
      confirms, then RE-READS the config and fails loudly if the chain
      does not hold what was proposed.
  glc-admin show-authorities --rpc-url URL
      Prints, in one place, who can currently do what: BridgeConfig.admin,
      any pending admin handover, the program's real BPF-loader upgrade
      authority, and whether the timelock PDA has been armed. These are
      INDEPENDENT authorities (changing one never changes the other) and
      keeping them on separate keys is a standing requirement — read this
      before and after any rotation.
  glc-admin transfer-admin --rpc-url URL --keypair PATH \\
      --new-admin PUBKEY --note TEXT
      Step 1 of 2 of the admin handover. Signed by the CURRENT admin.
      Nothing changes until the new admin runs accept-admin: this is the
      safeguard against handing governance to a typo, so the two steps are
      deliberately separate commands run from separate machines.
  glc-admin accept-admin --rpc-url URL --keypair PATH --note TEXT
      Step 2 of 2. Signed by the NEW admin (the key named by transfer-admin),
      on the machine that holds it. This is the call that actually moves
      BridgeConfig.admin. Verify with show-authorities afterwards.
  glc-admin rebalance-policy-show --rpc-url URL
      Prints the on-chain treasury allowlist and any queued policy change
      sitting in its governance timelock. A queued
      change you did not expect is an alert: a quorum is proposing to
      change where reserve funds may be sent, and there is still time to
      cancel it.
  glc-admin reset-rolling-window --rpc-url URL --keypair PATH \\
      --direction <glc-to-sol|sol-to-glc> --note TEXT
      Administrative override of the rolling-volume anti-drain protection:
      manually reopens the selected direction's 24h volume window (used
      volume -> 0, remaining -> the full configured rolling_volume_limit,
      quota_exhausted -> false) without waiting out its remainder. Refuses
      on-chain unless BridgeConfig.paused is already true — global pause
      first, then this, per docs/09-runbook.md's maintenance sequence.
      Applies ONLY to the two settlement directions. RebalancePolicy has
      no window or budget for this to reach: the treasury allowlist is the
      whole reserve-withdrawal policy, and it is governed by threshold-
      plus-timelock rather than being admin-editable, so no admin action
      here or elsewhere can widen where reserve funds may be sent.
      glc-to-sol resets the RELEASE window; sol-to-glc resets the DEPOSIT
      window. Does not require the individual direction's own pause, and
      never touches reserve balances, obligations, limits, or the other
      direction's window. Use only after verifying reserve/accounting
      state — see the runbook before running this in production.

GOLDCOIN PAYOUT RECOVERY (a payout stuck in Signed state after its
broadcast was rejected — e.g. request #8, Goldcoin RPC -26 'non-canonical
signature'. Never invoked automatically: Orchestrator::tick_goldcoin_
payouts always skips a request that already has a goldcoin_payouts row.
Reuses the exact same independent multi-signer signing path a normal
payout build uses — never rebroadcasts the stored signed_tx_hex verbatim,
never selects a new UTXO, never builds a second payout row. Safe to
re-run: a payout already Broadcast/Confirmed/Completed is reported and
left untouched.)
  glc-admin retry-goldcoin-payout --config PATH --request-id N --note TEXT
      --config points at the same config file glc-bridge-daemon uses
      (needs the configured vault signers + Goldcoin RPC, not just the
      ledger — see config.rs); --db alone is not enough for this command.

VAULT UTXO SPLITTING (proactively fragments one large mature root-vault
UTXO into several smaller ones, all still paying the vault's own script —
docs/09-runbook.md 'Vault UTXO splitting'. Answers the case where
coin::select correctly avoids an oversized UTXO when smaller ones exist,
but has no smaller ones to choose from. Uses the exact same 2-of-3 vault
signer path every payout uses; never exposes signer secrets. Idempotent —
a source outpoint that has already been split is reported and left alone,
never split twice. The full plan (source UTXO, output count, per-output
amount, fee, and the resulting mature-reserve effect) is always printed
BEFORE any signer is contacted, in both dry-run and --execute runs. The
reserve-safety check — the split must never itself drop mature reserve
below protected_minimum + pending_obligations — is unconditional: there is
no flag to override a failed check.)
  glc-admin split-vault-utxo --config PATH --txid TXID --vout N \\
      [--chunk-target-atomic N] --note TEXT [--execute] [--abandon]
      --txid/--vout name the exact mature root-vault UTXO to split (found
      via glc-admin status / direct ledger inspection) — never auto-picked.
      --chunk-target-atomic defaults to the config's own canonical
      change_fanout_target_atomic — one payout-chunk sizing for the whole
      service; pass it only for a deliberate one-off.
      Without --execute: prints the plan and safety check, contacts no
      signer, broadcasts nothing (dry run). With --execute: prints the
      same plan, then signs (real signer calls) and broadcasts it. A
      failed safety check refuses in both modes, with no override.
      If the outpoint already has a live split, --execute drives ITS
      lifecycle instead (resume a Built/Signed row, confirmation-check or
      re-broadcast a Broadcast row) — same code the daemon runs.
      --abandon (with --execute): operator-decided abandonment of a
      not-yet-Confirmed split the lifecycle cannot finish — audit row
      kept, source outpoint released. Refused for Confirmed splits.

REBALANCING (docs/22-production-readiness-review.md P1 'rebalancing'; this
service NEVER signs or broadcasts a fund-moving transaction itself — every
real transfer is authorized and executed entirely out of band, through
whatever real custody tooling holds the actual keys, and only ever
RECORDED here as evidence after the fact)
  glc-admin rebalance-status  --db PATH
      Read-only imbalance assessment for both reserves against their own
      configured target/warning/critical thresholds.
  glc-admin rebalance-list --db PATH [--direction <goldcoin|solana|robinhood>] [--open-only]
  glc-admin rebalance-propose --db PATH --direction <goldcoin|solana|robinhood> \\
      --kind <deposit|withdraw> (--amount N | --amount-glc GLC) --by IDENTITY \\
      --required-approvals N --note TEXT
      --amount is canonical 8-decimal atomic units for EVERY direction —
      including robinhood, whose contract speaks 18 decimals: the widening
      is done exactly, once, at execution time, never by an operator.
      --amount-glc takes whole GLC (1000 or 1000.5, at most 8 decimal
      places) and converts exactly; anything finer is refused.
      A robinhood `withdraw` is executed on-chain by
      `robinhood-treasury-withdraw` below; goldcoin and solana withdrawals
      by their own tools, and only ever RECORDED here.
  glc-admin rebalance-approve --db PATH --id N --by IDENTITY
  glc-admin rebalance-reject  --db PATH --id N --by IDENTITY --note TEXT
  glc-admin rebalance-cancel  --db PATH --id N --by IDENTITY --note TEXT
  glc-admin rebalance-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT
      Records evidence of a real transfer already authorized and executed
      outside this system — never constructs or broadcasts one itself.
  glc-admin rebalance-confirm --db PATH --id N --by IDENTITY --observed-amount N
  glc-admin rebalance-fail    --db PATH --id N --by IDENTITY --note TEXT

KEY ROTATION / VAULT SWEEP (docs/22-production-readiness-review.md P1 'key
rotation / vault sweep tooling'; generic tooling for retiring an old
attestation-signer set or Goldcoin vault identity in favor of a verified new
one. Like rebalancing, this service NEVER generates keys, signs, or executes
a real rotation/sweep itself — every real transition is authorized and
performed entirely out of band, and only ever RECORDED here as evidence
after the fact. Execution additionally requires the relevant reserve(s)
already paused: GoldcoinReserve for a vault sweep, both reserves for an
attestation-key rotation)
  glc-admin custody-list --db PATH [--kind <attestation-rotation|vault-sweep>] [--open-only]
  glc-admin custody-propose --db PATH --kind <attestation-rotation|vault-sweep> \\
      --old-identities CSV --new-identities CSV [--new-threshold N] \\
      --by IDENTITY --required-approvals N --note TEXT
  glc-admin custody-verify-identity --db PATH --id N --by IDENTITY
      Records that --by independently verified the claimed new identity.
      Required before any approval can be recorded.
  glc-admin custody-approve --db PATH --id N --by IDENTITY
  glc-admin custody-reject  --db PATH --id N --by IDENTITY --note TEXT
  glc-admin custody-cancel  --db PATH --id N --by IDENTITY --note TEXT
  glc-admin custody-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT
      Records evidence of a real rotation/sweep already authorized and
      executed outside this system — never performs one itself. Fails if
      the relevant reserve(s) are not already paused.
  glc-admin custody-confirm --db PATH --id N --by IDENTITY
  glc-admin custody-fail    --db PATH --id N --by IDENTITY --note TEXT
  glc-admin custody-rollback --db PATH --id N --by IDENTITY --note TEXT
      Records that a Failed transition's effect was reverted back to the
      old identity out of band — never performs the rollback itself.

Every mutating command requires --note (mandatory audit trail), except
rebalance-approve/-record-executed/-confirm and
custody-verify-identity/-approve/-record-executed/-confirm, which record
--by instead (a note is redundant with the approver/executor identity
itself).";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    let Some(cmd) = args.get(1) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let result = match cmd.as_str() {
        "status" => cmd_status(&args),
        "pause" => cmd_local_pause(&args, true),
        "unpause" => cmd_local_pause(&args, false),
        "close-admission" => cmd_admission(&args, true),
        "open-admission" => cmd_admission(&args, false),
        "route-admission-show" => cmd_route_admission_show(&args),
        "route-admission-close" => cmd_route_admission(&args, true),
        "route-admission-open" => cmd_route_admission(&args, false),
        "resume-manual-review" => cmd_resume_manual_review(&args),
        "refund-manual-review" => cmd_refund_manual_review(&args),
        "refund-list" => cmd_refund_list(&args),
        "manual-review-settle" => cmd_manual_review_settle(&args),
        "manual-review-settle-list" => cmd_manual_review_settle_list(&args),
        "manual-review-hold" => cmd_manual_review_hold(&args),
        "manual-review-hold-release" => cmd_manual_review_hold_release(&args),
        "manual-review-hold-list" => cmd_manual_review_hold_list(&args),
        "refund-glc-manual-review" => cmd_refund_glc_manual_review(&args),
        "glc-refund-list" => cmd_glc_refund_list(&args),
        "reconcile-unmatched-deposit" => cmd_reconcile_unmatched_deposit(&args),
        "show-config" => cmd_show_config(&args),
        "onchain-pause" => cmd_onchain_pause(&args, true),
        "onchain-unpause" => cmd_onchain_pause(&args, false),
        "set-limit" => cmd_set_limit(&args),
        "show-authorities" => cmd_show_authorities(&args),
        "transfer-admin" => cmd_transfer_admin(&args),
        "accept-admin" => cmd_accept_admin(&args),
        "rebalance-policy-show" => cmd_rebalance_policy_show(&args),
        "reset-rolling-window" => cmd_reset_rolling_window(&args),
        "retry-goldcoin-payout" => cmd_retry_goldcoin_payout(&args),
        "split-vault-utxo" => cmd_split_vault_utxo(&args),
        "rebalance-status" => cmd_rebalance_status(&args),
        "rebalance-list" => cmd_rebalance_list(&args),
        "rebalance-propose" => cmd_rebalance_propose(&args),
        "rebalance-approve" => cmd_rebalance_approve(&args),
        "rebalance-reject" => cmd_rebalance_reject(&args),
        "rebalance-cancel" => cmd_rebalance_cancel(&args),
        "rebalance-record-executed" => cmd_rebalance_record_executed(&args),
        "rebalance-confirm" => cmd_rebalance_confirm(&args),
        "rebalance-fail" => cmd_rebalance_fail(&args),
        "custody-list" => cmd_custody_list(&args),
        "custody-propose" => cmd_custody_propose(&args),
        "custody-verify-identity" => cmd_custody_verify_identity(&args),
        "custody-approve" => cmd_custody_approve(&args),
        "custody-reject" => cmd_custody_reject(&args),
        "custody-cancel" => cmd_custody_cancel(&args),
        "custody-record-executed" => cmd_custody_record_executed(&args),
        "custody-confirm" => cmd_custody_confirm(&args),
        "custody-fail" => cmd_custody_fail(&args),
        "custody-rollback" => cmd_custody_rollback(&args),
        "robinhood-status" => cmd_robinhood_status(&args),
        "robinhood-manual-review-list" => cmd_robinhood_manual_review_list(&args),
        "robinhood-tx-show" => cmd_robinhood_tx_show(&args),
        "robinhood-nonce-status" => cmd_robinhood_nonce_status(&args),
        "robinhood-refund" => cmd_robinhood_refund(&args),
        "robinhood-recover-deposit" => cmd_robinhood_recover_deposit(&args),
        "robinhood-treasury-withdraw" => cmd_robinhood_treasury_withdraw(&args),
        "robinhood-treasury-withdraw-status" => cmd_robinhood_treasury_withdraw_status(&args),
        "robinhood-clear-halt" => cmd_robinhood_clear_halt(&args),
        "robinhood-preflight" => cmd_robinhood_preflight(&args),
        "robinhood-reserve" => cmd_robinhood_reserve(&args),
        "robinhood-reserve-init" => cmd_robinhood_reserve_init(&args),
        "robinhood-local-pause" => cmd_robinhood_local_pause(&args),
        "robinhood-routes" => cmd_robinhood_routes(&args),
        "robinhood-route-enable" => cmd_robinhood_route(&args, true),
        "robinhood-route-disable" => cmd_robinhood_route(&args, false),
        "robinhood-governance-set-limits" => cmd_robinhood_governance_set_limits(&args),
        "robinhood-governance-pause" => cmd_robinhood_governance_pause(&args),
        "robinhood-governance-route" => cmd_robinhood_governance_route(&args),
        "robinhood-governance-commit-migration" => cmd_robinhood_governance_commit_migration(&args),
        "robinhood-governance-finalize-migration" => {
            cmd_robinhood_governance_finalize_migration(&args)
        }
        "fees-show" => cmd_fees_show(&args),
        "fees-set" => cmd_fees_set(&args),
        "chain-policy-check-config" => cmd_chain_policy_check_config(&args),
        "chain-policy-networks" => cmd_chain_policy_networks(&args),
        "chain-policy-show" => cmd_chain_policy_show(&args),
        "chain-policy-validate" => cmd_chain_policy_validate(&args),
        "chain-policy-apply" => cmd_chain_policy_apply(&args),
        other => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

/// Missing/malformed arguments are a usage error (exit 2), distinct from a
/// failed operation (exit 1) — same distinction `glc-audit` draws.
fn require<'a>(args: &'a [String], name: &str) -> &'a str {
    flag(args, name).unwrap_or_else(|| {
        eprintln!("missing required {name}\n\n{USAGE}");
        std::process::exit(2);
    })
}

fn require_note(args: &[String]) -> Result<&str, String> {
    match flag(args, "--note") {
        Some(n) if !n.trim().is_empty() => Ok(n),
        _ => Err("--note is required and must be non-empty (mandatory audit trail)".to_string()),
    }
}

fn parse_reserve_direction(s: &str) -> Result<ReserveDirection, String> {
    match s {
        "goldcoin" => Ok(ReserveDirection::GoldcoinReserve),
        "solana" => Ok(ReserveDirection::SolanaReserve),
        other => Err(format!(
            "unknown --direction {other} (expected goldcoin|solana)"
        )),
    }
}

/// `--direction` for the `rebalance-*` family: `goldcoin`, `solana`, and
/// `robinhood`.
///
/// `robinhood` is accepted here and NOT in [`parse_reserve_direction`],
/// which `pause`/`unpause`/`*-admission` use: the local Robinhood reserve
/// gate has its own command (`robinhood-local-pause`), and a shared parser
/// would give one family the wrong answer. A Robinhood `withdraw`
/// proposal is executable — `glc-admin robinhood-treasury-withdraw` drives
/// `GlcRobinhoodBridge.executeTreasuryWithdraw` — so it may be recorded.
fn parse_rebalance_direction(s: &str) -> Result<ReserveDirection, String> {
    match s {
        "robinhood" => Ok(ReserveDirection::RobinhoodReserve),
        other => parse_reserve_direction(other).map_err(|_| {
            format!("unknown --direction {other} (expected goldcoin|solana|robinhood)")
        }),
    }
}

fn parse_rebalance_kind(s: &str) -> Result<RebalanceKind, String> {
    match s {
        "deposit" => Ok(RebalanceKind::Deposit),
        "withdraw" => Ok(RebalanceKind::Withdraw),
        other => Err(format!(
            "unknown --kind {other} (expected deposit|withdraw)"
        )),
    }
}

fn parse_custody_kind(s: &str) -> Result<CustodyTransitionKind, String> {
    match s {
        "attestation-rotation" => Ok(CustodyTransitionKind::AttestationKeyRotation),
        "vault-sweep" => Ok(CustodyTransitionKind::GoldcoinVaultSweep),
        other => Err(format!(
            "unknown --kind {other} (expected attestation-rotation|vault-sweep)"
        )),
    }
}

fn parse_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn require_u64(args: &[String], name: &str) -> Result<u64, String> {
    require(args, name)
        .parse()
        .map_err(|e| format!("{name} must be a non-negative integer: {e}"))
}

fn require_i64(args: &[String], name: &str) -> Result<i64, String> {
    require(args, name)
        .parse()
        .map_err(|e| format!("{name} must be an integer: {e}"))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_pause_scope(s: &str) -> Result<PauseScope, String> {
    match s {
        "global" => Ok(PauseScope::Global),
        "release" => Ok(PauseScope::Release),
        "deposit" => Ok(PauseScope::Deposit),
        other => Err(format!(
            "unknown --scope {other} (expected global|release|deposit)"
        )),
    }
}

/// Reads and decodes the on-chain `BridgeConfig`, or says why it could
/// not. Shared by every command that needs to show an operator what the
/// chain currently holds before changing it.
async fn read_bridge_config(rpc: &RealSolanaRpc) -> Result<accounts::BridgeConfigSnapshot, String> {
    let pda = accounts::bridge_config_pda();
    let account = rpc
        .get_account(&pda)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!("bridge_config does not exist at {pda} — not initialized on this cluster")
        })?;
    accounts::decode_bridge_config(&account.data).map_err(|e| e.to_string())
}

/// One limit's current value, by field — so a dry run can print the real
/// "from" figure rather than asking an operator to look it up.
fn limit_value(config: &accounts::BridgeConfigSnapshot, field: LimitField) -> u64 {
    match field {
        LimitField::MinTransferAmount => config.min_transfer_amount,
        LimitField::PerTransferLimit => config.per_transfer_limit,
        LimitField::ProtectedMinimum => config.protected_minimum,
        LimitField::RollingVolumeLimit => config.rolling_volume_limit,
    }
}

/// The CLI spelling of a field, for messages that tell an operator what
/// to re-run. Kept beside [`parse_limit_field`] so the two cannot drift.
trait LimitFieldCli {
    fn as_str_cli(&self) -> &'static str;
}

impl LimitFieldCli for LimitField {
    fn as_str_cli(&self) -> &'static str {
        match self {
            LimitField::MinTransferAmount => "min-transfer",
            LimitField::PerTransferLimit => "per-transfer",
            LimitField::ProtectedMinimum => "protected-minimum",
            LimitField::RollingVolumeLimit => "rolling-volume",
        }
    }
}

fn parse_limit_field(s: &str) -> Result<LimitField, String> {
    match s {
        "min-transfer" => Ok(LimitField::MinTransferAmount),
        "per-transfer" => Ok(LimitField::PerTransferLimit),
        "protected-minimum" => Ok(LimitField::ProtectedMinimum),
        "rolling-volume" => Ok(LimitField::RollingVolumeLimit),
        other => Err(format!(
            "unknown --field {other} (expected min-transfer|per-transfer|protected-minimum|rolling-volume)"
        )),
    }
}

/// `glc-to-sol` = the RELEASE rolling-volume window (Goldcoin deposit ->
/// Solana reserve release); `sol-to-glc` = the DEPOSIT rolling-volume
/// window (Solana deposit -> Goldcoin reserve release) — the exact mapping
/// `programs/glc-reserve-bridge/src/instructions/initialize.rs` sets up
/// (`release_volume_window.direction = GoldcoinToSolana`,
/// `deposit_volume_window.direction = SolanaToGoldcoin`).
fn parse_rolling_window_direction(s: &str) -> Result<RollingWindowDirection, String> {
    match s {
        "glc-to-sol" => Ok(RollingWindowDirection::GoldcoinToSolana),
        "sol-to-glc" => Ok(RollingWindowDirection::SolanaToGoldcoin),
        other => Err(format!(
            "unknown --direction {other} (expected glc-to-sol|sol-to-glc)"
        )),
    }
}

fn cmd_status(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;

    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        match reserve_health::check(&ledger, direction, now_unix()) {
            Ok(s) => {
                println!(
                    "{direction:?}: balance={} protected_minimum={} reserved_liquidity={} \
                     pending_obligations={} accrued_fees={} immature_vault_utxo_total={} paused={} \
                     admission_closed={} invariant_holds={}",
                    s.total_reserve_balance,
                    s.protected_minimum,
                    s.reserved_liquidity,
                    s.pending_obligations,
                    s.accrued_fees,
                    s.immature_vault_utxo_total,
                    s.paused,
                    s.admission_closed,
                    s.invariant_holds
                );
                // The AUTOMATIC confirmed-liquidity gate, on its own line
                // and never folded into `admission_closed` above: an
                // operator must be able to tell "I closed this" apart
                // from "liquidity closed this", because the remedies are
                // completely different (open-admission vs. wait for
                // headroom to recover / add reserves). Goldcoin-only —
                // it governs admission of every inbound-to-Goldcoin
                // deposit, i.e. SolToGlc AND RhnToGlc — and silent when
                // the buffer is disabled on this deployment.
                if direction == ReserveDirection::GoldcoinReserve && s.admission_buffer_atomic > 0 {
                    println!(
                        "  Admission liquidity: confirmed_headroom={} buffer={} reopen_at={} \
                         liquidity_admission_closed={}{}",
                        s.confirmed_admission_headroom,
                        s.admission_buffer_atomic,
                        s.admission_reopen_atomic,
                        s.liquidity_admission_closed,
                        if s.liquidity_admission_closed {
                            " — NEW SolToGlc AND RhnToGlc deposits are parking in \
                             ManualReview; already-accepted obligations continue processing \
                             normally, and admission reopens automatically once confirmed \
                             headroom reaches reopen_at"
                        } else {
                            ""
                        }
                    );
                }
                // UTXO liquidity (docs/09-runbook.md "UTXO liquidity"):
                // reported as four distinct figures so a temporarily
                // immature payout change never reads as "reserves
                // disappeared" — the value is accounted for, just not yet
                // spendable. Solana has no UTXO-pool concept, so this line
                // is Goldcoin-only.
                if direction == ReserveDirection::GoldcoinReserve {
                    println!(
                        "  UTXO liquidity: reserve_value={} mature_spendable_capacity={} \
                         ({} UTXOs) temporarily_immature_internal_change={} ({} UTXOs){}",
                        s.total_reserve_balance,
                        s.utxo_pool.mature_available_atomic,
                        s.utxo_pool.available_utxo_count,
                        s.utxo_pool.own_unconfirmed_change_atomic,
                        s.utxo_pool.unconfirmed_change_utxo_count,
                        if s.utxo_pool_warning {
                            " — WARNING: mature UTXO pool is thin; this recovers automatically \
                             once payout change matures, but is worth an operator's attention"
                        } else {
                            ""
                        }
                    );
                    // Distinct from BOTH figures above: 0-conf-spendable
                    // bridge-created payout change is NOT confirmed
                    // reserve liquidity and is never counted in
                    // mature_spendable_capacity — shown separately so it
                    // can't be mistaken for it (docs/09-runbook.md
                    // "Zero-conf payout change").
                    println!(
                        "  zero-conf payout change (policy candidates, not confirmed liquidity): \
                         {} ({} UTXOs, {} on parent-validation hold)",
                        s.utxo_pool.zero_conf_change_candidate_atomic,
                        s.utxo_pool.zero_conf_change_candidate_count,
                        s.utxo_pool.zero_conf_change_held_count,
                    );
                }
            }
            Err(e) => println!("{direction:?}: not configured ({e})"),
        }
    }

    // The ROUTE-SCOPED admission axis, printed on its own lines and
    // never folded into the per-reserve `admission_closed` above. The
    // two are different scopes with different remedies — a reserve-wide
    // `close-admission` shuts both inbound routes, a route-scoped
    // closure shuts one — and an operator who cannot tell them apart
    // reaches for the wrong command. Silent on a pre-v25 ledger, where
    // the table does not exist and every route behaves exactly as it
    // always did.
    match ledger.route_admission_rows() {
        Ok(Some(state)) => {
            println!("Route admission (per-route, ANDed with the reserve gates above):");
            for route in glc_reserve_bridge_service::routes::Route::ADMISSION_SETTABLE {
                let row = state.row(route);
                // The live verdict from the SAME shared evaluator the
                // folds and GET /chains use, so this line and a fold
                // cannot disagree about whether the route admits.
                let verdict = route
                    .as_direction()
                    .map(|d| ledger.route_admission_blocker(d))
                    .transpose()
                    .map(Option::flatten);
                println!(
                    "  {}: route_admission={} {} admits_now={}",
                    route.as_str(),
                    match row {
                        Some(r) =>
                            if r.admission_closed {
                                "CLOSED"
                            } else {
                                "open"
                            },
                        None => "no-row(open)",
                    },
                    match row.and_then(|r| r.admission_closed_reason.as_deref()) {
                        Some(reason) => format!("reason={reason:?}"),
                        None => String::new(),
                    },
                    match &verdict {
                        Ok(None) => "yes".to_string(),
                        Ok(Some(b)) => format!("no (blocked by {})", b.as_str()),
                        Err(e) => format!("unknown ({e})"),
                    }
                );
            }
            if !state.unknown_route_ids.is_empty() {
                println!(
                    "  WARNING: route_admission holds {} unrecognised row(s): {}",
                    state.unknown_route_ids.len(),
                    state.unknown_route_ids.join(", ")
                );
            }
        }
        // Absence is a fact worth stating once, not a set of defaults:
        // the remedy is running the migration, not writing a flag.
        Ok(None) => println!(
            "Route admission: no route_admission table (pre-v25 ledger) — every route is \
             governed by the reserve-wide gates above alone, exactly as before v25"
        ),
        Err(e) => println!("Route admission: could not read route_admission ({e})"),
    }

    let manual_review: usize = Direction::ALL
        .iter()
        .map(|&d| {
            ledger
                .requests_by_state(d, RequestState::ManualReview)
                .map(|r| r.len())
                .unwrap_or(0)
        })
        .sum();
    println!("ManualReview backlog: {manual_review}");

    match ledger.post_finality_reorg_event_count() {
        Ok(0) => {}
        Ok(n) => println!(
            "WARNING: {n} post-finality reorg event(s) recorded — see \
             post_finality_reorg_events; both reserves are paused if any of these have not \
             yet been cleared by an operator"
        ),
        Err(e) => println!("could not read post_finality_reorg_events: {e}"),
    }

    Ok(())
}

/// The audit-log actor identity for this CLI invocation: `cli:<user>`,
/// so admin_audit_log rows distinguish SSH/CLI mutations from admin-API
/// ones while still naming the person (docs/27-admin-control-plane.md).
fn cli_actor() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    format!("cli:{user}")
}

fn cmd_local_pause(args: &[String], paused: bool) -> Result<(), String> {
    let db = require(args, "--db");
    let direction = parse_reserve_direction(require(args, "--direction"))?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    // Through the shared audited implementation, so a CLI pause leaves
    // the same admin_audit_log row (actor `cli:<user>`) an admin-API
    // pause would — one audit trail regardless of surface.
    audited_set_local_pause(&mut ledger, direction, paused, note, &cli_actor())
        .map_err(|e| e.to_string())?;
    println!("{direction:?} local ledger pause set to {paused} (note: {note})");
    Ok(())
}

/// Admission control (docs/09-runbook.md "Admission control
/// (Solana->Goldcoin)") — a separate axis from [`cmd_local_pause`] above.
/// Scoped to `--direction goldcoin` only: it is the reserve
/// `Ledger::fold_sol_deposit` AND `Ledger::fold_robinhood_deposit` both
/// check, so this one flag governs `SolToGlc` and `RhnToGlc` alike (see
/// `crate::ledger::admission`). `solana`/GlcToSol admission is unaffected
/// by this command and continues to depend only on the existing local
/// pause.
fn cmd_admission(args: &[String], closing: bool) -> Result<(), String> {
    let db = require(args, "--db");
    let direction = parse_reserve_direction(require(args, "--direction"))?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;

    // The direction restriction, the open path's two independent safety
    // checks (hard reserve invariant + count-based UTXO-liquidity gate),
    // and the audit row all live in the shared
    // `admin_api::audited_set_admission` — one implementation for the
    // CLI and the HTTP surface, so neither the checks nor the audit
    // trail can drift between them.
    audited_set_admission(&mut ledger, direction, closing, note, &cli_actor())
        .map_err(|e| e.to_string())?;
    println!(
        "{direction:?} admission {} (note: {note})",
        if closing { "closed" } else { "opened" }
    );
    Ok(())
}

/// Resumes a `SolToGlc` request stuck in `ManualReview` purely because it
/// was parked by `fold_sol_deposit`'s admission/pause/capacity gate — see
/// `Ledger::resume_manual_review_sol_to_glc`'s docs for the exact
/// preconditions and safety checks (unconditional, no override). Never
/// touches signer, confirmation, pause, quota, or admission logic —
/// admission may remain closed; this only ever unblocks something already
/// accepted, never admits anything new.
fn cmd_resume_manual_review(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let request_id = require_i64(args, "--request-id")?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    // Shared audited implementation: same safety checks, same audit row,
    // and the REAL invoking identity (`cli:<user>`) recorded in both the
    // admin audit log and bridge_request_state_log — never a hardcoded
    // placeholder actor.
    let (outcome, _receipt) =
        audited_resume_manual_review(&mut ledger, request_id, note, &cli_actor())
            .map_err(|e| e.to_string())?;
    match outcome {
        ResumeManualReviewOutcome::Resumed => {
            println!(
                "request {request_id}: resumed ManualReview -> SourceFinalized, capacity reserved (note: {note})"
            );
        }
        ResumeManualReviewOutcome::AlreadyResumed { state } => {
            println!(
                "request {request_id}: already resumed (state={state:?}) — nothing to do, no mutation performed"
            );
        }
    }
    Ok(())
}

/// `manual-review-hold` — see the USAGE banner. Explicit ids only; one
/// audited mutation each; every refusal lives in
/// `Ledger::set_manual_review_hold` and is reported per id.
fn cmd_manual_review_hold(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let ids_raw = require(args, "--request-ids");
    let hold_hours: i64 = require(args, "--hold-hours")
        .parse()
        .map_err(|e| format!("--hold-hours must be an integer: {e}"))?;
    if !(1..=24 * 365).contains(&hold_hours) {
        return Err("--hold-hours must be within 1..=8760".to_string());
    }
    let note = require_note(args)?;
    let mut ids = Vec::new();
    for part in ids_raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let id: i64 = part
            .parse()
            .map_err(|e| format!("--request-ids: `{part}` is not an integer: {e}"))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err("--request-ids must name at least one request".to_string());
    }
    let now = now_unix();
    let hold_until = now + hold_hours * 3600;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let actor = cli_actor();
    let mut held = 0usize;
    let mut refused = Vec::new();
    for id in &ids {
        match audited_manual_review_hold(&mut ledger, *id, hold_until, note, &actor) {
            Ok(_) => {
                held += 1;
                println!("request {id}: HELD (auto_resume_hold_until={hold_until})");
            }
            Err(e) => {
                refused.push(*id);
                println!("request {id}: REFUSED — {e}");
            }
        }
    }
    println!(
        "\n{held} held, {} refused; hold_until={hold_until} ({hold_hours}h from now); note: {note}",
        refused.len()
    );
    if refused.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} request(s) refused and NOT held: {}",
            refused.len(),
            refused
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ))
    }
}

fn cmd_manual_review_hold_release(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let request_id = require_i64(args, "--request-id")?;
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let (released, _receipt) =
        audited_manual_review_hold_release(&mut ledger, request_id, note, &cli_actor())
            .map_err(|e| e.to_string())?;
    if released {
        println!("request {request_id}: hold released (note: {note})");
    } else {
        println!("request {request_id}: not held — nothing to do, no mutation performed");
    }
    Ok(())
}

fn cmd_manual_review_hold_list(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let rows = ledger
        .held_manual_review_requests()
        .map_err(|e| e.to_string())?;
    let now = now_unix();
    println!(
        "HELD REQUESTS (schema v29 auto_resume_hold) — {} row(s), now={now}",
        rows.len()
    );
    for r in &rows {
        let until = r.auto_resume_hold_until.unwrap_or(0);
        println!(
            "  id={} route={} state={} gross={} hold_until={} ({}) note={}",
            r.id,
            r.direction.as_str(),
            r.state.as_str(),
            r.gross_amount_atomic,
            until,
            if until <= now { "ELAPSED" } else { "active" },
            r.auto_resume_hold_note.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

/// ManualReview refund — see the USAGE banner and docs/09-runbook.md
/// "ManualReview refunds (Solana->Goldcoin)". Without `--execute` this is
/// a strict read-only dry run (no signer contact, no keypair load, no
/// database write, no broadcast); with it, the full guarded pipeline in
/// `solana::refund::execute_refund` runs, re-checking everything against
/// fresh state first.
fn cmd_refund_manual_review(args: &[String]) -> Result<(), String> {
    let config_path = require(args, "--config");
    let request_id = require_i64(args, "--request-id")?;
    let note = require_note(args)?;
    let execute = args.iter().any(|a| a == "--execute");
    // The admin keypair is only touched on --execute; a dry run must not
    // require (or read) any key material at all.
    let admin_keypair_path = if execute {
        Some(require(args, "--keypair").to_string())
    } else {
        None
    };

    let config = Config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(config.solana.rpc_url.clone());
        let mut ledger = Ledger::open(&config.service.db_path).map_err(|e| {
            format!(
                "could not open ledger {}: {e}",
                config.service.db_path.display()
            )
        })?;

        let report = refund::dry_run_refund(&rpc, &ledger, request_id).await?;
        print_refund_report(&report);

        if !execute {
            println!(
                "\n--execute not supplied — DRY RUN ONLY: no signer was contacted, no keypair \
                 was loaded, nothing was written, nothing was broadcast."
            );
            return Ok(());
        }

        let admin_keypair_path = admin_keypair_path.expect("checked above");
        let admin = read_keypair_file(&admin_keypair_path)
            .map_err(|e| format!("could not read keypair {admin_keypair_path}: {e}"))?;
        let submitter = config.load_submitter().map_err(|e| e.to_string())?;
        let (attestation_signers, _vault_signers) =
            config.load_signers().await.map_err(|e| e.to_string())?;

        let outcome = refund::execute_refund(
            &rpc,
            &mut ledger,
            &attestation_signers,
            &admin,
            &submitter,
            request_id,
            note,
            &cli_actor(),
            ConfirmPolicy::default(),
        )
        .await?;
        match outcome {
            RefundExecuteOutcome::AlreadyRefunded { signature } => {
                println!(
                    "request {request_id}: already Refunded (tx {}) — nothing to do, no \
                     mutation performed",
                    signature.as_deref().unwrap_or("<unrecorded>")
                );
            }
            RefundExecuteOutcome::Confirmed { signature } => {
                println!(
                    "request {request_id}: refund CONFIRMED at finalized commitment (tx \
                     {signature}) — request is now Refunded, permanently closed (note: {note})"
                );
            }
        }
        Ok(())
    })
}

fn print_refund_report(report: &refund::RefundDryRunReport) {
    let request = &report.request;
    println!("Refund review for request {}:", request.id);
    println!("  state                     = {:?}", request.state);
    println!(
        "  manual review reason      = {}",
        report
            .db_checks
            .manual_review_reason
            .as_deref()
            .unwrap_or("<none>")
    );
    println!(
        "  gross deposited (canonical, 8 dec) = {}",
        request.gross_amount_atomic
    );
    match &report.plan {
        Ok(plan) => {
            println!(
                "  original deposit          = WithdrawalObligation #{} at {} (finalized, \
                 on-chain; the bridge stores no deposit tx signature — this obligation account \
                 IS the verified deposit record)",
                plan.obligation_index, plan.obligation_pda
            );
            println!("  original sender (owner)   = {}", plan.requester);
            println!(
                "  source token account      = {} (the deposit's canonical ATA — same account \
                 the refund returns to)",
                plan.destination_token_account
            );
            println!(
                "  refund destination        = {} ({})",
                plan.destination_token_account,
                if plan.destination_exists {
                    "exists"
                } else {
                    "missing — will be created idempotently at execute, submitter-paid"
                }
            );
            println!("  reserve mint              = {}", plan.reserve_mint);
            println!(
                "  token program             = {} (Token-2022 per BridgeConfig)",
                plan.token_program
            );
            println!(
                "  refund amount (native, {} dec) = {} — exact gross deposit; no fee applies \
                 (SolToGlc fees accrue only at settlement, never reached)",
                plan.mint_decimals, plan.amount_solana_atomic
            );
            println!(
                "  refund nonce              = {:#x} (refund domain | request id; PDA {})",
                plan.nonce, plan.nonce_pda
            );
            println!("  reserve balance (before)  = {}", plan.reserve_balance);
            println!(
                "  reserve balance (after)   = {}",
                plan.reserve_balance
                    .saturating_sub(plan.amount_solana_atomic)
            );
            println!("  protected minimum         = {}", plan.protected_minimum);
            println!(
                "  bridge globally paused    = {} (required at execute)",
                plan.bridge_paused
            );
            println!(
                "  attestation               = {} of {} keys required, epoch {}",
                plan.attestation_threshold,
                plan.attestation_keys.len(),
                plan.attestation_epoch
            );
        }
        Err(e) => println!("  chain-side verification   = FAILED: {e}"),
    }
    println!(
        "  Goldcoin payout exists    = {}",
        !report.db_checks.no_goldcoin_payout
    );
    match &report.refund {
        Some(r) => println!(
            "  prior refund              = state {} (nonce {:#x}, tx {})",
            r.state.as_str(),
            r.nonce,
            r.refund_signature.as_deref().unwrap_or("<none>")
        ),
        None => println!("  prior refund              = none"),
    }
    println!("\n  Safety checks:");
    for check in &report.checks {
        println!(
            "    [{}] {}{}",
            if check.ok { "PASS" } else { "FAIL" },
            check.name,
            if check.detail.is_empty() {
                String::new()
            } else {
                format!(" — {}", check.detail)
            }
        );
    }
    println!(
        "\n  overall: {}",
        if report.already_refunded {
            "ALREADY REFUNDED — terminal; --execute would report the existing transaction and \
             change nothing"
        } else if report.would_execute {
            "ELIGIBLE — --execute would proceed (all checks re-run against fresh state first)"
        } else if report.eligible_ignoring_pause {
            "ELIGIBLE, PENDING GLOBAL PAUSE — every request-level check passes. Engage the \
             on-chain global pause (glc-admin onchain-pause --scope global --note ...), then \
             rerun with --execute; unpause explicitly afterwards"
        } else {
            "NOT ELIGIBLE — --execute would refuse (no override exists)"
        }
    );
}

/// Read-only refund visibility — never mutates anything.
fn cmd_refund_list(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let open_only = args.iter().any(|a| a == "--open-only");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let refunds = ledger
        .list_solana_refunds(open_only)
        .map_err(|e| e.to_string())?;
    if refunds.is_empty() {
        println!(
            "no {}refund lifecycles recorded",
            if open_only { "open " } else { "" }
        );
        return Ok(());
    }
    for r in refunds {
        println!(
            "request {}: {} — obligation #{}, amount {} native, requester {}, destination {}, \
             nonce {:#x}, tx {}, reason {}, by {} (note: {})",
            r.request_id,
            r.state.as_str(),
            r.obligation_index,
            r.amount_solana_atomic,
            solana_sdk::pubkey::Pubkey::from(r.requester),
            solana_sdk::pubkey::Pubkey::from(r.destination_token_account),
            r.nonce,
            r.refund_signature.as_deref().unwrap_or("<none>"),
            r.manual_review_reason,
            r.created_by,
            r.note,
        );
    }
    Ok(())
}

/// Retroactively marks an `unmatched_goldcoin_deposits` row reconciled —
/// for rows recorded before `goldcoin::indexer` learned to recognize vault
/// split outputs (docs/09-runbook.md "Vault UTXO splitting"). Never
/// deletes the row; refuses (no override) unless it exactly matches a
/// known `Broadcast` split's expected output, the same check the indexer
/// itself now applies live going forward.
fn cmd_reconcile_unmatched_deposit(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let txid_hex = require(args, "--txid");
    let vout: u32 = require(args, "--vout")
        .parse()
        .map_err(|e| format!("--vout must be a non-negative integer: {e}"))?;
    let note = require_note(args)?;
    let txid: [u8; 32] = hex::decode_exact(txid_hex)
        .map_err(|e| format!("--txid must be 64 hex characters: {e}"))?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let outcome = ledger
        .reconcile_unmatched_goldcoin_deposit(txid, vout, note, now_unix())
        .map_err(|e| e.to_string())?;
    match outcome {
        ReconcileUnmatchedDepositOutcome::Reconciled => {
            println!(
                "unmatched deposit {txid_hex}:{vout} marked reconciled (note: {note}) — row kept for audit, not deleted"
            );
        }
        ReconcileUnmatchedDepositOutcome::AlreadyReconciled => {
            println!(
                "unmatched deposit {txid_hex}:{vout} was already reconciled — nothing to do, no mutation performed"
            );
        }
    }
    Ok(())
}

fn cmd_show_config(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());
        let pda = accounts::bridge_config_pda();
        let account = rpc
            .get_account(&pda)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!("bridge_config does not exist at {pda} — not initialized on this cluster")
            })?;
        let config = accounts::decode_bridge_config(&account.data).map_err(|e| e.to_string())?;
        println!("bridge_config ({pda}):");
        println!("  paused (global)    = {}", config.paused);
        println!("  release_paused     = {}", config.release_paused);
        println!("  deposit_paused     = {}", config.deposit_paused);
        println!("  reserve_token_mint = {}", config.reserve_token_mint);
        println!("  reserve_token_program = {}", config.reserve_token_program);
        println!("  obligation_count   = {}", config.obligation_count);
        println!("  protected_minimum  = {}", config.protected_minimum);
        println!("  per_transfer_limit = {}", config.per_transfer_limit);
        println!(
            "  rolling_volume_limit   = {} (GLOBAL, per direction — one field bounds both; \
             see docs/09-runbook.md 2026-08-22 update)",
            config.rolling_volume_limit
        );
        println!(
            "  rolling_window_seconds = {}",
            config.rolling_window_seconds
        );

        // Rolling-24h-volume quota, read live per direction — a read-only
        // projection (never itself a pause; see `accounts::
        // rolling_volume_remaining`'s docs). Never auto-clears on its
        // own reset alone requiring operator action: the window resets
        // on its own at the next bucket boundary regardless of anything
        // an operator does; only an explicit onchain-pause/-unpause ever
        // needs a human.
        let now = now_unix();
        for (label, direction_byte) in [("release (GlcToSol)", 0u8), ("deposit (SolToGlc)", 1u8)]
        {
            let window_pda = accounts::rolling_volume_window_pda(direction_byte);
            match rpc.get_account(&window_pda).await {
                Ok(Some(account)) => match accounts::decode_rolling_volume_window(&account.data) {
                    Ok(window) => {
                        let remaining = accounts::rolling_volume_remaining(
                            config.rolling_volume_limit,
                            config.rolling_window_seconds,
                            window,
                            now,
                        );
                        let exhausted = remaining < config.min_transfer_amount;
                        println!(
                            "  rolling_volume_window[{label}] ({window_pda}): remaining = {remaining} \
                             quota_exhausted = {exhausted}"
                        );
                    }
                    Err(e) => println!(
                        "  rolling_volume_window[{label}] ({window_pda}): could not decode: {e}"
                    ),
                },
                Ok(None) => println!(
                    "  rolling_volume_window[{label}] ({window_pda}): does not exist yet"
                ),
                Err(e) => println!(
                    "  rolling_volume_window[{label}] ({window_pda}): could not read: {e}"
                ),
            }
        }
        Ok(())
    })
}

fn cmd_onchain_pause(args: &[String], paused: bool) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let keypair_path = require(args, "--keypair");
    let scope = parse_pause_scope(require(args, "--scope"))?;
    let note = require_note(args)?;
    let admin = read_keypair_file(keypair_path)
        .map_err(|e| format!("could not read keypair {keypair_path}: {e}"))?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());
        let ix = instructions::set_paused(&admin.pubkey(), scope, paused);
        let blockhash = rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&admin.pubkey()), &[&admin], blockhash);
        let signature = rpc.send_transaction(&tx).await.map_err(|e| e.to_string())?;
        println!(
            "submitted set_paused(scope={scope:?}, paused={paused}) as {signature} (note: {note})"
        );
        confirm_transaction(&rpc, &signature, &blockhash, ConfirmPolicy::default())
            .await
            .map_err(|e| e.to_string())?;
        println!("confirmed.");
        Ok(())
    })
}

/// Prints every authority that governs this deployment, in one read.
///
/// The 2026-09-02 incident review found that no such command existed:
/// answering "who can move reserve funds right now?" meant hand-decoding
/// two accounts from two different programs. It also found that the
/// deployment's `BridgeConfig.admin` and its BPF-loader upgrade authority
/// were the SAME key, purely because `initialize` seeds the admin from
/// whoever holds the upgrade authority at genesis — they are otherwise
/// completely independent, and nothing had ever printed them side by side
/// for anyone to notice.
///
/// Read-only: no keypair, no transaction.
fn require_pubkey(args: &[String], name: &str) -> Result<solana_sdk::pubkey::Pubkey, String> {
    let raw = require(args, name);
    raw.parse()
        .map_err(|e| format!("{name} {raw:?} is not a valid pubkey: {e}"))
}

fn cmd_show_authorities(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());

        let config_pda = accounts::bridge_config_pda();
        let config_account = rpc
            .get_account(&config_pda)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("bridge_config does not exist at {config_pda}"))?;
        let admin = accounts::decode_bridge_config_admin(&config_account.data)
            .map_err(|e| e.to_string())?;

        println!("Program");
        println!("  program id                  = {}", accounts::PROGRAM_ID);
        println!("  bridge_config PDA           = {config_pda}");
        println!();
        println!("Operational authority (BridgeConfig)");
        println!("  admin                       = {}", admin.admin);
        match admin.pending_admin {
            Some(pending) => println!(
                "  pending admin               = {pending}  <-- a handover is IN PROGRESS; it \
                 completes when that key runs `glc-admin accept-admin`"
            ),
            None => println!("  pending admin               = (none)"),
        }
        println!("  can: set_paused, set_limit, transfer_admin, reset_rolling_volume_window,");
        println!("       propose_upgrade/cancel_upgrade, and CO-SIGN treasury/refund withdrawals");
        println!("  cannot: change the attestation keys, change the treasury allowlist,");
        println!("          change the reserve-withdrawal limits, or withdraw alone");
        println!();

        let program_data = accounts::program_data_address();
        println!("Program upgrade authority (BPF loader ProgramData {program_data})");
        let upgrade_authority = match rpc.get_account(&program_data).await {
            Err(e) => Err(e.to_string()),
            Ok(None) => Err(format!("ProgramData account {program_data} does not exist")),
            Ok(Some(account)) => accounts::decode_program_data_upgrade_authority(&account.data)
                .map_err(|e| e.to_string()),
        };
        match upgrade_authority {
            Ok(Some(authority)) => {
                println!("  upgrade authority           = {authority}");
                let armed = authority == accounts::upgrade_authority_pda();
                if armed {
                    println!(
                        "  status                      = ARMED: held by this program's own \
                         timelock PDA, so upgrades go through propose/execute with a delay"
                    );
                } else {
                    println!(
                        "  status                      = held by an EXTERNAL key; the on-chain \
                         upgrade timelock is NOT armed and this key can replace the program \
                         directly"
                    );
                }
                if authority == admin.admin {
                    println!();
                    println!(
                        "  *** WARNING: the upgrade authority and BridgeConfig.admin are the \
                         SAME key. ***"
                    );
                    println!(
                        "      These are independent authorities and must be held separately: a \
                         single"
                    );
                    println!(
                        "      compromise of this key gives an attacker both routine operations \
                         AND the"
                    );
                    println!(
                        "      ability to replace the program outright, bypassing every control \
                         in it."
                    );
                }
            }
            Ok(None) => println!(
                "  upgrade authority           = (none — the program is IMMUTABLE and can never \
                 be upgraded)"
            ),
            Err(e) => println!("  upgrade authority           = (could not read: {e})"),
        }
        Ok(())
    })
}

/// Step 1 of the two-step admin handover. See the USAGE banner.
///
/// Deliberately does nothing irreversible: after this runs, the current
/// admin is still the admin, and the handover completes only when the
/// named key signs `accept-admin` from its own machine. A typoed
/// `--new-admin` costs one wasted transaction, not the bridge.
fn cmd_transfer_admin(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let keypair_path = require(args, "--keypair");
    let new_admin = require_pubkey(args, "--new-admin")?;
    let note = require_note(args)?;
    let admin = read_keypair_file(keypair_path)
        .map_err(|e| format!("could not read keypair {keypair_path}: {e}"))?;
    if new_admin == admin.pubkey() {
        return Err(
            "--new-admin is the current admin; the on-chain instruction rejects a no-op handover"
                .to_string(),
        );
    }

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());
        let ix = instructions::transfer_admin(&admin.pubkey(), &new_admin);
        let blockhash = rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&admin.pubkey()), &[&admin], blockhash);
        let signature = rpc.send_transaction(&tx).await.map_err(|e| e.to_string())?;
        println!("submitted transfer_admin(new_admin={new_admin}) as {signature} (note: {note})");
        confirm_transaction(&rpc, &signature, &blockhash, ConfirmPolicy::default())
            .await
            .map_err(|e| e.to_string())?;
        println!("confirmed.");
        println!();
        println!(
            "STEP 1 OF 2 COMPLETE. {} is still the admin. The handover completes only when",
            admin.pubkey()
        );
        println!("{new_admin} runs, on the machine holding that key:");
        println!("  glc-admin accept-admin --rpc-url {rpc_url} --keypair PATH --note TEXT");
        println!("Verify afterwards with: glc-admin show-authorities --rpc-url {rpc_url}");
        Ok(())
    })
}

/// Step 2 of the two-step admin handover — the call that actually moves
/// `BridgeConfig.admin`. Must be signed by the key named in step 1.
fn cmd_accept_admin(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let keypair_path = require(args, "--keypair");
    let note = require_note(args)?;
    let new_admin = read_keypair_file(keypair_path)
        .map_err(|e| format!("could not read keypair {keypair_path}: {e}"))?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());
        let ix = instructions::accept_admin(&new_admin.pubkey());
        let blockhash = rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&new_admin.pubkey()),
            &[&new_admin],
            blockhash,
        );
        let signature = rpc.send_transaction(&tx).await.map_err(|e| e.to_string())?;
        println!(
            "submitted accept_admin as {} ({signature}) (note: {note})",
            new_admin.pubkey()
        );
        confirm_transaction(&rpc, &signature, &blockhash, ConfirmPolicy::default())
            .await
            .map_err(|e| e.to_string())?;
        println!(
            "confirmed — BridgeConfig.admin is now {}.",
            new_admin.pubkey()
        );
        println!("Verify with: glc-admin show-authorities --rpc-url {rpc_url}");
        Ok(())
    })
}

/// Read-only view of the reserve-withdrawal policy: where treasury funds
/// may go, how much at a time, how much per window, and whether anyone is
/// currently proposing to change any of that.
fn cmd_rebalance_policy_show(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());
        let pda = accounts::rebalance_policy_pda();
        let Some(account) = rpc.get_account(&pda).await.map_err(|e| e.to_string())? else {
            println!("No RebalancePolicy exists at {pda}.");
            println!();
            println!(
                "This means NO treasury destination is allowlisted, and treasury_withdraw fails"
            );
            println!(
                "closed for every destination. That is the safe state, not a broken one — but it"
            );
            println!(
                "also means no operator withdrawal is possible until initialize_rebalance_policy"
            );
            println!("has been run under a threshold attestation.");
            return Ok(());
        };
        let policy = accounts::decode_rebalance_policy(&account.data).map_err(|e| e.to_string())?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        println!("RebalancePolicy ({pda})");
        println!("  version                     = {}", policy.version);
        println!(
            "  allowlisted treasuries      = {}",
            policy.treasuries.len()
        );
        for (i, t) in policy.treasuries.iter().enumerate() {
            println!("    [{i}] {t}");
        }
        println!();

        let pending_pda = accounts::pending_rebalance_policy_pda();
        match rpc
            .get_account(&pending_pda)
            .await
            .map_err(|e| e.to_string())?
        {
            None => println!("No policy change is pending."),
            Some(pending_account) => {
                let pending = accounts::decode_pending_rebalance_policy(&pending_account.data)
                    .map_err(|e| e.to_string())?;
                println!("*** A POLICY CHANGE IS PENDING ({pending_pda}) ***");
                println!(
                    "  earliest execution (eta)    = {} ({}s from now)",
                    pending.eta,
                    pending.eta - now
                );
                println!(
                    "  approved under epoch        = {}",
                    pending.proposed_under_epoch
                );
                println!(
                    "  proposed treasuries         = {}",
                    pending.treasuries.len()
                );
                for (i, t) in pending.treasuries.iter().enumerate() {
                    println!("    [{i}] {t}");
                }
                println!();
                println!(
                    "If this change is not one you expect, treat it as an incident: a quorum of"
                );
                println!(
                    "attestation keys is proposing to change where reserve funds may be sent. It"
                );
                println!(
                    "can be stopped with cancel_rebalance_policy (a fresh threshold proof) at any"
                );
                println!("time before the eta above.");
            }
        }
        Ok(())
    })
}

fn cmd_set_limit(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let keypair_path = require(args, "--keypair");
    let field = parse_limit_field(require(args, "--field"))?;
    let new_value = require_u64(args, "--value")?;
    let note = require_note(args)?;
    // DRY RUN unless asked otherwise, matching
    // `robinhood-governance-set-limits`. This instruction is
    // admin-gated-IMMEDIATE with no timelock, so the reviewable moment is
    // before the broadcast and nowhere after it.
    let execute = args.iter().any(|a| a == "--execute");
    let admin = read_keypair_file(keypair_path)
        .map_err(|e| format!("could not read keypair {keypair_path}: {e}"))?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());

        // What the chain holds RIGHT NOW, so the diff an operator is
        // approving is the real one rather than one from a runbook
        // written last month.
        let before = read_bridge_config(&rpc).await?;
        let previous = limit_value(&before, field);
        println!("set_limit({}) on {}", field.as_str_cli(), rpc_url);
        println!("  current = {previous}");
        println!("  new     = {new_value}");
        if previous == new_value {
            println!(
                "  NO CHANGE — the chain already holds this value. Nothing to do; \
                 re-run only if you meant a different figure."
            );
            return Ok(());
        }
        println!(
            "  units   = atomic units of the reserve mint {} — NOT canonical 8dp",
            before.reserve_token_mint
        );
        println!("  note    = {note}");
        println!("  admin   = {}", admin.pubkey());

        if !execute {
            println!(
                "\nDRY RUN — nothing was broadcast. Re-run with --execute to apply, then \
                 verify with `glc-admin show-config --rpc-url {rpc_url}`. To roll back, run \
                 this command again with --value {previous}."
            );
            return Ok(());
        }

        let ix = instructions::set_limit(&admin.pubkey(), field, new_value);
        let blockhash = rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&admin.pubkey()), &[&admin], blockhash);
        let signature = rpc.send_transaction(&tx).await.map_err(|e| e.to_string())?;
        println!(
            "submitted set_limit(field={field:?}, new_value={new_value}) as {signature} (note: {note})"
        );
        confirm_transaction(&rpc, &signature, &blockhash, ConfirmPolicy::default())
            .await
            .map_err(|e| e.to_string())?;
        println!("confirmed.");

        // Re-read rather than trust the receipt: the same discipline
        // `robinhood-governance-set-limits` applies, and the only thing
        // that proves the chain holds what the proposal said.
        let after = read_bridge_config(&rpc).await?;
        let observed = limit_value(&after, field);
        if observed != new_value {
            return Err(format!(
                "the transaction confirmed but {} reads {observed}, not {new_value} — \
                 investigate before assuming this change took effect",
                field.as_str_cli()
            ));
        }
        println!(
            "verified: {} = {observed} (was {previous}). Roll back with --value {previous}.",
            field.as_str_cli()
        );
        Ok(())
    })
}

/// Administrative override of the rolling-volume anti-drain protection —
/// see the USAGE banner and `programs/glc-reserve-bridge/src/instructions/
/// admin.rs`'s `reset_rolling_volume_window` doc comment for the full rule
/// (admin-gated, requires `BridgeConfig.paused` already `true`, touches
/// only the selected direction's window). `--note` is required and, same
/// as every other on-chain command here, is recorded only in this
/// command's own printed output and the transaction history itself — this
/// CLI has no separate local audit-log file for on-chain actions.
fn cmd_reset_rolling_window(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let keypair_path = require(args, "--keypair");
    let direction = parse_rolling_window_direction(require(args, "--direction"))?;
    let note = require_note(args)?;
    let admin = read_keypair_file(keypair_path)
        .map_err(|e| format!("could not read keypair {keypair_path}: {e}"))?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());
        let ix = instructions::reset_rolling_volume_window(&admin.pubkey(), direction);
        let blockhash = rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&admin.pubkey()), &[&admin], blockhash);
        let signature = rpc.send_transaction(&tx).await.map_err(|e| e.to_string())?;
        println!(
            "submitted reset_rolling_volume_window(direction={direction:?}) as {signature} (note: {note})"
        );
        confirm_transaction(&rpc, &signature, &blockhash, ConfirmPolicy::default())
            .await
            .map_err(|e| e.to_string())?;
        println!("confirmed.");
        Ok(())
    })
}

// ---------------------------------------------- goldcoin payout recovery --
//
// Unlike rebalancing/key-rotation above, this command DOES sign and
// broadcast a real transaction — but never a NEW one: it only completes a
// payout `Orchestrator::build_and_broadcast_payout` already independently
// signed and left stuck after a broadcast rejection
// (`goldcoin::payout_recovery` module docs). This is why it needs
// `--config`, not just `--db`: broadcasting requires the same configured
// vault signers and Goldcoin RPC the daemon itself uses, loaded exactly
// the same mode-gated way (`Config::load_signers`).

fn cmd_retry_goldcoin_payout(args: &[String]) -> Result<(), String> {
    let config_path = require(args, "--config");
    let request_id = require_i64(args, "--request-id")?;
    let note = require_note(args)?;

    let config = Config::load(Path::new(config_path)).map_err(|e| e.to_string())?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let (_attestation_signers, vault_signers) =
            config.load_signers().await.map_err(|e| e.to_string())?;
        let vault = MultisigVault::new(
            config.operators.vault_pubkeys.clone(),
            config.operators.vault_threshold,
            config.goldcoin.network,
        )
        .map_err(|e| e.to_string())?;
        let goldcoin_rpc = GoldcoinRpcClient::new(&GoldcoinRpcConfig {
            url: config.goldcoin.rpc_url.clone(),
            user: config.goldcoin.rpc_user.clone(),
            password: config.goldcoin.rpc_password.clone(),
            connect_timeout_ms: config.goldcoin.rpc_connect_timeout_ms,
            read_timeout_ms: config.goldcoin.rpc_read_timeout_ms,
        })
        .map_err(|e| e.to_string())?;
        let mut ledger =
            Ledger::open(&config.service.db_path).map_err(|e| e.to_string())?;

        let previous = ledger
            .get_goldcoin_payout_full(request_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no Goldcoin payout record exists for request {request_id}"))?;
        println!(
            "request {request_id}: current payout state = {} (note: {note})",
            previous.state
        );

        let policy = glc_reserve_bridge_service::goldcoin::payout::PayoutPolicy {
            fee_rate_per_kb: config.goldcoin.fee_rate_per_kb,
            dust_threshold: config.goldcoin.dust_threshold,
            max_inputs: config.goldcoin.max_inputs,
            change_fanout_target_atomic: config.goldcoin.change_fanout_target_atomic,
            change_fanout_max_outputs: config.goldcoin.change_fanout_max_outputs,
            zero_conf_change_max_depth: config.goldcoin.zero_conf_change_max_depth,
            zero_conf_change_mode: config.goldcoin.zero_conf_change_mode,
            zero_conf_change_recursive_chain_limit: config.goldcoin.zero_conf_change_recursive_chain_limit,
        };
        let outcome = recover_stuck_goldcoin_payout(
            &mut ledger,
            &vault,
            &vault_signers,
            &goldcoin_rpc,
            request_id,
            config.operators.vault_threshold as usize,
            &policy,
            config.goldcoin.network,
            Duration::from_millis(config.service.signer_timeout_ms),
            now_unix(),
        )
        .await
        .map_err(|e| e.to_string())?;

        match outcome {
            RecoveryOutcome::AlreadyDone { state } => {
                println!(
                    "request {request_id}: payout is already {state} — nothing to do, no mutation performed"
                );
            }
            RecoveryOutcome::Broadcast {
                txid,
                resigned_hex_changed,
            } => {
                println!(
                    "request {request_id}: recovered and broadcast, txid = {}",
                    glc_reserve_bridge_service::goldcoin::hex::encode(&txid)
                );
                if resigned_hex_changed {
                    println!(
                        "  the re-signed transaction differs from what was previously stored — \
                         the original broadcast likely failed due to the non-canonical (high-S) \
                         signature this recovery corrects."
                    );
                } else {
                    println!(
                        "  WARNING: the re-signed transaction is BYTE-IDENTICAL to what was \
                         previously stored. Re-signing did not change anything, so if the \
                         original broadcast was rejected, that rejection likely has a cause \
                         OTHER than signature canonicalization — investigate before assuming \
                         this fix alone resolves it for future requests."
                    );
                }
            }
        }
        Ok(())
    })
}

// ------------------------------------------------------- vault UTXO splitting --
//
// Proactively fragments one large mature root-vault UTXO into several
// smaller ones, all still paying the vault's own script (never a derived
// or external destination) — see `goldcoin::split`/`signing::
// goldcoin_split` module docs and docs/09-runbook.md's "Vault UTXO
// splitting" section. Reuses the same `--config`-based wiring
// `cmd_retry_goldcoin_payout` above uses, for the identical reason: this
// needs the configured vault signers + Goldcoin RPC, not just the ledger.

// The chunk-target default is the config's own canonical
// `change_fanout_target_atomic` — ONE payout-chunk sizing for the whole
// service (2026-08-30 review: a hardcoded 12,500 GLC here silently
// diverged from the retuned 5,000 GLC canonical target). `--chunk-target-
// atomic` still overrides it for a deliberate one-off.

#[allow(clippy::too_many_arguments)]
fn print_split_plan(
    txid_hex: &str,
    vout: u32,
    plan: &split::SplitPlan,
    chunk_target_atomic: u64,
    current_mature_reserve: u64,
    protected_minimum: u64,
    pending_obligations: u64,
    mature_reserve_during_window: u64,
    reserve_after_fee: u64,
    required_floor: u64,
    safety_ok: bool,
) {
    println!("Split plan");
    println!("  source UTXO:                 {txid_hex}:{vout}");
    println!(
        "  source amount (atomic):      {}",
        plan.source.amount_atomic
    );
    println!("  chunk target (atomic):       {chunk_target_atomic}");
    println!("  outputs:                     {}", plan.output_count());
    for (i, amount) in plan.output_amounts.iter().enumerate() {
        println!("    output[{i}] (atomic):        {amount}");
    }
    println!("  total fee (atomic):          {}", plan.fee_atomic);
    println!(
        "  destination (all outputs):   vault script {}",
        hex::encode(&plan.vault_script_pubkey)
    );
    println!();
    println!("Reserve effect");
    println!("  current mature reserve (atomic):      {current_mature_reserve}");
    println!("  protected minimum (atomic):           {protected_minimum}");
    println!("  pending obligations (atomic):         {pending_obligations}");
    println!("  mature reserve during maturity window (atomic): {mature_reserve_during_window}");
    println!("  reserve value after fee (atomic):     {reserve_after_fee}");
    println!("  required floor (atomic):              {required_floor}");
    println!(
        "  safety check:                         {}",
        if safety_ok { "PASS" } else { "FAIL" }
    );
}

fn cmd_split_vault_utxo(args: &[String]) -> Result<(), String> {
    let config_path = require(args, "--config");
    let txid_hex = require(args, "--txid").to_string();
    let vout: u32 = require(args, "--vout")
        .parse()
        .map_err(|e| format!("--vout must be a non-negative integer: {e}"))?;
    let chunk_target_override: Option<u64> =
        match flag(args, "--chunk-target-atomic") {
            Some(s) => Some(s.parse().map_err(|e| {
                format!("--chunk-target-atomic must be a non-negative integer: {e}")
            })?),
            None => None,
        };
    let note = require_note(args)?;
    let execute = args.iter().any(|a| a == "--execute");
    let abandon = args.iter().any(|a| a == "--abandon");
    if abandon && !execute {
        return Err("--abandon requires --execute (abandonment is a mutation)".to_string());
    }

    let txid: [u8; 32] = hex::decode_exact(&txid_hex)
        .map_err(|e| format!("--txid must be 64 hex characters: {e}"))?;

    let config = Config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
    let chunk_target_atomic =
        chunk_target_override.unwrap_or(config.goldcoin.change_fanout_target_atomic);
    // (Checked again after the ledger lookup: --abandon with NO live
    // split row is an error, never a fall-through into building a fresh
    // split — a command whose intent is to walk away from a transaction
    // must be structurally incapable of creating one. 2026-08-30
    // third-pass review, finding 3.)

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let (_attestation_signers, vault_signers) =
            config.load_signers().await.map_err(|e| e.to_string())?;
        let vault = MultisigVault::new(
            config.operators.vault_pubkeys.clone(),
            config.operators.vault_threshold,
            config.goldcoin.network,
        )
        .map_err(|e| e.to_string())?;
        let goldcoin_rpc = GoldcoinRpcClient::new(&GoldcoinRpcConfig {
            url: config.goldcoin.rpc_url.clone(),
            user: config.goldcoin.rpc_user.clone(),
            password: config.goldcoin.rpc_password.clone(),
            connect_timeout_ms: config.goldcoin.rpc_connect_timeout_ms,
            read_timeout_ms: config.goldcoin.rpc_read_timeout_ms,
        })
        .map_err(|e| e.to_string())?;
        let mut ledger = Ledger::open(&config.service.db_path).map_err(|e| e.to_string())?;

        // A LIVE (non-Abandoned) split row for this outpoint means the
        // lifecycle already owns it: this command never builds a second
        // one, but it never falsely reports a pending row as finished
        // either (the 2026-08-30 review found `Built` rows reported as
        // "already split — nothing to do", permanently strandable with
        // shaping disabled). `Built`/`Signed` resume, `Broadcast` runs
        // the same confirm/re-broadcast/abandon maintenance the daemon
        // tick runs, `Confirmed` is genuinely done — all through the
        // IDENTICAL `goldcoin::liquidity` lifecycle functions the daemon
        // uses, never a parallel implementation. An `Abandoned` prior
        // attempt does not appear here at all: the outpoint may be split
        // afresh below.
        if let Some(existing) = ledger
            .get_vault_utxo_split(txid, vout)
            .map_err(|e| e.to_string())?
        {
            if abandon {
                // The deliberate, operator-decided release valve for a
                // split the automatic lifecycle cannot finish. Refused
                // outright for a Confirmed split — that one already
                // happened.
                if existing.state == "Confirmed" {
                    return Err(format!(
                        "split #{} is Confirmed — a completed split cannot be abandoned",
                        existing.id
                    ));
                }
                // Any split with SIGNED BYTES (Signed or Broadcast) is
                // only abandonable when the node DEFINITELY does not
                // know its transaction (2026-08-31 production-readiness
                // review, B1/H1): a Signed row can mean "broadcast
                // pre-crash, bookkeeping never recorded", so its txid is
                // derived from the stored bytes exactly as the daemon's
                // resume does; and an RPC failure NEVER means "absent" —
                // it fails closed and refuses. Only a Built row (nothing
                // was ever signed, no transaction can exist) skips the
                // probe.
                let probe_txid: Option<[u8; 32]> = match existing.state.as_str() {
                    "Built" => None,
                    _ => match (existing.txid, existing.signed_tx_hex.as_deref()) {
                        (Some(t), _) => Some(t),
                        (None, Some(signed_hex)) => {
                            let bytes = hex::decode_vec(signed_hex).map_err(|e| {
                                format!(
                                    "split #{}: stored signed_tx_hex is not valid hex ({e}) — \
                                     refusing to abandon what cannot be probed",
                                    existing.id
                                )
                            })?;
                            Some(
                                glc_reserve_bridge_service::goldcoin::tx::txid_of_serialized(
                                    &bytes,
                                ),
                            )
                        }
                        (None, None) => {
                            return Err(format!(
                                "split #{} is {} but has no txid or signed bytes — refusing \
                                 to abandon inconsistent state",
                                existing.id, existing.state
                            ));
                        }
                    },
                };
                if let Some(t_hex) = probe_txid.map(|t| hex::encode(&t)) {
                    match liquidity::probe_transaction(&goldcoin_rpc, &t_hex).await {
                        liquidity::TxProbe::Absent => {} // provably unknown: abandonable
                        liquidity::TxProbe::Known => {
                            return Err(format!(
                                "split #{} ({}): the node still knows its transaction \
                                 ({t_hex}) — it can confirm at any moment; refusing to \
                                 abandon live in-flight value. If it is stuck, wait for \
                                 eviction or investigate the transaction itself.",
                                existing.id, existing.state
                            ));
                        }
                        liquidity::TxProbe::Unknown(e) => {
                            return Err(format!(
                                "split #{} ({}): cannot verify whether the node knows \
                                 transaction {t_hex} ({e}) — refusing to abandon on \
                                 uncertainty; retry when the node is reachable",
                                existing.id, existing.state
                            ));
                        }
                    }
                }
                ledger
                    .abandon_vault_utxo_split(
                        existing.id,
                        &format!("operator abandon via split-vault-utxo: {note}"),
                        // The derived txid is persisted onto the row so
                        // the daemon's re-adoption watch covers this
                        // abandonment even for Signed rows (final
                        // review, finding 4).
                        probe_txid,
                        now_unix(),
                    )
                    .map_err(|e| e.to_string())?;
                println!(
                    "split #{} ({}) ABANDONED by operator decision — audit row kept; any \
                     phantom chunk rows were marked Spent. {}",
                    existing.id,
                    existing.state,
                    if existing.state == "Built" {
                        "The source outpoint is released back to the pool."
                    } else {
                        "The source outpoint stays Spent — its signed spender could resurface. \
                         If the node reports the transaction within the next 24h the daemon \
                         re-adopts the split automatically; after that, recovery of \
                         chain-resurrected value is a reserve-custody runbook decision."
                    }
                );
                return Ok(());
            }
            match existing.state.as_str() {
                "Confirmed" => {
                    println!(
                        "vault UTXO {txid_hex}:{vout} was already split and the split confirmed \
                         (split #{}, txid={}, {} chunk(s)) — nothing to do (note: {note})",
                        existing.id,
                        existing
                            .txid
                            .map(|t| hex::encode(&t))
                            .unwrap_or_else(|| "<none>".to_string()),
                        existing.chunk_count
                    );
                    return Ok(());
                }
                "Broadcast" => {
                    if !execute {
                        println!(
                            "split #{} for {txid_hex}:{vout} is Broadcast and awaiting \
                             confirmation — re-run with --execute to run lifecycle maintenance \
                             (confirmation check / eviction re-broadcast) now; the daemon's \
                             shaping tick does the same automatically",
                            existing.id
                        );
                        return Ok(());
                    }
                    let mut outcome = liquidity::ShapingOutcome::default();
                    liquidity::maintain_broadcast_splits(
                        &mut ledger,
                        &goldcoin_rpc,
                        Some(existing.id),
                        config.goldcoin.vault_min_confirmations,
                        &mut outcome,
                        now_unix(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    print_lifecycle_outcome(&outcome);
                    return Ok(());
                }
                "Built" | "Signed" => {
                    if !execute {
                        println!(
                            "split #{} for {txid_hex}:{vout} is {} but not yet Broadcast — \
                             re-run with --execute to resume it ({}); the daemon's shaping tick \
                             does the same automatically",
                            existing.id,
                            existing.state,
                            if existing.state == "Signed" {
                                "re-submits the EXACT already-signed transaction, no new signer \
                                 round-trip"
                            } else {
                                "re-signs the exact persisted plan through the independent \
                                 2-of-3 path"
                            }
                        );
                        return Ok(());
                    }
                    println!(
                        "split #{} for {txid_hex}:{vout} is {} — resuming (note: {note})...",
                        existing.id, existing.state
                    );
                    let pending = PendingVaultUtxoSplit {
                        id: existing.id,
                        source_txid: txid,
                        source_vout: vout,
                        state: existing.state.clone(),
                    };
                    let mut outcome = liquidity::ShapingOutcome::default();
                    liquidity::resume_pending_split(
                        &mut ledger,
                        &goldcoin_rpc,
                        &vault,
                        &vault_signers,
                        config.operators.vault_threshold as usize,
                        Duration::from_millis(config.service.signer_timeout_ms),
                        &pending,
                        &mut outcome,
                        now_unix(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    print_lifecycle_outcome(&outcome);
                    return Ok(());
                }
                other => {
                    return Err(format!(
                        "split #{} for {txid_hex}:{vout} is in unexpected state {other} — \
                         refusing to act",
                        existing.id
                    ));
                }
            }
        }

        if abandon {
            return Err(format!(
                "--abandon: no live split exists for {txid_hex}:{vout} (it may already be \
                 Abandoned) — refusing to do anything else under an abandon command"
            ));
        }

        // The full plan — source UTXO, output count, per-output amount,
        // fee, and the resulting mature-reserve effect — is computed and
        // printed here directly, BEFORE any signer is ever contacted, in
        // both dry-run and --execute runs. Uses exactly the same checks
        // `LedgerSplitSource` below independently re-runs per signer, so
        // what's printed here is exactly what gets signed.
        let row = ledger
            .get_vault_utxo(txid, vout)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no vault UTXO {txid_hex}:{vout} is known to this ledger"))?;
        if row.state != "Available" {
            return Err(format!(
                "vault UTXO {txid_hex}:{vout} is not available to split — state is {}, not Available",
                row.state
            ));
        }
        if !row
            .script_pubkey_hex
            .eq_ignore_ascii_case(&vault.script_pubkey_hex())
        {
            return Err(format!(
                "vault UTXO {txid_hex}:{vout} does not belong to the root vault — splitting is \
                 refused for a per-request derived deposit address"
            ));
        }
        let source = VaultUtxo {
            txid,
            vout,
            amount_atomic: row.amount_atomic,
            script_pubkey_hex: row.script_pubkey_hex,
        };
        // Same output-count bound the daemon applies (2026-08-31 final
        // review, finding 9): a very large source splits into at most
        // `utxo_shaping_max_outputs_per_split` correspondingly larger
        // chunks — never a hundreds-of-outputs transaction the network
        // would refuse after signing.
        let effective_chunk_target = source
            .amount_atomic
            .div_ceil(config.goldcoin.utxo_shaping_max_outputs_per_split as u64)
            .max(chunk_target_atomic);
        if effective_chunk_target != chunk_target_atomic {
            println!(
                "note: chunk target raised {chunk_target_atomic} -> {effective_chunk_target} \
                 atomic to respect the {}-output cap (each chunk is itself a later split \
                 candidate)",
                config.goldcoin.utxo_shaping_max_outputs_per_split
            );
        }
        let chunk_target_atomic = effective_chunk_target;
        let plan = split::plan_split(
            &source,
            &vault,
            chunk_target_atomic,
            config.goldcoin.fee_rate_per_kb,
        )
        .map_err(|e| e.to_string())?;

        let (current_mature_reserve, protected_minimum, _reserved_liquidity, pending_obligations) =
            ledger
                .reserve_snapshot(ReserveDirection::GoldcoinReserve)
                .map_err(|e| e.to_string())?;
        // Solvency-invariant-aligned check (2026-08-30, see
        // `signing::goldcoin_split::LedgerSplitSource` — the exact same
        // formula every signer independently re-runs): only the network
        // fee genuinely leaves the vault; every chunk output pays the
        // vault's own script and is ledger-tracked as known internal
        // value from broadcast. `mature_reserve_during_window` is printed
        // for operator awareness (how much stays individually spendable
        // while the chunks mature), but the refusal itself is on
        // `reserve_after_fee`.
        let mature_reserve_during_window =
            current_mature_reserve.saturating_sub(source.amount_atomic);
        let reserve_after_fee = current_mature_reserve.saturating_sub(plan.fee_atomic);
        let required_floor = protected_minimum + pending_obligations;
        let safety_ok = reserve_after_fee >= required_floor;

        print_split_plan(
            &txid_hex,
            vout,
            &plan,
            chunk_target_atomic,
            current_mature_reserve,
            protected_minimum,
            pending_obligations,
            mature_reserve_during_window,
            reserve_after_fee,
            required_floor,
            safety_ok,
        );

        if !safety_ok {
            println!(
                "\nRefused: this split would drop reserve value below the required floor. \
                 No signer was contacted. No transaction was broadcast. There is no override \
                 for this check."
            );
            return Err(format!(
                "refusing unsafe split: reserve_after_fee={reserve_after_fee} < \
                 required_floor={required_floor} (protected_minimum + pending_obligations)"
            ));
        }
        // Payout-liveness guard, identical to the daemon's and equally
        // non-overridable (2026-08-30 third-pass review, finding 5):
        // splitting takes the source's full value out of the MATURE pool
        // for the chunks' maturity window, and already-admitted
        // obligations need mature liquidity now — the rest of the pool
        // must cover them without this UTXO.
        let mature_total: u64 = ledger
            .available_vault_utxos()
            .map_err(|e| e.to_string())?
            .iter()
            .map(|u| u.amount_atomic)
            .sum();
        if mature_total.saturating_sub(source.amount_atomic) < pending_obligations {
            println!(
                "\nRefused: splitting this UTXO would leave the mature pool below the \
                 {pending_obligations} atomic units of already-admitted obligations — payouts \
                 keep first claim on mature liquidity. No signer was contacted. There is no \
                 override; retry once obligations drain or change matures."
            );
            return Err(format!(
                "refusing split for payout liveness: mature pool without this UTXO = {} < \
                 pending_obligations = {pending_obligations}",
                mature_total.saturating_sub(source.amount_atomic)
            ));
        }

        if !execute {
            println!(
                "\n--execute not supplied — plan assembled and safety-checked only. No signer \
                 was contacted. No transaction was broadcast. Re-run with --execute to sign and \
                 broadcast this exact plan."
            );
            return Ok(());
        }

        let threshold = config.operators.vault_threshold as usize;
        println!(
            "\n--execute supplied — claiming the source outpoint, then contacting {threshold} \
             of {} vault signers (the claim, not the signing order, is what makes a concurrent \
             daemon payout unable to race this source)...",
            vault_signers.len()
        );
        match liquidity::execute_fresh_split(
            &mut ledger,
            &goldcoin_rpc,
            &vault,
            &vault_signers,
            threshold,
            Duration::from_millis(config.service.signer_timeout_ms),
            &source,
            chunk_target_atomic,
            config.goldcoin.fee_rate_per_kb,
            note,
            now_unix(),
        )
        .await
        .map_err(|e| e.to_string())?
        {
            liquidity::FreshSplitOutcome::Broadcast { txid: broadcast_txid } => {
                println!(
                    "broadcast outcome: Accepted, txid = {}",
                    hex::encode(&broadcast_txid)
                );
                Ok(())
            }
            liquidity::FreshSplitOutcome::RefusedFloor {
                reserve_after_fee,
                required_floor,
            } => Err(format!(
                "refusing unsafe split: reserve_after_fee={reserve_after_fee} < \
                 required_floor={required_floor} (protected_minimum + pending_obligations)"
            )),
            liquidity::FreshSplitOutcome::Abandoned { split_id, reason } => Err(format!(
                "split #{split_id} could not proceed and was abandoned ({reason}) — the source \
                 outpoint is released; investigate, then re-run if appropriate"
            )),
            liquidity::FreshSplitOutcome::Deferred { split_id, reason } => Err(format!(
                "split #{split_id} was signed but its broadcast was refused ({reason}) — the \
                 row remains Signed and the daemon's resume path (or a re-run of this command) \
                 will drive it; --abandon --execute is the deliberate walk-away"
            )),
        }
    })
}

/// Prints what a `goldcoin::liquidity` lifecycle call actually did, in
/// the CLI's own voice.
fn print_lifecycle_outcome(
    outcome: &glc_reserve_bridge_service::goldcoin::liquidity::ShapingOutcome,
) {
    for id in &outcome.confirmed_split_ids {
        println!("split #{id}: first confirmation observed — marked Confirmed");
    }
    if let Some(txid) = outcome.rebroadcast_split_txid {
        println!(
            "re-broadcast evicted split transaction: txid = {}",
            hex::encode(&txid)
        );
    }
    if let Some(txid) = outcome.resumed_split_txid {
        println!("resumed split to Broadcast: txid = {}", hex::encode(&txid));
    }
    if let Some((id, reason)) = &outcome.abandoned_split {
        println!(
            "split #{id} ABANDONED: {reason} — its source outpoint is released; the audit row \
             is kept"
        );
    }
    if let Some(err) = &outcome.lifecycle_error {
        println!("lifecycle error (state unchanged, safe to retry): {err}");
    }
    if outcome.confirmed_split_ids.is_empty()
        && outcome.rebroadcast_split_txid.is_none()
        && outcome.resumed_split_txid.is_none()
        && outcome.abandoned_split.is_none()
        && outcome.lifecycle_error.is_none()
    {
        println!("no lifecycle action was needed");
    }
}

// -------------------------------------------------------------- rebalancing --
//
// This service never signs or broadcasts a fund-moving transaction for a
// rebalance — see the module-level docs on `ledger::Ledger`'s rebalance
// methods and docs/22-production-readiness-review.md. Every command here
// either reads state, records an approval/decision, or records EVIDENCE
// of a transfer an operator already executed through real custody
// tooling outside this system.

fn cmd_rebalance_status(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        match rebalance::assess(&ledger, direction) {
            Ok(a) => {
                println!(
                    "{direction:?}: severity={:?} balance={} target={} warning={} critical={} \
                     protected_minimum={}",
                    a.severity,
                    a.total_reserve_balance,
                    a.target_reserve,
                    a.warning_reserve,
                    a.critical_reserve,
                    a.protected_minimum
                );
                if let Some(suggested) = a.suggested_deposit_atomic {
                    println!(
                        "  suggested: a Deposit of {suggested} would restore target_reserve \
                         (sizing only — propose explicitly with rebalance-propose)"
                    );
                }
            }
            Err(e) => println!("{direction:?}: not configured ({e})"),
        }
    }
    Ok(())
}

fn cmd_rebalance_list(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let direction = flag(args, "--direction")
        .map(parse_rebalance_direction)
        .transpose()?;
    let open_only = args.iter().any(|a| a == "--open-only");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let requests = ledger
        .list_rebalances(direction, open_only)
        .map_err(|e| e.to_string())?;
    if requests.is_empty() {
        println!("no rebalance requests found");
    }
    for r in requests {
        println!(
            "#{} {:?} {:?} amount={} state={:?} reason={:?} requested_by={} approvals={}/{}{}",
            r.id,
            r.direction,
            r.kind,
            r.amount_atomic,
            r.state,
            r.reason,
            r.requested_by,
            r.approved_by.len(),
            r.required_approvals,
            r.tx_reference
                .map(|t| format!(" tx_reference={t}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// `--amount N` (canonical 8dp atomic) or `--amount-glc GLC` (whole GLC,
/// at most 8 decimal places, converted EXACTLY through
/// `chain_policy::human::parse_glc`). Exactly one of the two.
///
/// Canonical for every direction, robinhood included. The contract's
/// 18-decimal unit is 10^10 times finer; that widening happens once, at
/// execution, inside a typed conversion — never here, and never by an
/// operator multiplying. An operator who types a Robinhood-atomic figure
/// into `--amount` by mistake proposes 10^10 times too much, which the
/// dry run's human-readable GLC line then makes obvious.
fn parse_rebalance_amount(args: &[String]) -> Result<u64, String> {
    match (flag(args, "--amount"), flag(args, "--amount-glc")) {
        (Some(_), Some(_)) => Err("pass --amount OR --amount-glc, not both".to_string()),
        (None, None) => {
            Err("missing required --amount (canonical atomic) or --amount-glc".to_string())
        }
        (Some(_), None) => require_u64(args, "--amount"),
        (None, Some(glc)) => glc_reserve_bridge_service::chain_policy::human::parse_glc(glc)
            .map(|c| c.0)
            .map_err(|e| format!("--amount-glc: {e}")),
    }
}

fn cmd_rebalance_propose(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let direction = parse_rebalance_direction(require(args, "--direction"))?;
    let kind = parse_rebalance_kind(require(args, "--kind"))?;
    let amount = parse_rebalance_amount(args)?;
    let by = require(args, "--by");
    let required_approvals: u32 = require(args, "--required-approvals")
        .parse()
        .map_err(|e| format!("--required-approvals must be a positive integer: {e}"))?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let id = ledger
        .propose_rebalance(
            direction,
            kind,
            amount,
            note,
            by,
            required_approvals,
            now_unix(),
        )
        .map_err(|e| e.to_string())?;
    println!(
        "proposed rebalance #{id}: {direction:?} {kind:?} {amount} canonical atomic = {} \
         (requires {required_approvals} approval(s))",
        glc_reserve_bridge_service::chain_policy::human::format_glc(amount)
    );
    if direction == ReserveDirection::RobinhoodReserve && kind == RebalanceKind::Withdraw {
        println!(
            "  execute after approval with: glc-admin robinhood-treasury-withdraw --config PATH \
             --rebalance-id {id} --note TEXT [--execute] [--json]"
        );
    }
    Ok(())
}

fn cmd_rebalance_approve(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let outcome = ledger
        .approve_rebalance(id, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id}: {outcome:?}");
    Ok(())
}

fn cmd_rebalance_reject(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .reject_rebalance(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id} rejected (note: {note})");
    Ok(())
}

fn cmd_rebalance_cancel(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .cancel_rebalance(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id} cancelled (note: {note})");
    Ok(())
}

fn cmd_rebalance_record_executed(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let tx_reference = require(args, "--tx-reference");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .record_rebalance_executed(id, tx_reference, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id} recorded executed (tx_reference: {tx_reference}) — this command did NOT construct or broadcast any transaction");
    Ok(())
}

fn cmd_rebalance_confirm(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let observed_amount = require_u64(args, "--observed-amount")?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .confirm_rebalance(id, observed_amount, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id} confirmed (observed_amount={observed_amount})");
    Ok(())
}

fn cmd_rebalance_fail(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .fail_rebalance(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id} marked failed (note: {note})");
    Ok(())
}

// ----------------------------------------------------- key rotation / vault sweep --
//
// This service never generates keys, signs, or executes a real
// rotation/sweep for a custody transition — see the module-level docs on
// `ledger::Ledger`'s custody-transition methods and
// docs/22-production-readiness-review.md. Every command here either
// reads state, records a verification/approval/decision, or records
// EVIDENCE of a rotation/sweep an operator already executed through real
// custody tooling outside this system.

fn cmd_custody_list(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let kind = flag(args, "--kind").map(parse_custody_kind).transpose()?;
    let open_only = args.iter().any(|a| a == "--open-only");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let transitions = ledger
        .list_custody_transitions(kind, open_only)
        .map_err(|e| e.to_string())?;
    if transitions.is_empty() {
        println!("no custody transitions found");
    }
    for t in transitions {
        println!(
            "#{} {:?} state={:?} old={:?} new={:?}{} reason={:?} requested_by={} approvals={}/{}{}",
            t.id,
            t.kind,
            t.state,
            t.old_identities,
            t.new_identities,
            t.new_threshold
                .map(|n| format!(" new_threshold={n}"))
                .unwrap_or_default(),
            t.reason,
            t.requested_by,
            t.approved_by.len(),
            t.required_approvals,
            t.tx_reference
                .map(|tx| format!(" tx_reference={tx}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn cmd_custody_propose(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let kind = parse_custody_kind(require(args, "--kind"))?;
    let old_identities = parse_csv(require(args, "--old-identities"));
    let new_identities = parse_csv(require(args, "--new-identities"));
    let new_threshold = flag(args, "--new-threshold")
        .map(|s| {
            s.parse::<u32>()
                .map_err(|e| format!("--new-threshold must be a positive integer: {e}"))
        })
        .transpose()?;
    let by = require(args, "--by");
    let required_approvals: u32 = require(args, "--required-approvals")
        .parse()
        .map_err(|e| format!("--required-approvals must be a positive integer: {e}"))?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let id = ledger
        .propose_custody_transition(
            kind,
            &old_identities,
            &new_identities,
            new_threshold,
            note,
            by,
            required_approvals,
            now_unix(),
        )
        .map_err(|e| e.to_string())?;
    println!(
        "proposed custody transition #{id}: {kind:?} (requires {required_approvals} approval(s))"
    );
    Ok(())
}

fn cmd_custody_verify_identity(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .verify_new_identity(id, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id}: new identity verified by {by}");
    Ok(())
}

fn cmd_custody_approve(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let outcome = ledger
        .approve_custody_transition(id, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id}: {outcome:?}");
    Ok(())
}

fn cmd_custody_reject(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .reject_custody_transition(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} rejected (note: {note})");
    Ok(())
}

fn cmd_custody_cancel(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .cancel_custody_transition(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} cancelled (note: {note})");
    Ok(())
}

fn cmd_custody_record_executed(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let tx_reference = require(args, "--tx-reference");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .record_custody_transition_executed(id, tx_reference, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} recorded executed (tx_reference: {tx_reference}) — this command did NOT perform any rotation/sweep");
    Ok(())
}

fn cmd_custody_confirm(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .confirm_custody_transition(id, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} confirmed");
    Ok(())
}

fn cmd_custody_fail(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .fail_custody_transition(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} marked failed (note: {note})");
    Ok(())
}

fn cmd_custody_rollback(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .rollback_custody_transition(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} marked rolled back (note: {note})");
    Ok(())
}

/// ManualReview -> Goldcoin L1 settlement recovery. Dry run by default;
/// `--execute` performs the atomic audited re-admission. Needs no
/// keypair and no signer in either mode: re-admission is a ledger
/// transition, and the funds movement happens later in the normal payout
/// pipeline under the vault signers it already uses.
fn cmd_manual_review_settle(args: &[String]) -> Result<(), String> {
    let config_path = require(args, "--config");
    let request_id = require_i64(args, "--request-id")?;
    let note = require_note(args)?;
    let execute = args.iter().any(|a| a == "--execute");

    let config = Config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(config.solana.rpc_url.clone());
        let mut ledger = Ledger::open(&config.service.db_path).map_err(|e| {
            format!(
                "could not open ledger {}: {e}",
                config.service.db_path.display()
            )
        })?;
        let now = now_unix();

        let report =
            manual_review_settle::dry_run_settle(&rpc, &mut ledger, request_id, now).await?;
        print_settle_report(&report);

        if !execute {
            println!(
                "\n--execute not supplied — DRY RUN ONLY: nothing was written, nothing was \
                 broadcast, and no signer or keypair was involved."
            );
            return Ok(());
        }
        if !report.would_settle {
            return Err(
                "refusing to execute: the dry run above did not clear. Fix the named cause \
                 and re-run — there is no override."
                    .to_string(),
            );
        }

        let outcome =
            manual_review_settle::execute_settle(&rpc, &mut ledger, request_id, note, &cli_actor())
                .await?;
        match outcome {
            ResumeManualReviewOutcome::Resumed => println!(
                "request {request_id}: re-admitted ManualReview -> SourceFinalized, capacity \
                 reserved. The normal Goldcoin payout pipeline will pick it up on the next \
                 daemon tick (note: {note})"
            ),
            ResumeManualReviewOutcome::AlreadyResumed { state } => println!(
                "request {request_id}: already recovered (state={state:?}) — nothing to do, no \
                 mutation performed"
            ),
        }
        Ok(())
    })
}

fn print_settle_report(report: &manual_review_settle::SettleDryRunReport) {
    let r = &report.request;
    println!("ManualReview -> L1 settlement review for request {}:", r.id);
    println!("  state                     = {:?}", r.state);
    println!(
        "  manual review reason      = {}",
        r.manual_review_note.as_deref().unwrap_or("<none>")
    );
    println!(
        "  destination (Goldcoin)    = {} (from the original request; not operator-supplied)",
        String::from_utf8_lossy(&r.recipient)
    );
    println!(
        "  amount (net, destination) = {} atomic (from the original request; not \
         operator-supplied)",
        r.net_destination_atomic
    );
    println!("  gross deposited (canonical) = {}", r.gross_amount_atomic);

    match &report.chain {
        Ok(v) => {
            println!(
                "  original deposit          = WithdrawalObligation #{} at {} — PROVEN at \
                 finalized commitment",
                v.obligation_index, v.obligation_pda
            );
            println!("  original depositor        = {}", v.requester);
            println!(
                "  on-chain status           = {} (Pending — no settlement evidence)",
                v.status
            );
            println!(
                "  on-chain amount           = {} native (matches stored gross narrowed at {} \
                 decimals)",
                v.onchain_amount, v.mint_decimals
            );
            println!(
                "  on-chain destination      = {} (matches the stored recipient this payout \
                 will use)",
                String::from_utf8_lossy(&v.onchain_glc_address)
            );
        }
        Err(e) => println!("  on-chain verification     = FAILED: {e}"),
    }

    let c = &report.context;
    println!("  GoldcoinReserve balance   = {}", c.total_reserve_balance);
    println!("  protected minimum         = {}", c.protected_minimum);
    println!("  reserved liquidity        = {}", c.reserved_liquidity);
    println!("  pending obligations       = {}", c.pending_obligations);
    println!(
        "  available capacity        = {} (needs {})",
        c.available_capacity, c.net_destination_atomic
    );
    println!(
        "  mature UTXO count         = {} ({} atomic mature)",
        c.available_utxo_count, c.mature_available_atomic
    );
    println!(
        "  recipient rate-limited    = {}",
        c.recipient_rate_limited_until
            .map(|t| format!("until {t}"))
            .unwrap_or_else(|| "no".to_string())
    );
    println!(
        "  source wallet rate-limited= {}",
        c.source_wallet_rate_limited_until
            .map(|t| format!("until {t}"))
            .unwrap_or_else(|| "no".to_string())
    );
    if c.admission_buffer_atomic > 0 {
        // A closed gate is the one refusal the capacity figures above
        // cannot explain: headroom can look ample while admission stays
        // shut, because it reopens only on a genuine recovery to
        // admission_reopen_atomic.
        println!(
            "  admission safety buffer   = {} (reopens at {}) — gate {}",
            c.admission_buffer_atomic,
            c.admission_reopen_atomic,
            if c.liquidity_admission_closed {
                "CLOSED: recovery is refused until confirmed headroom recovers"
            } else {
                "open"
            }
        );
    }

    println!("\n  Ledger verdict (real re-admission, trialled and rolled back):");
    match &report.ledger {
        glc_reserve_bridge_service::ledger::ResumeDryRunOutcome::WouldResume => {
            println!("    WOULD RE-ADMIT — every ledger check passes")
        }
        glc_reserve_bridge_service::ledger::ResumeDryRunOutcome::AlreadyResumed { state } => {
            println!("    ALREADY RECOVERED (state={state:?}) — executing would be a safe no-op")
        }
        glc_reserve_bridge_service::ledger::ResumeDryRunOutcome::WouldRefuse { reason } => {
            println!("    WOULD REFUSE — {reason}")
        }
    }

    println!(
        "\n  overall: {}",
        if report.would_settle {
            "ELIGIBLE — --execute would re-admit this request into the normal Goldcoin payout \
             pipeline"
        } else {
            "NOT ELIGIBLE — --execute would refuse (no override exists)"
        }
    );
}

/// Read-only recovery-candidate listing.
///
/// Every listed candidate carries the verdict of the SAME trial
/// `manual-review-settle --request-id N` reports for it, so the two
/// surfaces can never disagree about whether a request is recoverable.
/// With `--config` (RPC available) each row is the FULL verdict, on-chain
/// deposit proof included — literally `dry_run_settle` per candidate.
/// With only `--db` the chain half cannot be evaluated, so the row shows
/// the ledger half and says so.
///
/// Candidates that are currently REFUSED are still listed, with the
/// reason. A request blocked by a rate-limit window that ages out, or by
/// headroom that recovers, is precisely what an operator is looking for
/// here; hiding it was the reported production defect.
fn cmd_manual_review_settle_list(args: &[String]) -> Result<(), String> {
    match (flag(args, "--config"), flag(args, "--db")) {
        (Some(config_path), _) => settle_list_with_chain(config_path),
        (None, Some(db)) => settle_list_ledger_only(db),
        (None, None) => {
            eprintln!("missing required --db (or --config for the full, chain-verified listing)\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

/// `--db` only: the RPC-free listing. Reports the ledger half of the
/// verdict, which is the half the database can answer.
fn settle_list_ledger_only(db: &str) -> Result<(), String> {
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let candidates = manual_review_settle::list_candidates(&mut ledger, now_unix())?;
    if candidates.is_empty() {
        println!("no ManualReview requests are currently recoverable for L1 settlement");
        return Ok(());
    }
    let ready = candidates
        .iter()
        .filter(|c| c.ledger_would_resume())
        .count();
    for c in &candidates {
        print_candidate_row(&c.request);
        match &c.ledger {
            glc_reserve_bridge_service::ledger::ResumeDryRunOutcome::WouldResume => println!(
                "    LEDGER VERDICT: WOULD RE-ADMIT — run `manual-review-settle --config PATH \
                 --request-id {}` to also prove the deposit on chain",
                c.request.id
            ),
            glc_reserve_bridge_service::ledger::ResumeDryRunOutcome::AlreadyResumed { state } => {
                println!("    ALREADY RECOVERED (state={state:?})")
            }
            glc_reserve_bridge_service::ledger::ResumeDryRunOutcome::WouldRefuse { reason } => {
                println!("    NOT YET — {reason}")
            }
        }
    }
    println!(
        "\n{} candidate(s); {ready} would re-admit on the ledger side right now. This listing \
         did NOT verify any deposit on chain — pass --config instead of --db for that.",
        candidates.len()
    );
    Ok(())
}

/// `--config`: the full listing. Runs the identical `dry_run_settle` the
/// single-request command runs, for every candidate, so a row's verdict
/// here and that command's `overall:` line are the same computation.
fn settle_list_with_chain(config_path: &str) -> Result<(), String> {
    let config = Config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(config.solana.rpc_url.clone());
        let mut ledger = Ledger::open(&config.service.db_path).map_err(|e| {
            format!(
                "could not open ledger {}: {e}",
                config.service.db_path.display()
            )
        })?;
        let reports =
            manual_review_settle::list_candidate_reports(&rpc, &mut ledger, now_unix()).await?;
        if reports.is_empty() {
            println!("no ManualReview requests are currently recoverable for L1 settlement");
            return Ok(());
        }
        let eligible = reports.iter().filter(|r| r.would_settle).count();
        for report in &reports {
            print_candidate_row(&report.request);
            if let Err(e) = &report.chain {
                println!("    CHAIN PROOF FAILED — {e}");
            }
            match &report.ledger {
                glc_reserve_bridge_service::ledger::ResumeDryRunOutcome::WouldResume => {
                    println!("    Ledger verdict: WOULD RE-ADMIT")
                }
                glc_reserve_bridge_service::ledger::ResumeDryRunOutcome::AlreadyResumed {
                    state,
                } => println!("    ALREADY RECOVERED (state={state:?})"),
                glc_reserve_bridge_service::ledger::ResumeDryRunOutcome::WouldRefuse { reason } => {
                    println!("    Ledger verdict: WOULD REFUSE — {reason}")
                }
            }
            println!(
                "    overall: {}",
                if report.would_settle {
                    "ELIGIBLE"
                } else {
                    "NOT ELIGIBLE"
                }
            );
        }
        println!(
            "\n{} candidate(s); {eligible} ELIGIBLE right now. Each verdict above is the same \
             dry run `manual-review-settle --request-id N` performs, nothing was written, and \
             nothing was broadcast.",
            reports.len()
        );
        Ok(())
    })
}

fn print_candidate_row(r: &glc_reserve_bridge_service::ledger::BridgeRequest) {
    println!(
        "request {}: reason {} — destination {}, net {} atomic, gross {} canonical, \
         obligation #{}",
        r.id,
        r.manual_review_note.as_deref().unwrap_or("<none>"),
        String::from_utf8_lossy(&r.recipient),
        r.net_destination_atomic,
        r.gross_amount_atomic,
        r.source_obligation_index
            .map(|i| i.to_string())
            .unwrap_or_else(|| "<none>".to_string()),
    );
}

// ---------------------------------------------------------------------------
// GlcToSol ManualReview refunds (Goldcoin side)
// ---------------------------------------------------------------------------

/// Adapts the real Goldcoin RPC client to the narrow surface the refund
/// path needs.
struct RefundGoldcoinRpc(GoldcoinRpcClient);

impl glc_reserve_bridge_service::goldcoin::refund::RefundRpc for RefundGoldcoinRpc {
    async fn get_raw_transaction(
        &self,
        txid_hex: &str,
    ) -> Result<
        glc_reserve_bridge_service::goldcoin::rpc::DecodedTransaction,
        glc_reserve_bridge_service::goldcoin::rpc::RpcError,
    > {
        self.0.get_raw_transaction(txid_hex).await
    }
    async fn send_raw_transaction(
        &self,
        hex: &str,
    ) -> Result<
        glc_reserve_bridge_service::goldcoin::rpc::BroadcastOutcome,
        glc_reserve_bridge_service::goldcoin::rpc::RpcError,
    > {
        self.0.send_raw_transaction(hex).await
    }
}

/// Adapts the Solana RPC to the single account-existence read the
/// no-release witness needs.
struct ReleaseWitness(RealSolanaRpc);

impl glc_reserve_bridge_service::goldcoin::refund::ReleaseWitnessRpc for ReleaseWitness {
    async fn account_exists(&self, pubkey: &solana_sdk::pubkey::Pubkey) -> Result<bool, String> {
        use glc_reserve_bridge_service::solana::rpc::SolanaRpc;
        self.0
            .get_account(pubkey)
            .await
            .map(|a| a.is_some())
            .map_err(|e| e.to_string())
    }
}

fn refund_payout_policy(
    config: &Config,
) -> glc_reserve_bridge_service::goldcoin::payout::PayoutPolicy {
    glc_reserve_bridge_service::goldcoin::payout::PayoutPolicy {
        fee_rate_per_kb: config.goldcoin.fee_rate_per_kb,
        dust_threshold: config.goldcoin.dust_threshold,
        max_inputs: config.goldcoin.max_inputs,
        change_fanout_target_atomic: config.goldcoin.change_fanout_target_atomic,
        change_fanout_max_outputs: config.goldcoin.change_fanout_max_outputs,
        zero_conf_change_max_depth: config.goldcoin.zero_conf_change_max_depth,
        zero_conf_change_mode: config.goldcoin.zero_conf_change_mode,
        zero_conf_change_recursive_chain_limit: config
            .goldcoin
            .zero_conf_change_recursive_chain_limit,
    }
}

fn cmd_refund_glc_manual_review(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::goldcoin::refund;

    let config_path = require(args, "--config");
    let request_id = require_i64(args, "--request-id")?;
    let note = require_note(args)?;
    let execute = args.iter().any(|a| a == "--execute");

    // There is deliberately no --destination and no --amount. Both are
    // derived from verified chain evidence; an operator flag that could
    // set either would be the single most dangerous thing this command
    // could offer.
    for forbidden in ["--destination", "--amount", "--force", "--yes"] {
        if args
            .iter()
            .any(|a| a == forbidden || a.starts_with(&format!("{forbidden}=")))
        {
            return Err(format!(
                "{forbidden} is not a valid option for this command. The refund destination and \
                 amount are derived from verified Goldcoin chain data and cannot be supplied or \
                 overridden by an operator."
            ));
        }
    }

    let config = Config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
    let vault = MultisigVault::new(
        config.operators.vault_pubkeys.clone(),
        config.operators.vault_threshold,
        config.goldcoin.network,
    )
    .map_err(|e| e.to_string())?;
    let policy = refund_payout_policy(&config);
    // The approved spec pins this to vault_min_confirmations, and the
    // daemon's executor uses the same value — a dry run must predict
    // exactly what the execute will enforce.
    let required_confirmations = config.goldcoin.vault_min_confirmations;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let goldcoin = RefundGoldcoinRpc(
            GoldcoinRpcClient::new(&GoldcoinRpcConfig {
                url: config.goldcoin.rpc_url.clone(),
                user: config.goldcoin.rpc_user.clone(),
                password: config.goldcoin.rpc_password.clone(),
                connect_timeout_ms: config.goldcoin.rpc_connect_timeout_ms,
                read_timeout_ms: config.goldcoin.rpc_read_timeout_ms,
            })
            .map_err(|e| e.to_string())?,
        );
        let solana = ReleaseWitness(RealSolanaRpc::new(config.solana.rpc_url.clone()));
        let ledger = Ledger::open(&config.service.db_path).map_err(|e| {
            format!(
                "could not open ledger {}: {e}",
                config.service.db_path.display()
            )
        })?;

        let report = refund::dry_run_refund(
            &goldcoin,
            &solana,
            &ledger,
            request_id,
            &vault,
            &policy,
            config.goldcoin.network,
            required_confirmations,
        )
        .await
        .map_err(|e| e.to_string())?;
        print_glc_refund_report(&report, required_confirmations);

        if !execute {
            println!(
                "\n  DRY RUN — nothing was written, no signer was contacted, no transaction was \
                 built or broadcast."
            );
            return Ok(());
        }

        if !report.would_refund {
            return Err(
                "REFUSING — the dry run above did not pass every check; --execute will not \
                 proceed."
                    .to_string(),
            );
        }

        println!("\n  Executing refund...");
        // Execution happens in the DAEMON, which owns the vault signer
        // clients and their tokens. This CLI never holds a signer, a key,
        // or a signer token: it sends a request id and an audit note to an
        // authenticated, privately-bound endpoint and prints what came
        // back. Every value that decides where money goes is derived
        // server-side and re-verified there immediately before signing.
        let view = call_daemon_execute_glc_refund(&config, request_id, note).await?;
        print_glc_refund_execute_result(&view);
        Ok(())
    })
}

fn print_glc_refund_report(
    report: &glc_reserve_bridge_service::goldcoin::refund::RefundDryRunReport,
    required_confirmations: i64,
) {
    use glc_reserve_bridge_service::goldcoin::refund::format_glc;

    println!(
        "Goldcoin-sourced ManualReview refund — request {} ({})",
        report.request_id,
        report
            .db
            .direction
            .map(|d| d.as_str())
            .unwrap_or("direction unknown")
    );
    println!("\n  REQUEST");
    println!(
        "    expected gross            = {} atomic ({} GLC)",
        report.expected_gross_atomic,
        format_glc(report.expected_gross_atomic)
    );
    match (report.source_txid, report.source_vout) {
        (Some(txid), Some(vout)) => {
            println!("    source transaction        = {}", hex::encode(&txid));
            println!("    source output             = {vout}");
        }
        _ => println!("    source transaction        = (none recorded)"),
    }

    // The mode banner is printed BEFORE the facts, and the two modes are
    // worded so they can never be skimmed as equivalent.
    println!("\n  AMOUNT WITNESS MODE:");
    match report.amount_witness_mode {
        Some(mode) => {
            println!("    {}", mode.describe());
            if mode.is_legacy() {
                println!(
                    "    ^ REDUCED ASSURANCE. This request was parked before the durable\n\
                     \x20     observed_amount_atomic witness existed, so the principal rests on\n\
                     \x20     the verified RPC read alone rather than on two independent\n\
                     \x20     observations. Every OTHER binding — outpoint, independently derived\n\
                     \x20     deposit script, confirmations, single input, prevout trace, no\n\
                     \x20     release, no prior refund — still applies in full. The amount is\n\
                     \x20     never parsed from the manual_review_note."
                );
            }
        }
        None => println!("    (not established — the trace did not complete)"),
    }
    println!(
        "\n  REQUEST-BOUND DEPOSIT SCRIPT (derived here, not read from the database)\n    {}",
        report.expected_deposit_script_hex
    );

    println!("\n  INDEPENDENTLY VERIFIED CHAIN FACTS");
    match report.derived.as_ref() {
        Some(d) => {
            println!(
                "    observed deposit          = {} atomic ({} GLC)",
                d.observed_amount_atomic,
                format_glc(d.observed_amount_atomic)
            );
            println!(
                "    confirmations             = {} (required {required_confirmations})",
                d.confirmations
            );
            println!(
                "    traced source input       = {}:{}",
                hex::encode(&d.source_input_txid),
                d.source_input_vout
            );
            println!("    derived refund address    = {}", d.refund_dest_address);
            println!(
                "    refund amount             = {} atomic ({} GLC)  [the FULL observed \
                 deposit; the vault pays the miner fee]",
                d.observed_amount_atomic,
                format_glc(d.observed_amount_atomic)
            );
        }
        None => println!("    (chain facts could not be established — see checks below)"),
    }

    println!("\n  SOLANA RELEASE WITNESS");
    println!("    {}", report.solana_check_detail);

    // The route-specific half of "no settlement has begun". Printed as
    // its own block, and always — an empty one is the affirmative
    // statement that nothing was found, which is exactly what an operator
    // authorizing a refund needs to read.
    println!("\n  ROBINHOOD PAYOUT WITNESS (durable ledger state)");
    if report.db.robinhood_payout_evidence.is_empty() {
        println!(
            "    no payout operation and no folded deposit observation names this request \
             — no Robinhood payout has begun"
        );
    } else {
        println!(
            "    REFUSING — {} blocker(s). A Goldcoin refund would return the deposit a \
             Robinhood payout is drawn against.",
            report.db.robinhood_payout_evidence.len()
        );
        for evidence in &report.db.robinhood_payout_evidence {
            println!("      [{}] {}", evidence.code(), evidence.reason());
        }
        println!(
            "    Inspect the operation with: glc-admin robinhood-tx-show --config PATH \
             --request-id {}",
            report.request_id
        );
    }

    println!("\n  EXISTING REFUND");
    match report.existing_refund.as_ref() {
        Some(r) => println!(
            "    state {}, txid {}",
            r.state.as_str(),
            r.txid
                .map(|t| hex::encode(&t))
                .unwrap_or_else(|| "-".into())
        ),
        None => println!("    none"),
    }

    println!("\n  VAULT / RESERVE EFFECT");
    match (report.plan.as_ref(), report.vault_outflow_atomic()) {
        (Some(plan), Some(outflow)) => {
            println!("    inputs selected           = {}", plan.inputs.len());
            println!(
                "    estimated miner fee       = {} atomic ({} GLC)",
                plan.fee_atomic,
                format_glc(plan.fee_atomic)
            );
            println!(
                "    change outputs            = {} totalling {} atomic",
                plan.change_outputs.len(),
                plan.total_change_atomic()
            );
            println!(
                "    total vault outflow       = {} atomic ({} GLC)  [refund + fee]",
                outflow,
                format_glc(outflow)
            );
        }
        _ => println!("    (no plan — the refund could not be constructed)"),
    }
    println!(
        "    GoldcoinReserve paused    = {}",
        report.goldcoin_reserve_paused
    );

    println!("\n  SAFETY CHECKS");
    for c in &report.checks {
        let mark = if c.passed { "PASS" } else { "FAIL" };
        if c.detail.is_empty() {
            println!("    [{mark}] {}", c.name);
        } else {
            println!("    [{mark}] {} — {}", c.name, c.detail);
        }
    }

    // The verdict distinguishes VERIFICATION from EXECUTABILITY. They are
    // different questions, and conflating them could read as "safe to
    // execute now" while the pause gate is still failing.
    println!("\n  VERDICT");
    if !report.would_refund {
        println!(
            "    VERIFICATION FAILED — at least one check above did not pass. No refund is \
             possible in this state, with or without a pause."
        );
    } else if report.goldcoin_reserve_paused {
        println!(
            "    VERIFICATION PASSED and the GoldcoinReserve is paused.\n    \
             Both the verification and the execute prerequisite are satisfied; --execute may \
             proceed. It will re-run every check server-side against fresh state before signing."
        );
    } else {
        println!(
            "    VERIFICATION PASSED, but the execute prerequisite is NOT met:\n      \
             GoldcoinReserve paused = FAIL\n    \
             --execute WILL REFUSE until the reserve is paused. This is not yet safe to \
             execute.\n    Next: pause, then RE-RUN THIS DRY RUN against the paused state, \
             and only then --execute:\n      \
             glc-admin pause --db PATH --direction goldcoin --note TEXT"
        );
    }
}

fn cmd_glc_refund_list(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::goldcoin::refund::format_glc;

    let db_path = require(args, "--db");
    let open_only = args.iter().any(|a| a == "--open-only");
    let ledger = Ledger::open(Path::new(db_path)).map_err(|e| e.to_string())?;
    let rows = ledger
        .list_goldcoin_refunds(open_only)
        .map_err(|e| e.to_string())?;

    if rows.is_empty() {
        println!(
            "no {}Goldcoin refunds recorded",
            if open_only { "open " } else { "" }
        );
        return Ok(());
    }
    println!(
        "{} {}Goldcoin refund(s):",
        rows.len(),
        if open_only { "open " } else { "" }
    );
    for r in rows {
        println!(
            "\n  request {}  [{}]{}",
            r.request_id,
            r.state.as_str(),
            if r.reservation_released {
                "  (SolanaReserve reservation released)"
            } else {
                ""
            }
        );
        println!(
            "    deposit      {}:{}  observed {} atomic ({} GLC)",
            hex::encode(&r.source_txid),
            r.source_vout,
            r.observed_amount_atomic,
            format_glc(r.observed_amount_atomic)
        );
        println!(
            "    refund       {} -> {} atomic ({} GLC), fee {} atomic",
            r.refund_dest_address,
            r.refund_amount_atomic,
            format_glc(r.refund_amount_atomic),
            r.fee_atomic
        );
        println!(
            "    traced input {}:{}",
            hex::encode(&r.source_input_txid),
            r.source_input_vout
        );
        if let Some(txid) = r.txid {
            println!(
                "    txid         {}  ({} confirmations, last observed)",
                hex::encode(&txid),
                r.confirmations
            );
        }
        println!("    note         {} (by {})", r.note, r.created_by);
        // The single most important line in this listing. `Broadcast` is
        // not "pending, maybe send it": a transaction paying real vault
        // funds ALREADY EXISTS and is named above. Operators reading a
        // long-open row have to be told that explicitly, or the obvious
        // reading — "this never went out" — leads to a second refund of
        // money that has already left the vault, unrecallably.
        match r.state {
            GoldcoinRefundState::Broadcast => println!(
                "    ACTION       none. A refund transaction ALREADY EXISTS for this request \
                 (txid above) and is authoritative.\n\
                 \x20                 The daemon checks its depth every tick and marks the \
                 request Refunded on its own once the\n\
                 \x20                 configured payout confirmation depth is reached. Do NOT \
                 send another refund. If this row\n\
                 \x20                 is not advancing, verify the txid on a Goldcoin node and \
                 check the daemon's logs for\n\
                 \x20                 'glc refund' — never by re-running refund-glc-manual-review \
                 against a different transaction."
            ),
            GoldcoinRefundState::Signed => println!(
                "    ACTION       a SIGNED transaction exists and may already be in a mempool. \
                 Re-running refund-glc-manual-review\n\
                 \x20                 --execute re-broadcasts those exact bytes (same inputs, \
                 same txid); it never builds a second one."
            ),
            GoldcoinRefundState::Built => println!(
                "    ACTION       inputs are reserved but nothing was signed or sent. \
                 refund-glc-manual-review --execute resumes\n\
                 \x20                 from signing, reusing the SAME reserved inputs."
            ),
            GoldcoinRefundState::Refunded => {}
        }
    }
    Ok(())
}

/// Calls the daemon's one fund-moving admin endpoint.
///
/// # What this sends, and what it deliberately cannot
///
/// The body is `{"note": "<operator note>"}` and the request id is in the
/// path. That is the entire input surface: there is no field for a
/// destination, an amount, a fee, a transaction, a signer or an override,
/// so nothing this CLI sends can influence where the money goes. The
/// daemon derives every such value itself and re-verifies it against the
/// chain immediately before signing.
///
/// # Credentials
///
/// The operator's bearer token is read from the environment variable
/// named by `service.admin_operators[].token_env` for the operator
/// identity in `GLC_ADMIN_OPERATOR` — never a CLI argument, never a
/// config value, and never printed. This mirrors `signing::remote`'s and
/// the daemon's own secret discipline.
async fn call_daemon_execute_glc_refund(
    config: &Config,
    request_id: i64,
    note: &str,
) -> Result<glc_reserve_bridge_service::admin_api::GlcRefundExecuteView, String> {
    let base = config.service.admin_bind_addr.ok_or_else(|| {
        "REFUSING — service.admin_bind_addr is not configured, so there is no daemon admin \
         endpoint to execute the refund through. Refund execution runs in the daemon (which \
         holds the vault signers); this CLI never signs."
            .to_string()
    })?;

    let operator = std::env::var("GLC_ADMIN_OPERATOR").map_err(|_| {
        "REFUSING — set GLC_ADMIN_OPERATOR to your operator name (as configured in \
         service.admin_operators) so the daemon can authenticate and audit this action"
            .to_string()
    })?;
    let op_config = config
        .service
        .admin_operators
        .iter()
        .find(|o| o.name == operator)
        .ok_or_else(|| {
            format!(
                "REFUSING — operator {operator:?} is not in service.admin_operators; this \
                 deployment does not know that identity"
            )
        })?;
    if !op_config.may_execute_glc_refunds {
        return Err(format!(
            "REFUSING — operator {operator:?} is not on the refund-execution allow-list \
             (may_execute_glc_refunds = false). Ordinary admin access does not permit moving \
             vault funds. The daemon enforces this too; this is the early, clearer refusal."
        ));
    }
    let token = std::env::var(&op_config.token_env).map_err(|_| {
        format!(
            "REFUSING — admin token env var {} is not set for operator {operator:?}",
            op_config.token_env
        )
    })?;
    if token.is_empty() {
        return Err(format!(
            "REFUSING — admin token env var {} is set but empty",
            op_config.token_env
        ));
    }

    let url = format!("http://{base}/refunds/glc/{request_id}/execute");
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("could not build the admin API client: {e}"))?;
    let response = client
        .post(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .json(&serde_json::json!({ "note": note }))
        .send()
        .await
        .map_err(|e| {
            // Never include the token or headers in an error.
            format!(
                "could not reach the bridge daemon's admin API at {base} ({e}). Refund \
                 execution runs in the daemon — is it running, and is admin_bind_addr \
                 reachable from here? Nothing was signed or broadcast."
            )
        })?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("could not read the daemon's response: {e}"))?;
    if !status.is_success() {
        let detail = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or(body);
        return Err(format!("daemon refused ({status}): {detail}"));
    }
    serde_json::from_str(&body).map_err(|e| format!("could not parse the daemon's response: {e}"))
}

fn print_glc_refund_execute_result(
    view: &glc_reserve_bridge_service::admin_api::GlcRefundExecuteView,
) {
    use glc_reserve_bridge_service::admin_api::GlcRefundAction;

    println!("\n  DAEMON RESULT (all values derived and verified server-side)");
    println!(
        "    AMOUNT WITNESS MODE       = {}",
        view.amount_witness_mode
    );
    if view.amount_witness_is_legacy {
        println!(
            "                                ^ REDUCED ASSURANCE — the principal rested on the\n\
             \x20                                 verified RPC read alone; every other binding\n\
             \x20                                 still applied in full."
        );
    }
    println!(
        "    derived deposit script    = {}",
        view.expected_deposit_script_hex
    );
    println!(
        "    action                    = {}",
        match view.action {
            GlcRefundAction::Broadcast => "BUILT, SIGNED and BROADCAST",
            GlcRefundAction::Rebroadcast =>
                "RE-BROADCAST the already-signed transaction (no new transaction was built)",
            GlcRefundAction::AlreadyBroadcast =>
                "ALREADY BROADCAST by an earlier invocation — a transaction paying this refund \
                 already exists (txid below) and no second one was built. The daemon marks the \
                 request Refunded on its own once that transaction reaches the configured payout \
                 confirmation depth; there is nothing to re-send",
            GlcRefundAction::AlreadyRefunded => "ALREADY REFUNDED — terminal, nothing to do",
        }
    );
    println!("    request                   = {}", view.request_id);
    println!("    refund lifecycle state    = {}", view.lifecycle_state);
    println!("    bridge request state      = {}", view.request_state);
    println!(
        "    source outpoint           = {}:{}",
        view.source_txid, view.source_vout
    );
    println!(
        "    observed deposit          = {} atomic ({} GLC)",
        view.observed_amount_atomic, view.observed_amount_glc
    );
    println!(
        "    refund destination        = {}",
        view.refund_destination
    );
    println!(
        "    refund principal          = {} atomic ({} GLC)",
        view.refund_principal_atomic, view.refund_principal_glc
    );
    println!(
        "    miner fee (vault-paid)    = {} atomic ({} GLC)",
        view.fee_atomic, view.fee_glc
    );
    match view.txid.as_deref() {
        Some(txid) => println!(
            "    refund txid               = {txid} ({} confirmations)",
            view.confirmations
        ),
        None => println!("    refund txid               = (not broadcast yet)"),
    }
    println!(
        "    audited as                = {} / {:?}",
        view.actor, view.note
    );

    println!("\n  SERVER-SIDE CHECKS (re-run immediately before signing)");
    for c in &view.checks {
        let mark = if c.passed { "PASS" } else { "FAIL" };
        if c.detail.is_empty() {
            println!("    [{mark}] {}", c.name);
        } else {
            println!("    [{mark}] {} — {}", c.name, c.detail);
        }
    }
    println!(
        "\n  Track it with: glc-admin glc-refund-list --db PATH --open-only\n  \
         It clears itself: the daemon marks the request Refunded once this transaction reaches \
         the configured\n  payout confirmation depth. Do not run this command again for this \
         request — the transaction above is\n  authoritative.\n  \
         Unpause when you are done: glc-admin unpause --db PATH --direction goldcoin --note TEXT"
    );
}

// =====================================================================
// Robinhood Network operator commands (Phase G)
// =====================================================================
//
// The recovery, inspection and preflight surface for the two EXECUTABLE
// Robinhood routes. Every command here follows the discipline the Solana
// and Goldcoin recovery commands already established:
//
//   - read-only by default, `--execute` for anything that writes;
//   - every safety check printed individually as PASS/FAIL;
//   - no flag anywhere that overrides a refused check;
//   - no `--destination`, no `--amount`, no `--nonce`, no force-complete.
//
// Nothing here enables a route. `robinhood-preflight` reads the
// contract's route flags and REPORTS them; there is deliberately no
// command in this binary that sets one.

/// Builds a read/call/submit-capable Robinhood RPC client from a config.
fn robinhood_rpc(
    config: &Config,
) -> Result<glc_reserve_bridge_service::robinhood::rpc::EvmRpcClient, String> {
    let indexer = config
        .robinhood_indexer
        .as_ref()
        .ok_or("this config has no [robinhood.indexer] section")?;
    glc_reserve_bridge_service::robinhood::rpc::EvmRpcClient::new(
        &glc_reserve_bridge_service::robinhood::rpc::EvmRpcConfig {
            url: indexer.rpc_url.clone(),
            connect_timeout_ms: indexer.request_timeout_ms,
            read_timeout_ms: indexer.request_timeout_ms,
        },
    )
    .map_err(|e| format!("could not construct the Robinhood RPC client: {e}"))
}

/// `request N` or `rebalance N` — a `TxView`'s subject, for one-line output.
fn tx_view_subject(tx: &glc_reserve_bridge_service::robinhood::admin::TxView) -> String {
    match (tx.request_id, tx.rebalance_request_id) {
        (Some(id), _) => format!("request {id}"),
        (None, Some(id)) => format!("rebalance {id}"),
        (None, None) => "(no subject)".to_string(),
    }
}

fn print_checks(checks: &[glc_reserve_bridge_service::robinhood::admin::Check]) {
    for check in checks {
        println!(
            "  [{}] {:<34} {}",
            if check.ok { "PASS" } else { "FAIL" },
            check.name,
            check.detail
        );
    }
}

/// `robinhood-status` — the one-screen picture of the Robinhood leg.
fn cmd_robinhood_status(args: &[String]) -> Result<(), String> {
    let ledger = open_ledger_arg(args)?;
    let halt = glc_reserve_bridge_service::robinhood::admin::halt_state(&ledger)
        .map_err(|e| e.to_string())?;

    println!("Robinhood indexer");
    match &halt.halt {
        Some(h) => println!(
            "  HALTED           {} at {} — {}",
            h.reason.as_str(),
            h.halted_at,
            h.detail
        ),
        None => println!("  halted           no"),
    }
    println!("  scan cursor      {:?}", halt.cursor_block);
    println!("  cursor hash      {:?}", halt.cursor_block_hash);
    println!("  retained anchors {}", halt.retained_anchors);
    println!(
        "  observations     provisional {} / final {} / reorged {} (highest final block {:?})",
        halt.observations.provisional,
        halt.observations.finalized,
        halt.observations.reorged,
        halt.observations.highest_finalized_block
    );
    println!("  folded final     {}", halt.folded_final_observations);

    let open = glc_reserve_bridge_service::robinhood::admin::open_operations(&ledger)
        .map_err(|e| e.to_string())?;
    let stalled = glc_reserve_bridge_service::robinhood::admin::stalled_operations(&ledger)
        .map_err(|e| e.to_string())?;
    println!("\nOperations");
    println!("  in flight        {}", open.len());
    for tx in &open {
        println!(
            "    #{} {:<16} {:<11} {} nonce {:?} sigs {}/{}",
            tx.id,
            tx.kind.as_str(),
            tx.state.as_str(),
            tx_view_subject(tx),
            tx.nonce,
            tx.signatures_collected,
            glc_reserve_bridge_service::robinhood::SIGNER_THRESHOLD,
        );
    }
    println!("  stalled          {}", stalled.len());
    for tx in &stalled {
        println!(
            "    #{} {:<16} {:<11} {} — {}",
            tx.id,
            tx.kind.as_str(),
            tx.state.as_str(),
            tx_view_subject(tx),
            tx.failure_reason
                .as_deref()
                .unwrap_or("(no reason recorded)")
        );
    }

    let queue = glc_reserve_bridge_service::robinhood::admin::manual_review_queue(&ledger)
        .map_err(|e| e.to_string())?;
    println!("\nManualReview (RhnToGlc)  {} request(s)", queue.len());
    for item in &queue {
        println!(
            "  request {} obligation {:?} gross {} — {}{}",
            item.request_id,
            item.obligation_index,
            item.gross_amount_atomic,
            item.reason.as_deref().unwrap_or("(no reason)"),
            match item.operation_state {
                Some(state) => format!(" [operation {}]", state.as_str()),
                None => String::new(),
            }
        );
    }

    match glc_reserve_bridge_service::robinhood::admin::reserve_report(&ledger, now_unix())
        .map_err(|e| e.to_string())?
    {
        None => println!(
            "\nRobinhood reserve  NOT CONFIGURED (no [reserve.robinhood] section — nothing can \
             be reserved against it)"
        ),
        Some(r) => {
            println!("\nRobinhood reserve (canonical 8dp, a THIRD independent reserve)");
            println!("  balance          {}", r.balance_atomic);
            println!("  protected min    {}", r.protected_minimum_atomic);
            println!("  reserved         {}", r.reserved_liquidity_atomic);
            println!("  pending outbound {}", r.pending_obligations_atomic);
            println!("  accrued fees     {}", r.accrued_fees_atomic);
            println!("  available        {}", r.available_capacity_atomic);
            println!("  invariant holds  {}", r.invariant_holds);
            println!("  paused           {}", r.paused);
            // Named, because reading this line as "the Robinhood leg is
            // paused" is exactly the misread that made a production
            // incident hard to diagnose: this reserve backs the OUTBOUND
            // GlcToRhn route only. RhnToGlc pays out of the Goldcoin
            // reserve and is gated by ITS pause and admission flags,
            // which `glc-admin status` prints and this command does not.
            println!(
                "  (backs GlcToRhn only — RhnToGlc admission is GoldcoinReserve's \
                 paused/admission_closed; see `glc-admin status`)"
            );
            // Which flag this line IS, and which four flags it is not.
            // A `paused=true` here has exactly one supported cause and
            // exactly one supported remedy; naming both stops the next
            // operator hunting through contract governance for a gate
            // that lives in this database.
            print_robinhood_local_pause_legend();
        }
    }
    Ok(())
}

/// The `paused` line's legend, printed by BOTH `robinhood-status` and
/// `robinhood-reserve` so the two can never explain the same flag
/// differently.
///
/// `RobinhoodReserve.paused` is a LOCAL, ledger-side gate on the
/// `GlcToRhn` route. It is a term of the same `InboundAdmissionGates`
/// evaluator `GET /chains` computes `available` from, so a `true` here
/// makes `GlcToRhn` unavailable on its own, with every other gate open.
/// It is emphatically not any of the four flags that share the word
/// "pause" or "enabled" on this leg, and the incident that motivated
/// this legend was an operator reading it as the contract's.
fn print_robinhood_local_pause_legend() {
    println!(
        "  paused = the LOCAL GlcToRhn reserve gate (reserve_ledger.paused, this database).\n  \
         Set it with: glc-admin robinhood-local-pause --db PATH --paused <true|false> --note TEXT\n  \
         It is SEPARATE from, and never reflects: the GlcRobinhoodBridge contract's\n    \
         depositsPaused/payoutsPaused (robinhood-governance-pause),\n    \
         the contract's routeEnabled flags (robinhood-governance-route, robinhood-preflight),\n    \
         ledger bridge_routes enablement (robinhood-route-enable, robinhood-routes),\n    \
         and the config file's own [routes] gate.\n  \
         It does NOT gate RhnToGlc: that route settles out of GoldcoinReserve, whose\n    \
         paused/admission_closed flags `glc-admin status` prints."
    );
}

/// Opens the ledger from `--db`, or from `--config`'s `service.db_path`.
fn open_ledger_arg(args: &[String]) -> Result<Ledger, String> {
    if let Some(db) = flag(args, "--db") {
        return Ledger::open(Path::new(db)).map_err(|e| e.to_string());
    }
    if let Some(config_path) = flag(args, "--config") {
        let config = Config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
        return Ledger::open(&config.service.db_path).map_err(|e| e.to_string());
    }
    Err("missing required --db (or --config)".to_string())
}

/// `robinhood-manual-review-list`
fn cmd_robinhood_manual_review_list(args: &[String]) -> Result<(), String> {
    let ledger = open_ledger_arg(args)?;
    let queue = glc_reserve_bridge_service::robinhood::admin::manual_review_queue(&ledger)
        .map_err(|e| e.to_string())?;
    if queue.is_empty() {
        println!("no Robinhood-sourced (RhnToGlc / RhnToSol) requests are in ManualReview");
        return Ok(());
    }
    for item in queue {
        println!("request {}", item.request_id);
        println!("  direction        {}", item.direction.as_str());
        println!("  obligation       {:?}", item.obligation_index);
        println!(
            "  reason           {}",
            item.reason.as_deref().unwrap_or("(none)")
        );
        println!("  gross (8dp)      {}", item.gross_amount_atomic);
        println!("  net   (8dp)      {}", item.net_amount_atomic);
        println!("  destination      {}", item.destination);
        println!("  created at       {}", item.created_at);
        println!("  refund begun     {}", item.has_refund);
        println!("  settlement begun {}", item.has_settlement);
        if let Some(state) = item.operation_state {
            println!("  operation state  {}", state.as_str());
        }
    }
    Ok(())
}

/// `robinhood-tx-show`
fn cmd_robinhood_tx_show(args: &[String]) -> Result<(), String> {
    let ledger = open_ledger_arg(args)?;
    let views = match flag(args, "--request-id") {
        Some(raw) => {
            let id: i64 = raw
                .parse()
                .map_err(|_| format!("--request-id {raw} is not an integer"))?;
            glc_reserve_bridge_service::robinhood::admin::txs_for_request(&ledger, id)
                .map_err(|e| e.to_string())?
        }
        None if args.iter().any(|a| a == "--stalled") => {
            glc_reserve_bridge_service::robinhood::admin::stalled_operations(&ledger)
                .map_err(|e| e.to_string())?
        }
        None => glc_reserve_bridge_service::robinhood::admin::open_operations(&ledger)
            .map_err(|e| e.to_string())?,
    };
    if views.is_empty() {
        println!("no matching Robinhood operations");
        return Ok(());
    }
    for tx in views {
        println!("operation #{} ({})", tx.id, tx.kind.as_str());
        println!("  state            {}", tx.state.as_str());
        println!("  subject          {}", tx_view_subject(&tx));
        println!(
            "  route            {}",
            tx.route
                .map(|r| r.as_str())
                .unwrap_or("(none — treasury withdrawal)")
        );
        println!("  chain id         {}", tx.chain_id);
        println!("  contract req id  {}", tx.contract_request_id);
        println!("  obligation       {:?}", tx.obligation_index);
        println!("  recipient        {:?}", tx.recipient);
        println!("  amount (18dp)    {:?}", tx.amount_robinhood_atomic);
        println!("  signer epoch     {}", tx.signer_epoch);
        println!("  expiry           {}", tx.expiry);
        println!("  auth digest      {}", tx.auth_digest);
        println!(
            "  signatures       {}/{} {:?}",
            tx.signatures_collected,
            glc_reserve_bridge_service::robinhood::SIGNER_THRESHOLD,
            tx.signers
        );
        println!("  submitter        {:?}", tx.submitter);
        println!("  nonce            {:?}", tx.nonce);
        println!("  raw tx persisted {}", tx.has_raw_tx);
        println!("  tx hash          {:?}", tx.tx_hash);
        println!("  gas limit        {:?}", tx.gas_limit);
        println!("  fees             {:?}", tx.fee_summary);
        println!(
            "  broadcasts       {} (replacements {})",
            tx.broadcast_attempts, tx.replacement_attempts
        );
        println!("  receipt status   {:?}", tx.receipt_status);
        println!("  receipt block    {:?}", tx.receipt_block_number);
        println!("  confirmations    {}", tx.confirmations);
        println!("  finalized at     {:?}", tx.finalized_at);
        println!("  failure reason   {:?}", tx.failure_reason);
        println!();
    }
    Ok(())
}

/// `robinhood-nonce-status`
fn cmd_robinhood_nonce_status(args: &[String]) -> Result<(), String> {
    let config = Config::load(Path::new(require(args, "--config"))).map_err(|e| e.to_string())?;
    let settlement = config
        .robinhood_settlement
        .as_ref()
        .ok_or("this config has no [robinhood.settlement] section")?;
    let ledger = Ledger::open(&config.service.db_path).map_err(|e| e.to_string())?;
    let state = glc_reserve_bridge_service::robinhood::admin::submitter_state(
        &ledger,
        settlement.submitter_address,
        settlement.chain_id.get(),
    )
    .map_err(|e| e.to_string())?;

    println!("submitter         {}", state.submitter);
    println!("chain id          {}", state.chain_id);
    println!("highest allocated {:?}", state.highest_allocated_nonce);
    println!(
        "observed pending  {:?} (recorded at {:?}) — a RECONCILIATION input and a floor, never \
         the allocator",
        state.observed_pending_nonce, state.observed_at
    );
    println!("in flight         {}", state.in_flight.len());
    for tx in &state.in_flight {
        println!(
            "  nonce {:?} operation #{} {} ({}) tx {:?}",
            tx.nonce,
            tx.id,
            tx.kind.as_str(),
            tx.state.as_str(),
            tx.tx_hash
        );
    }
    println!(
        "\nThis command READS. Nothing in this binary sets, resets, skips or reallocates a \
         nonce: the allocator is the ledger's own maximum inside the same write transaction \
         that stores it, and editing that by hand would reintroduce the duplicate-broadcast \
         window the design removes."
    );
    Ok(())
}

/// `robinhood-refund` — the production caller Phase F's `begin_refund`
/// was missing.
/// `robinhood-recover-deposit` — see the usage text and
/// `robinhood::recover_deposit`.
fn cmd_robinhood_recover_deposit(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::evm::EvmTxHash;
    use glc_reserve_bridge_service::robinhood::recover_deposit::{self, RecoverInputs};
    use glc_reserve_bridge_service::solana::accounts;
    use glc_reserve_bridge_service::solana::rpc::{RealSolanaRpc, SolanaRpc};

    let config = Config::load(Path::new(require(args, "--config"))).map_err(|e| e.to_string())?;
    let execute = args.iter().any(|a| a == "--execute");
    let txs: Vec<EvmTxHash> = {
        let mut out = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == "--tx" {
                let raw = it.next().ok_or("--tx needs a value")?;
                out.push(
                    raw.parse::<EvmTxHash>()
                        .map_err(|e| format!("--tx {raw:?} is not a transaction hash: {e}"))?,
                );
            }
        }
        if out.is_empty() {
            return Err("at least one --tx 0xHASH is required".to_string());
        }
        out
    };
    let indexer = config
        .robinhood_indexer
        .as_ref()
        .ok_or("this config has no [robinhood.indexer] section, so it names no contract")?
        .clone();

    println!(
        "Robinhood deposit recovery ({})",
        if execute { "EXECUTE" } else { "DRY RUN" }
    );
    println!("  ledger      {}", config.service.db_path.display());
    println!(
        "  contract    {} (chain {}, finality depth {})",
        indexer.bridge_contract.to_checksum_string(),
        indexer.chain_id.get(),
        indexer.confirmation_depth
    );

    let rpc = robinhood_rpc(&config)?;
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        // The reserve mint's decimals, read live exactly as the daemon
        // reads them for its own RhnToSol fold. Needed only for that
        // route; fetched once, up front, so a Solana outage refuses
        // before anything is written.
        let solana_decimals = {
            let sol = RealSolanaRpc::new(config.solana.rpc_url.clone());
            let account = sol
                .get_account(&accounts::bridge_config_pda())
                .await
                .map_err(|e| format!("reading the Solana bridge config: {e}"))?
                .ok_or("the Solana bridge config account does not exist")?;
            let snapshot = accounts::decode_bridge_config(&account.data).map_err(|e| e.to_string())?;
            accounts::fetch_reserve_mint_decimals(&sol, &snapshot.reserve_token_mint)
                .await
                .map_err(|e| format!("reading the reserve mint's decimals: {e}"))?
        };
        let mut ledger = Ledger::open(&config.service.db_path).map_err(|e| e.to_string())?;

        for tx in txs {
            println!("\n--- {tx}");
            let verified = recover_deposit::verify(&rpc, &indexer, tx)
                .await
                .map_err(|e| e.to_string())?;
            let o = &verified.observation;
            let fee_bps = config
                .route_fees
                .fee_bps(verified.route)
                .map_err(|e| format!("no fee configured for {}: {e}", verified.route.as_str()))?;
            println!("  obligation  #{} on {}", o.obligation_index, indexer.bridge_contract.to_checksum_string());
            println!("  route       {} (contract route id {})", verified.route.as_str(), verified.route.contract_route_id().unwrap_or(0));
            println!("  depositor   {}", verified.depositor.to_checksum_string());
            println!(
                "  amount      {} canonical 8dp = {}",
                o.amount_canonical_atomic,
                glc_reserve_bridge_service::chain_policy::human::format_glc(o.amount_canonical_atomic)
            );
            println!(
                "  destination {}",
                glc_reserve_bridge_service::robinhood::admin::render_destination(
                    verified
                        .route
                        .as_direction()
                        .ok_or("a contract route always has a direction")?,
                    &o.destination
                )
            );
            println!("  block       {} (head {}, hash canonical)  log index {}", o.block_number, verified.head, o.log_index);
            println!("  fee         {fee_bps} bps; fold with route CLOSED -> ManualReview, refundable");
            let existing = ledger
                .robinhood_observation_by_source(o.source_contract, o.obligation_index)
                .map_err(|e| e.to_string())?;
            println!(
                "  ledger      {}",
                match existing {
                    Some(row) => format!("observation already recorded (row {}, {})", row.id, row.finality.as_str()),
                    None => "no observation yet".to_string(),
                }
            );
            if !execute {
                continue;
            }
            let recovered = recover_deposit::recover(
                &rpc,
                &mut ledger,
                &indexer,
                tx,
                RecoverInputs {
                    fee_bps,
                    source_minimum: glc_reserve_bridge_service::min_transfer::SOURCE_MINIMUM_CANONICAL,
                    solana_decimals: Some(solana_decimals),
                    goldcoin_network: config.goldcoin.network,
                },
                now_unix(),
            )
            .await
            .map_err(|e| e.to_string())?;
            println!(
                "  RESULT      observation {:?}; request {} -> {:?}",
                recovered.observation,
                recovered.request_id(),
                recovered.fold
            );
        }
        if !execute {
            println!(
                "\nDRY RUN — nothing was written. Re-run with --execute to record the observation(s) \
                 and fold each into a ManualReview request. No payout, no refund, no cursor change."
            );
        }
        Ok(())
    })
}

fn cmd_robinhood_refund(args: &[String]) -> Result<(), String> {
    let config = Config::load(Path::new(require(args, "--config"))).map_err(|e| e.to_string())?;
    let note = require_note(args)?;
    let request_id: i64 = require(args, "--request-id")
        .parse()
        .map_err(|_| "--request-id must be an integer".to_string())?;
    let execute = args.iter().any(|a| a == "--execute");

    let mut ledger = Ledger::open(&config.service.db_path).map_err(|e| e.to_string())?;
    let assessment =
        glc_reserve_bridge_service::robinhood::admin::refund_assessment(&ledger, request_id)
            .map_err(|e| e.to_string())?;

    println!("Robinhood refund — request {request_id}");
    println!("  note: {note}");
    println!(
        "  obligation {:?}, ledger gross {} (canonical 8dp, CONTEXT ONLY)",
        assessment.obligation_index, assessment.gross_amount_atomic
    );
    println!("\nLedger-side checks:");
    print_checks(&assessment.checks);

    if let Some(existing) = &assessment.existing_refund {
        println!(
            "\nA refund operation already exists: #{} in state {}. Inspect it with \
             `robinhood-tx-show --request-id {request_id}` rather than beginning a second.",
            existing.id,
            existing.state.as_str()
        );
        return Ok(());
    }
    if !assessment.ledger_eligible {
        return Err("refused: at least one ledger-side check does not hold".to_string());
    }

    println!(
        "\nThe refund's RECIPIENT and AMOUNT are the obligation's own on-chain `depositor` and\n\
         `amount`, read from the contract at execution time. There is deliberately no\n\
         --destination and no --amount flag: neither is an operator's choice, and the contract\n\
         compares both exactly and reverts on any difference."
    );

    if !execute {
        println!(
            "\nDRY RUN — nothing was written, no signer was contacted, nothing was broadcast.\n\
             Re-run with --execute to begin the refund."
        );
        return Ok(());
    }

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let settler = build_robinhood_settler(&config).await?;
        let now = now_unix();
        // `begin_refund` re-runs every check against fresh state and
        // reads the obligation back from the chain. The assessment above
        // was a preview of this, never a precondition for it.
        let tx_id = glc_reserve_bridge_service::robinhood::begin_refund(
            &settler,
            &mut ledger,
            request_id,
            now,
        )
        .await
        .map_err(|e| format!("begin_refund refused: {e}"))?;

        println!("\nrefund authorization minted: operation #{tx_id}");

        // The settlement daemon only runs its broadcast phase while BOTH
        // executable routes are open, and a refund is exactly what an
        // operator does when they are not. So the broadcast and receipt
        // phases are driven here, and what they did is reported in full —
        // a refund that is authorized but never sent is not a refund.
        let mut report = glc_reserve_bridge_service::robinhood::SettlementReport::default();
        settler.tick_broadcast(&mut ledger, now, &mut report).await;
        settler.tick_receipts(&mut ledger, now, &mut report).await;
        println!(
            "broadcast phase: broadcast {} replaced {} included {} finalized {} reverted {} \
             manual_review {}",
            report.broadcast,
            report.replaced,
            report.included,
            report.finalized,
            report.reverted,
            report.manual_review
        );
        for error in &report.errors {
            println!("  error: {error}");
        }

        for tx in glc_reserve_bridge_service::robinhood::admin::txs_for_request(&ledger, request_id)
            .map_err(|e| e.to_string())?
        {
            println!(
                "operation #{} ({}) is now {} — tx {:?}, confirmations {}",
                tx.id,
                tx.kind.as_str(),
                tx.state.as_str(),
                tx.tx_hash,
                tx.confirmations
            );
        }
        println!(
            "\nRe-run this command to advance an unfinished refund: it is idempotent, resumes \
             the SAME operation under the SAME nonce, and can never produce a second transfer."
        );
        Ok::<(), String>(())
    })
}

/// `robinhood-treasury-withdraw` — see the usage text.
fn cmd_robinhood_treasury_withdraw(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::ledger::RobinhoodTxState;
    use glc_reserve_bridge_service::robinhood::treasury_withdraw::{
        self as tw, TreasuryWithdrawResult,
    };

    let config = Config::load(Path::new(require(args, "--config"))).map_err(|e| e.to_string())?;
    let note = require_note(args)?;
    let rebalance_id: i64 = require(args, "--rebalance-id")
        .parse()
        .map_err(|_| "--rebalance-id must be an integer".to_string())?;
    let execute = args.iter().any(|a| a == "--execute");
    let json = args.iter().any(|a| a == "--json");
    let wait_secs: i64 = match flag(args, "--wait-secs") {
        Some(raw) => raw
            .parse()
            .map_err(|_| "--wait-secs must be a non-negative integer".to_string())?,
        None => 600,
    };
    if flag(args, "--destination").is_some() || flag(args, "--amount").is_some() {
        return Err(
            "this command takes no --destination and no --amount: the destination is the \
             contract's immutable TREASURY, read live, and the amount is the approved rebalance \
             request's. See `glc-admin --help`."
                .to_string(),
        );
    }

    let mut ledger = Ledger::open(&config.service.db_path).map_err(|e| e.to_string())?;
    let indexer = config
        .robinhood_indexer
        .as_ref()
        .ok_or("this config has no [robinhood.indexer] section")?;
    let settlement = config
        .robinhood_settlement
        .as_ref()
        .ok_or("this config has no [robinhood.settlement] section")?;
    let required_confirmations = settlement.required_confirmations;
    let configured_signers = match config.operators.mode {
        glc_reserve_bridge_service::config::SignerMode::Dev => config
            .load_robinhood_dev_auth_signers()
            .map(|s| s.len())
            .unwrap_or(0),
        glc_reserve_bridge_service::config::SignerMode::Production => {
            config.robinhood_auth_remote_signers.len()
        }
    };

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        // ---- the read-only picture, for the dry run AND as the execute
        // path's first look ----
        let rpc = robinhood_rpc(&config)?;
        let existing = ledger
            .get_robinhood_tx_for_rebalance(rebalance_id)
            .map_err(|e| e.to_string())?;
        let onchain = tw::read_onchain(
            &rpc,
            indexer.bridge_contract,
            indexer.expected_token,
            existing.as_ref().map(|tx| tx.contract_request_id),
        )
        .await
        .map_err(|e| format!("reading the contract: {e}"))?;
        let assessment = tw::assess(
            &ledger,
            rebalance_id,
            Some(onchain),
            configured_signers,
            now_unix(),
        )
        .map_err(|e| e.to_string())?;

        let emit = |result: &TreasuryWithdrawResult| -> Result<(), String> {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(result).map_err(|e| e.to_string())?
                );
            }
            Ok(())
        };

        if !json {
            println!("Robinhood treasury withdrawal — rebalance #{rebalance_id}");
            println!("  note: {note}");
            println!(
                "  amount: {}  =  {} canonical (8dp)  =  {} Robinhood atomic (18dp)",
                glc_reserve_bridge_service::chain_policy::human::format_glc(
                    assessment.rebalance.amount_atomic
                ),
                assessment.rebalance.amount_atomic,
                assessment
                    .amount_robinhood
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "UNREPRESENTABLE".to_string())
            );
            if let Some(c) = &assessment.onchain {
                println!(
                    "  treasury (contract immutable): {}",
                    c.treasury.to_checksum_string()
                );
                println!(
                    "  contract: depositsPaused={} payoutsPaused={} migrated={} signerEpoch={}",
                    c.deposits_paused, c.payouts_paused, c.migrated, c.signer_epoch
                );
                println!(
                    "  contract reserve (18dp): balanceOf={} encumbered={} protectedMin={} \
                     post-withdraw={}",
                    c.reserve_balance,
                    c.encumbered,
                    c.protected_min_reserve,
                    assessment
                        .amount_robinhood
                        .map(|a| c.reserve_balance.saturating_sub(a.to_u256()).to_string())
                        .unwrap_or_else(|| "?".to_string())
                );
            }
            if let Some(r) = &assessment.ledger_reserve {
                println!(
                    "  ledger reserve (8dp): balance={} protected_min={} reserved={} pending={} \
                     available={} post-withdraw={} local_paused={}",
                    r.balance_atomic,
                    r.protected_minimum_atomic,
                    r.reserved_liquidity_atomic,
                    r.pending_obligations_atomic,
                    r.available_capacity_atomic,
                    i128::from(r.balance_atomic) - i128::from(assessment.rebalance.amount_atomic),
                    r.paused
                );
            }
            println!("\nChecks:");
            print_checks(&assessment.checks);
        }

        if let Some(tx) = &assessment.existing {
            if tx.state.is_terminal() && !execute {
                let result = TreasuryWithdrawResult::build(
                    &assessment,
                    Some(tx),
                    assessment.rebalance.state,
                    required_confirmations,
                    true,
                    Vec::new(),
                );
                emit(&result)?;
                if !json {
                    println!(
                        "\nOperation #{} already reached {} — nothing to do. Inspect with \
                         robinhood-treasury-withdraw-status.",
                        tx.id,
                        tx.state.as_str()
                    );
                }
                return if result.success {
                    Ok(())
                } else {
                    Err(format!(
                        "operation #{} ended in {} (rebalance {})",
                        tx.id,
                        tx.state.as_str(),
                        result.rebalance_state
                    ))
                };
            }
        }

        if !execute {
            let result = TreasuryWithdrawResult::build(
                &assessment,
                assessment.existing.as_ref(),
                assessment.rebalance.state,
                required_confirmations,
                true,
                Vec::new(),
            );
            emit(&result)?;
            if !json {
                println!(
                    "\nDRY RUN — nothing was written, no signer was contacted, nothing was \
                     signed, nothing was broadcast, no nonce was consumed.{}",
                    if assessment.eligible() {
                        " Every check holds. Re-run with --execute to withdraw."
                    } else {
                        " At least one check FAILS; --execute would refuse."
                    }
                );
            }
            return if assessment.eligible() || assessment.existing.is_some() {
                Ok(())
            } else {
                Err("dry run: at least one check does not hold".to_string())
            };
        }

        // ---- execute ----
        let settler = build_robinhood_settler(&config).await?;
        let tx_id = tw::begin(&settler, &mut ledger, rebalance_id, now_unix())
            .await
            .map_err(|e| format!("refused: {e}"))?;
        if !json {
            println!("\nauthorized: operation #{tx_id} (2-of-3 quorum stored)");
        }
        let deadline = now_unix() + wait_secs;
        let outcome = tw::drive(&settler, &mut ledger, tx_id, now_unix, deadline, || {
            std::thread::sleep(Duration::from_secs(2));
        })
        .await
        .map_err(|e| e.to_string())?;

        let rebalance_state = ledger
            .get_rebalance(rebalance_id)
            .map_err(|e| e.to_string())?
            .map(|r| r.state)
            .unwrap_or(assessment.rebalance.state);
        let result = TreasuryWithdrawResult::build(
            &assessment,
            Some(&outcome.tx),
            rebalance_state,
            required_confirmations,
            false,
            outcome.report.errors.clone(),
        );
        emit(&result)?;
        if !json {
            println!(
                "operation #{} is {} — tx {:?}, nonce {:?}, receipt_status {:?}, confirmations \
                 {}/{}",
                outcome.tx.id,
                outcome.tx.state.as_str(),
                result.tx_hash,
                outcome.tx.nonce,
                outcome.tx.receipt_status,
                outcome.tx.confirmations,
                required_confirmations
            );
            for error in &outcome.report.errors {
                println!("  error: {error}");
            }
        }
        match outcome.tx.state {
            RobinhoodTxState::Finalized if result.success => {
                if !json {
                    println!("COMPLETE — the withdrawal is mined, successful and at depth.");
                }
                Ok(())
            }
            RobinhoodTxState::Finalized => Err(format!(
                "operation #{} reads Finalized but rebalance #{rebalance_id} is {} — the receipt \
                 succeeded but the operation's effect could not be verified (see the errors \
                 above). NOT complete; do not re-authorize until this is understood.",
                outcome.tx.id, result.rebalance_state
            )),
            state => Err(format!(
                "operation #{} is {} — NOT complete. {}",
                outcome.tx.id,
                state.as_str(),
                if state.is_terminal() {
                    "This is terminal; see robinhood-treasury-withdraw-status and the rebalance's \
                     failure_reason."
                } else {
                    "Re-run this command to resume the SAME operation under the SAME nonce."
                }
            )),
        }
    })
}

/// `robinhood-treasury-withdraw-status` — read-only.
fn cmd_robinhood_treasury_withdraw_status(args: &[String]) -> Result<(), String> {
    let ledger = open_ledger_arg(args)?;
    let json = args.iter().any(|a| a == "--json");
    let views = glc_reserve_bridge_service::robinhood::admin::treasury_withdrawal_views(
        &ledger,
        flag(args, "--rebalance-id")
            .map(|r| {
                r.parse::<i64>()
                    .map_err(|_| "--rebalance-id must be an integer")
            })
            .transpose()?,
        flag(args, "--operation-id")
            .map(|r| {
                r.parse::<i64>()
                    .map_err(|_| "--operation-id must be an integer")
            })
            .transpose()?,
    )
    .map_err(|e| e.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&views).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if views.is_empty() {
        println!("no treasury-withdrawal operations");
    }
    for v in views {
        println!(
            "operation #{} rebalance #{} {} | amount {} (18dp) = {} canonical | treasury {} | \
             nonce {:?} | tx {:?} | receipt_status {:?} | confirmations {} | rebalance_state {} | \
             failure {:?}",
            v.operation_id,
            v.rebalance_id,
            v.state,
            v.amount_atomic,
            v.amount_canonical_atomic,
            v.destination,
            v.nonce,
            v.tx_hash,
            v.receipt_status,
            v.confirmations,
            v.rebalance_state,
            v.failure_reason
        );
    }
    Ok(())
}

/// Builds a `Settler` from a config: preflight, submitter, signers.
///
/// Runs the full startup preflight first, deliberately: a `Settler` can
/// only be built from a `VerifiedDeployment`, so an operator command
/// cannot act against a deployment whose contracts were never checked.
async fn build_robinhood_settler(
    config: &Config,
) -> Result<
    glc_reserve_bridge_service::robinhood::Settler<
        glc_reserve_bridge_service::robinhood::rpc::EvmRpcClient,
    >,
    String,
> {
    let indexer = config
        .robinhood_indexer
        .as_ref()
        .ok_or("this config has no [robinhood.indexer] section")?;
    let settlement = config
        .robinhood_settlement
        .as_ref()
        .ok_or("this config has no [robinhood.settlement] section")?;
    let rpc = robinhood_rpc(config)?;
    let deployment =
        glc_reserve_bridge_service::robinhood::preflight::verify(&rpc, indexer, settlement)
            .await
            .map_err(|e| format!("Robinhood preflight failed: {e}"))?;
    let submitter = glc_reserve_bridge_service::robinhood::Submitter::load(settlement)
        .map_err(|e| format!("could not load the Robinhood submitter key: {e}"))?;
    let signers = config
        .load_robinhood_auth_signers()
        .await
        .map_err(|e| format!("could not load the Robinhood authorization signers: {e}"))?;
    if signers.len() < glc_reserve_bridge_service::robinhood::SIGNER_THRESHOLD {
        return Err(format!(
            "only {} authorization signer(s) are available and a quorum requires {} — no \
             Robinhood operation can be authorized",
            signers.len(),
            glc_reserve_bridge_service::robinhood::SIGNER_THRESHOLD
        ));
    }
    let rpc = robinhood_rpc(config)?;
    Ok(glc_reserve_bridge_service::robinhood::Settler::new(
        rpc,
        submitter,
        signers,
        deployment,
        settlement.clone(),
        Duration::from_millis(config.service.signer_timeout_ms),
        config.goldcoin.network,
        config.goldcoin.required_payout_confirmations,
        config
            .chain_policies
            .fee_bps_for(glc_reserve_bridge_service::routes::Chain::Robinhood),
    ))
}

/// `robinhood-clear-halt`
fn cmd_robinhood_clear_halt(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::robinhood::admin::HaltClearance;
    let note = require_note(args)?;
    let execute = args.iter().any(|a| a == "--execute");
    let expect_reason = match flag(args, "--expect-reason") {
        None => None,
        Some(raw) => Some(
            raw.parse::<glc_reserve_bridge_service::ledger::RobinhoodHaltReason>()
                .map_err(|e| format!("--expect-reason: {e}"))?,
        ),
    };
    let acknowledge_orphaned_finality = args.iter().any(|a| a == "--acknowledge-orphaned-finality");

    // A chain-id / wrong-contract halt is only clearable once the
    // endpoint has been RE-READ and found correct. That read happens
    // here, against the live chain, rather than being an assertion the
    // operator makes on the command line.
    let (mut ledger, endpoint_reverified) = match flag(args, "--config") {
        Some(config_path) => {
            let config = Config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
            let ledger = Ledger::open(&config.service.db_path).map_err(|e| e.to_string())?;
            let verified = match (&config.robinhood_indexer, &config.robinhood_settlement) {
                (Some(indexer), Some(settlement)) => {
                    let rpc = robinhood_rpc(&config)?;
                    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
                    match rt.block_on(glc_reserve_bridge_service::robinhood::preflight::verify(
                        &rpc, indexer, settlement,
                    )) {
                        Ok(_) => {
                            println!(
                                "endpoint re-verified: preflight against the configured \
                                 deployment passes right now"
                            );
                            true
                        }
                        Err(e) => {
                            println!("endpoint NOT re-verified: preflight still fails — {e}");
                            false
                        }
                    }
                }
                _ => {
                    println!(
                        "no [robinhood.settlement] section — the endpoint could not be \
                         re-verified from this config"
                    );
                    false
                }
            };
            (ledger, verified)
        }
        None => (open_ledger_arg(args)?, false),
    };

    let clearance = HaltClearance {
        expect_reason,
        acknowledge_orphaned_finality,
        endpoint_reverified,
    };
    let state = glc_reserve_bridge_service::robinhood::admin::halt_state(&ledger)
        .map_err(|e| e.to_string())?;
    println!("\nHalt state");
    match &state.halt {
        Some(h) => println!("  {} at {} — {}", h.reason.as_str(), h.halted_at, h.detail),
        None => println!("  not halted"),
    }
    println!(
        "  cursor {:?}, retained anchors {}, folded final observations {}",
        state.cursor_block, state.retained_anchors, state.folded_final_observations
    );
    println!("\nClearance checks:");
    let checks =
        glc_reserve_bridge_service::robinhood::admin::halt_clear_assessment(&ledger, &clearance)
            .map_err(|e| e.to_string())?;
    print_checks(&checks);

    if !execute {
        println!(
            "\nDRY RUN — the halt was NOT cleared. Re-run with --execute.\n\
             note: {note}"
        );
        return Ok(());
    }
    let applied = glc_reserve_bridge_service::robinhood::admin::clear_halt(
        &mut ledger,
        &clearance,
        now_unix(),
    )
    .map_err(|e| format!("refused: {e}"))?;
    print_checks(&applied);
    println!(
        "\nhalt cleared (note: {note}). The indexer resumes from its persisted cursor on its \
         next tick. If the underlying condition is still true it will halt again — clearing a \
         halt is not a fix for what caused it."
    );
    Ok(())
}

/// `robinhood-preflight`
fn cmd_robinhood_preflight(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::robinhood::preflight::{
        operator_preflight, ExpectedRoutes, OperatorPreflightInputs, Verdict,
    };
    let config = Config::load(Path::new(require(args, "--config"))).map_err(|e| e.to_string())?;
    let indexer = config
        .robinhood_indexer
        .as_ref()
        .ok_or("this config has no [robinhood.indexer] section")?;
    let settlement = config
        .robinhood_settlement
        .as_ref()
        .ok_or("this config has no [robinhood.settlement] section")?;

    // Default: every route expected CLOSED, which is how this ships.
    let mut expect_enabled = Vec::new();
    if let Some(list) = flag(args, "--expect-route-enabled") {
        for name in list.split(',').filter(|s| !s.trim().is_empty()) {
            expect_enabled.push(
                name.trim()
                    .parse::<glc_reserve_bridge_service::routes::Route>()
                    .map_err(|e| format!("--expect-route-enabled: {e}"))?,
            );
        }
    }

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let report = rt.block_on(async {
        let rpc = robinhood_rpc(&config)?;
        let signers = config
            .load_robinhood_auth_signers()
            .await
            .unwrap_or_default();
        Ok::<_, String>(
            operator_preflight(
                &rpc,
                &OperatorPreflightInputs {
                    indexer,
                    settlement,
                    expected_routes: ExpectedRoutes { expect_enabled },
                    signers_available: signers.len(),
                    signers_required: glc_reserve_bridge_service::robinhood::SIGNER_THRESHOLD,
                    policy: config
                        .chain_policies
                        .get(glc_reserve_bridge_service::routes::Chain::Robinhood),
                    route_fees: Some(&config.route_fees),
                },
            )
            .await,
        )
    })?;

    for check in &report.checks {
        println!(
            "[{:<10}] {:<28} {}",
            check.verdict.as_str(),
            check.name,
            check.detail
        );
    }
    let (pass, fail, unverified) = report.counts();
    println!("\n{pass} PASS, {fail} FAIL, {unverified} UNVERIFIED");
    println!(
        "\nUNVERIFIED is not PASS. The token security properties above are NOT established by \
         anything this command does — a successful `decimals()` read says nothing about a mint \
         authority, a blocklist, a transfer hook, a pause, or an upgradeable proxy. Those need \
         a separate mainnet token review against the token's SOURCE and governance."
    );
    if report.any_failed() {
        return Err(format!("{fail} preflight check(s) FAILED"));
    }
    let _ = Verdict::Pass;
    Ok(())
}

/// `route-admission-show` — the READ side of the ROUTE-SCOPED admission
/// gate (schema v25's `route_admission`).
///
/// STRICTLY READ-ONLY: opens the ledger, reads one table plus each
/// route's live admission verdict, and writes nothing. It contacts no
/// chain, loads no keypair and reads no secret. The verdict comes from
/// `Ledger::route_admission_blocker`, which reads the
/// confirmed-liquidity gate's PERSISTED state and never evaluates the
/// hysteresis rule, so listing can never move a gate.
///
/// # It resolves nothing
///
/// Same discipline as `robinhood-routes`: an absent `route_admission`
/// table is reported as an absent table, not as a set of defaults,
/// because "open" and "never recorded" have different remedies (write
/// the flag, versus run the v25 migration).
///
/// # It prints BOTH axes
///
/// The route's own gate and the reserve-wide `paused`/`admission_closed`
/// it is ANDed with, side by side. Printing only the route flag would
/// leave an operator unable to tell "I closed this route" from "the
/// whole reserve is stopped", which is precisely the confusion the
/// route-scoped axis was added to remove.
fn cmd_route_admission_show(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::routes::Route;

    let ledger = open_ledger_arg(args)?;
    let state = ledger.route_admission_rows().map_err(|e| e.to_string())?;

    // One evaluation per route, through the SAME shared evaluator both
    // folds and `GET /chains` use — so this listing can never claim a
    // route admits when a fold would park it.
    // The RESERVE half of the picture, which a route's own row does not
    // carry. Every field is `Result`, rendered inline rather than
    // propagated: an unconfigured destination reserve must not stop this
    // command from printing the route state it CAN read.
    //
    // That is not a cosmetic choice. `route-admission-show` is the
    // command an operator runs to find out what is closed and why, so it
    // has to work on a half-set-up ledger — which is exactly when the
    // reserve row may be missing — for the same reason
    // `robinhood-routes` resolves nothing and `status` prints "not
    // configured" per reserve instead of aborting.
    struct Verdict {
        blocker: Result<Option<String>, String>,
        reserve_paused: Result<bool, String>,
        reserve_admission_closed: Result<bool, String>,
    }
    let render = |r: &Result<bool, String>, yes: &'static str, no: &'static str| -> String {
        match r {
            Ok(true) => yes.to_string(),
            Ok(false) => no.to_string(),
            Err(e) => format!("unknown ({e})"),
        }
    };
    let verdict = |route: Route| -> Verdict {
        let Some(direction) = route.as_direction() else {
            // Unreachable for `ADMISSION_SETTABLE` (pinned by
            // `routes::tests::admission_settable_routes_all_have_a_direction`),
            // handled rather than unwrapped so a future route variant
            // cannot turn a listing into a panic.
            let reason = format!("route {} has no settlement direction", route.as_str());
            return Verdict {
                blocker: Err(reason.clone()),
                reserve_paused: Err(reason.clone()),
                reserve_admission_closed: Err(reason),
            };
        };
        let reserve = direction.destination_reserve();
        Verdict {
            blocker: ledger
                .route_admission_blocker(direction)
                .map(|b| b.map(|b| b.as_str().to_string()))
                .map_err(|e| e.to_string()),
            reserve_paused: ledger.is_paused(reserve).map_err(|e| e.to_string()),
            reserve_admission_closed: ledger
                .is_admission_closed(reserve)
                .map_err(|e| e.to_string()),
        }
    };

    if args.iter().any(|a| a == "--porcelain") {
        println!(
            "route_admission_table\t{}",
            if state.is_some() { "present" } else { "absent" }
        );
        for route in Route::ADMISSION_SETTABLE {
            let row = state.as_ref().and_then(|s| s.row(route));
            let v = verdict(route);
            println!(
                "route\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                route.as_str(),
                match row {
                    Some(r) =>
                        if r.admission_closed {
                            "closed"
                        } else {
                            "open"
                        },
                    None => "no-row",
                },
                row.map(|r| r.updated_at.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                render(&v.reserve_paused, "paused", "running"),
                render(&v.reserve_admission_closed, "closed", "open"),
                match &v.blocker {
                    Ok(None) => "-".to_string(),
                    Ok(Some(b)) => b.clone(),
                    Err(e) => format!("unknown({e})"),
                },
                row.and_then(|r| r.admission_closed_reason.as_deref())
                    .unwrap_or("-"),
            );
        }
        return Ok(());
    }

    if args.iter().any(|a| a == "--json") {
        let mut routes = Vec::new();
        for route in Route::ADMISSION_SETTABLE {
            let row = state.as_ref().and_then(|s| s.row(route));
            let v = verdict(route);
            // Every reserve-side field is `null` when it could not be
            // read, never a fabricated `false` — an unreadable gate must
            // not serialize as an open one.
            routes.push(serde_json::json!({
                "route": route.as_str(),
                "route_admission_closed": row.map(|r| r.admission_closed),
                "route_admission_closed_reason": row.and_then(|r| r.admission_closed_reason.clone()),
                "route_admission_updated_at": row.map(|r| r.updated_at),
                "reserve_paused": v.reserve_paused.as_ref().ok(),
                "reserve_admission_closed": v.reserve_admission_closed.as_ref().ok(),
                "admits_now": v.blocker.as_ref().ok().map(Option::is_none),
                "blocker": v.blocker.as_ref().ok().and_then(|b| b.clone()),
                "read_error": v.blocker.as_ref().err(),
            }));
        }
        println!(
            "{}",
            serde_json::json!({
                "route_admission_table": if state.is_some() { "present" } else { "absent" },
                "unknown_route_ids": state.as_ref().map(|s| s.unknown_route_ids.clone()),
                "routes": routes,
            })
        );
        return Ok(());
    }

    if state.is_none() {
        println!(
            "route_admission: NO TABLE — this ledger predates schema v25, so no route-scoped \
             admission state has ever been recorded in it and `route-admission-close` would \
             refuse.\n  Every route therefore behaves exactly as it did before v25: governed by \
             the reserve-wide pause and admission control alone.\n  Start the daemon (or any \
             binary of this version) against this ledger once to migrate.\n"
        );
    }
    println!("ROUTE-SCOPED ADMISSION (schema v25 `route_admission`)");
    println!(
        "  The route's own gate, ANDed with the reserve-wide gates beside it. A route admits a"
    );
    println!("  new deposit only when BOTH are open; neither can clear the other.\n");
    for route in Route::ADMISSION_SETTABLE {
        let row = state.as_ref().and_then(|s| s.row(route));
        let v = verdict(route);
        println!("{}", route.as_str());
        match row {
            Some(r) => {
                println!(
                    "  route admission        {}",
                    if r.admission_closed { "CLOSED" } else { "open" }
                );
                println!("  last written           {}", r.updated_at);
                if let Some(reason) = &r.admission_closed_reason {
                    println!("  closed reason          {reason}");
                }
            }
            None => {
                println!("  route admission        NO ROW (treated as open — see the note above)")
            }
        }
        println!(
            "  reserve-wide pause     {}",
            render(&v.reserve_paused, "PAUSED", "running")
        );
        println!(
            "  reserve-wide admission {}",
            render(&v.reserve_admission_closed, "CLOSED", "open")
        );
        match &v.blocker {
            Ok(None) => println!("  admits now             YES"),
            Ok(Some(b)) => println!("  admits now             no — blocked by {b}"),
            // Never rendered as YES: an unreadable gate is not an open
            // one, the same fail-closed posture `api::route_availability`
            // takes for the public signal.
            Err(e) => println!("  admits now             UNKNOWN — could not evaluate ({e})"),
        }
        println!();
    }
    if let Some(unknown) = state.as_ref().map(|s| &s.unknown_route_ids) {
        if !unknown.is_empty() {
            println!(
                "WARNING: route_admission holds {} row(s) this build does not model: {}",
                unknown.len(),
                unknown.join(", ")
            );
            println!(
                "  The table's CHECK makes such a row impossible for this binary to write, so it \
                 is hand-written or a downgrade artefact. Inspect it."
            );
        }
    }
    println!(
        "This is ONE gate of several. A route also needs its ENABLEMENT gate open (`glc-admin \
         robinhood-routes`), its reserve unpaused and solvent (`glc-admin status`), and — for \
         Robinhood routes — the custody contract's own flags (`glc-admin robinhood-preflight`)."
    );
    Ok(())
}

/// `route-admission-close` / `route-admission-open` — the WRITE side of
/// the route-scoped admission gate.
///
/// Deliberately `--db`-only: this writes one boolean into the ledger's
/// `route_admission` table. It reads no config, contacts no chain, loads
/// no keypair and touches no secret, so requiring a config file would
/// imply a reach this command does not have. Same shape as
/// `robinhood-route-enable`.
///
/// # Closing is always allowed; opening is guarded
///
/// Closing needs no safety check, exactly as `close-admission` needs
/// none: refusing to stop taking deposits is not a safety property.
///
/// Opening runs behind `admin_api::guard::open_route_admission_guarded`,
/// which applies the SAME three unconditional checks `open-admission`
/// does — the hard reserve invariant, the mature-UTXO floor and the
/// confirmed-liquidity buffer — against this route's own destination
/// reserve. Without that, closing the reserve-wide switch and opening a
/// route would be a way to admit onto a reserve `open-admission` would
/// have refused.
fn cmd_route_admission(args: &[String], closed: bool) -> Result<(), String> {
    let db = require(args, "--db");
    let route: glc_reserve_bridge_service::routes::Route = require(args, "--route")
        .parse()
        .map_err(|e| format!("--route: {e}"))?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    // Which routes carry a route-scoped gate, the open path's three
    // safety checks, and the audit row all live in the shared audited
    // implementation — one place, so the CLI cannot drift from the HTTP
    // surface on any of them, and a refusal is audited too.
    audited_set_route_admission(&mut ledger, route, closed, note, &cli_actor())
        .map_err(|e| e.to_string())?;
    println!(
        "route admission for {} set to closed={closed} (note: {note})",
        route.as_str()
    );
    if closed {
        println!(
            "NEW {} deposits will now park in ManualReview with \
             `route_admission_closed_at_fold`. Already-accepted obligations are unaffected and \
             keep processing. The other inbound-to-Goldcoin route is unchanged — check it with \
             `glc-admin route-admission-show --db PATH`.",
            route.as_str()
        );
    } else {
        println!(
            "This opens ONE gate. {} still needs its reserve unpaused and its reserve-wide \
             admission open before it admits anything — `glc-admin status` and `glc-admin \
             route-admission-show --db PATH` report both.",
            route.as_str()
        );
    }
    Ok(())
}

/// `robinhood-routes` — the READ side of the ledger route gate.
///
/// Added because there was no read-only way to see what
/// `robinhood-route-enable` had written. `robinhood-status` reports the
/// indexer, operations, the ManualReview queue and the reserve, and says
/// nothing about `bridge_routes`; the only alternative was inspecting the
/// table with `sqlite3`, which is a second reader of the schema living
/// outside this binary. `scripts/bridge-admin.sh` calls this instead.
///
/// STRICTLY READ-ONLY: opens the ledger, reads one table, and — with
/// `--config` — reads the config through the real parser. It writes
/// nothing, contacts no chain, loads no keypair and reads no secret.
///
/// # It resolves nothing
///
/// [`Ledger::route_enabled`] must resolve an absent row to
/// [`Route::default_enabled`], because the admission gate has to return a
/// verdict. For a DISPLAY that resolution is wrong: "disabled" and "never
/// recorded" have different remedies (write the flag, versus run the v24
/// migration). So an absent table is reported as an absent table, and an
/// absent row as an absent row, with the fallback named beside it rather
/// than substituted for it.
///
/// # The three service-side gates, in one place
///
/// With `--config` this reports all three legs of
/// [`crate::routes::RouteGate`]'s AND — config, ledger, adapter — because
/// the failure this whole area keeps producing is treating them as one
/// switch. The adapter leg is reported STATICALLY, and says so: for
/// `SolToRhn`/`RhnToSol` it is `Unavailable` unconditionally and no
/// deployment can change that, and for `GlcToRhn`/`RhnToGlc` it is
/// Operational only in a process whose startup preflight verified the
/// deployment — which is a property of the running daemon, not of any
/// file, and is therefore reported as "verified at daemon startup" rather
/// than guessed at here.
fn cmd_robinhood_routes(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::chains::robinhood::RobinhoodAdapter;
    use glc_reserve_bridge_service::routes::Route;

    let ledger = open_ledger_arg(args)?;
    // `--config` is optional here and only ADDS columns. `open_ledger_arg`
    // already accepts it as a way to locate the ledger, so a config that
    // parses is re-used rather than re-read differently.
    let config = match flag(args, "--config") {
        Some(path) => Some(Config::load(Path::new(path)).map_err(|e| e.to_string())?),
        None => None,
    };
    let state = ledger.route_ledger_rows().map_err(|e| e.to_string())?;

    // The adapter leg's static verdict, in the adapter's OWN words — never
    // a second copy of the reason string.
    let adapter_verdict = |route: Route| -> &'static str {
        match route {
            Route::SolToRhn | Route::RhnToSol => "unavailable-always",
            Route::GlcToRhn | Route::RhnToGlc => "verified-at-daemon-startup",
            Route::GlcToSol | Route::SolToGlc => "operational",
        }
    };

    if args.iter().any(|a| a == "--porcelain") {
        println!(
            "bridge_routes_table\t{}",
            if state.is_some() { "present" } else { "absent" }
        );
        if let Some(config) = &config {
            println!("config_gate\tavailable");
            println!("ledger_path\t{}", config.service.db_path.display());
        } else {
            println!("config_gate\tnot-read");
        }
        for route in Route::ALL {
            let row = state.as_ref().and_then(|s| s.row(route));
            println!(
                "route\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                route.as_str(),
                match row {
                    Some(r) =>
                        if r.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                    None => "no-row",
                },
                row.map(|r| r.updated_at.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                route.default_enabled(),
                route.is_operator_settable(),
                match config {
                    Some(ref c) => c.routes.enabled(route).to_string(),
                    None => "unknown".to_string(),
                },
                adapter_verdict(route),
            );
        }
        if let Some(state) = &state {
            for unknown in &state.unknown_route_ids {
                println!("unknown_route_id\t{unknown}");
            }
        }
        return Ok(());
    }

    if args.iter().any(|a| a == "--json") {
        let rows: Vec<serde_json::Value> = Route::ALL
            .iter()
            .map(|route| {
                let row = state.as_ref().and_then(|s| s.row(*route));
                serde_json::json!({
                    "route": route.as_str(),
                    "ledger_gate": match row {
                        Some(r) => serde_json::json!({
                            "recorded": true,
                            "enabled": r.enabled,
                            "updated_at": r.updated_at,
                            "disabled_reason": r.disabled_reason,
                        }),
                        None => serde_json::json!({
                            "recorded": false,
                            "fallback_enabled": route.default_enabled(),
                        }),
                    },
                    "config_gate": config
                        .as_ref()
                        .map(|c| serde_json::Value::Bool(c.routes.enabled(*route)))
                        .unwrap_or(serde_json::Value::Null),
                    "adapter_gate": adapter_verdict(*route),
                    "operator_settable": route.is_operator_settable(),
                    "uses_this_gate_as_its_control": !route.is_legacy(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "bridge_routes_table": if state.is_some() { "present" } else { "absent" },
                "routes": rows,
                "unknown_route_ids": state
                    .as_ref()
                    .map(|s| s.unknown_route_ids.clone())
                    .unwrap_or_default(),
            }))
            .map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    println!("Robinhood route state — the LEDGER gate (bridge_routes)\n");
    let Some(state) = state.as_ref() else {
        println!(
            "  NO bridge_routes TABLE. This ledger has not run schema v24, so no route state \
             has ever been recorded in it and `robinhood-route-enable` would refuse.\n\n\
             Start this version's daemon against it once to migrate — take a fresh backup with \
             scripts/backup-ledger.sh first, because a v24 database cannot be reopened by a \
             pre-v24 binary. Until then the gate resolves every route to its compiled-in \
             default: enabled for GlcToSol/SolToGlc, disabled for all four Robinhood routes."
        );
        return Ok(());
    };

    println!(
        "  {:<9}  {:<10}  {:<20}  {:<9}  {:<11}",
        "ROUTE", "LEDGER", "UPDATED_AT (unix)", "SETTABLE", "CONFIG GATE"
    );
    for route in Route::ALL {
        let row = state.row(route);
        let ledger_col = match row {
            Some(r) if r.enabled => "ENABLED".to_string(),
            Some(_) => "disabled".to_string(),
            None => format!("no row ({})", route.default_enabled()),
        };
        // Raw unix seconds, as `halted_at` and `resets_at` are printed
        // elsewhere in this binary — this crate carries no date
        // formatter, and inventing one here for one column would be a
        // second way of rendering a timestamp.
        let updated = match row {
            Some(r) => r.updated_at.to_string(),
            None => "-".to_string(),
        };
        let config_col = match &config {
            Some(c) => {
                if c.routes.enabled(route) {
                    "enabled".to_string()
                } else {
                    "disabled".to_string()
                }
            }
            None => "not read (--config)".to_string(),
        };
        println!(
            "  {:<9}  {:<10}  {:<20}  {:<9}  {}",
            route.as_str(),
            ledger_col,
            updated,
            route.is_operator_settable(),
            config_col,
        );
        if let Some(reason) = row.and_then(|r| r.disabled_reason.as_deref()) {
            println!("  {:<9}  reason: {reason}", "");
        }
    }

    for unknown in &state.unknown_route_ids {
        println!(
            "\n  WARNING: bridge_routes holds a row for {unknown:?}, which this build does not \
             model. It is not a route this binary can evaluate; nothing reads it."
        );
    }

    println!(
        "\nOperator-settable in this gate: GlcToRhn, RhnToGlc, SolToRhn, RhnToSol — and nothing \
         else."
    );
    println!(
        "  glc-admin robinhood-route-enable/-disable --db PATH --route \
         <GlcToRhn|RhnToGlc|SolToRhn|RhnToSol> --note TEXT"
    );

    println!("\nWHY THE LEGACY ROUTES ARE LISTED BUT NOT CONTROLLED HERE:");
    println!(
        "  GlcToSol and SolToGlc have a seeded row (both enabled), because the v24 migration \
         records\n  every route's state rather than only some. That row is NOT their control \
         and never becomes\n  one: `robinhood-route-enable` refuses them outright. Their \
         controls are the local ledger\n  pause (glc-admin pause/unpause --direction \
         <goldcoin|solana>) and admission control\n  (glc-admin close-admission/open-admission \
         --direction goldcoin). A second, divergent switch\n  here would be one no reserve \
         invariant or liquidity check knows about."
    );
    println!(
        "\n  SolToRhn and RhnToSol have a seeded row too, at disabled, so their off state is \
         RECORDED\n  rather than merely absent. Since Phase H both have settlement machinery \
         and are\n  operator-settable here like the Goldcoin<->Robinhood pair; they stay closed \
         until an\n  operator opens them at every gate."
    );

    println!("\nTHIS IS ONE GATE OF THREE, and none of them substitutes for another:");
    println!(
        "  1. CONFIG   [routes] in the bridge config file{}",
        if config.is_some() {
            " — shown above"
        } else {
            " — pass --config to read it"
        }
    );
    println!("  2. LEDGER   bridge_routes — shown above; this command's subject");
    println!(
        "  3. ADAPTER  chain-adapter capability, evaluated in the DAEMON's process:\n     \
         every Robinhood route is Operational only where the startup preflight verified the \
         deployment\n     (glc-admin robinhood-preflight --config PATH reads the same \
         contracts). A file cannot\n     answer this, so it is not guessed at here."
    );
    println!(
        "\n  The CONTRACT's own routeEnabled flag is a FOURTH, separate switch, on the other \
         side of\n  the bridge: read it with `glc-admin robinhood-preflight --config PATH`, set \
         it with\n  `glc-admin robinhood-governance-route` under a 2-of-3 quorum. A route \
         moves value only\n  when every one of them agrees, and the contract's pause flags, the \
         signer quorum, reserve\n  availability and the local pause are still evaluated on top."
    );
    let _ = RobinhoodAdapter::UNAVAILABLE_REASON;
    Ok(())
}

/// `robinhood-route-enable` / `robinhood-route-disable` — the LEDGER leg
/// of the route gate, and only that leg.
///
/// Deliberately `--db`-only: this writes one boolean into the ledger's
/// `bridge_routes` table. It reads no config, contacts no chain, loads no
/// keypair and touches no secret, so requiring a config file would imply
/// a reach this command does not have.
///
/// Enabling a route here does NOT open it. The service config's per-route
/// flag, both chain adapters' capability, the contract's own
/// `routeEnabled`/`depositsPaused`/`payoutsPaused`, preflight, the signer
/// quorum, reserve availability and the local pause are each evaluated
/// independently on every request, and none of them is touched here.
/// `robinhood-routes` shows this gate's recorded state afterwards, and
/// `robinhood-preflight` reads the contract's own flag; no single command
/// reports a "resolved verdict", because the adapter leg is decided in the
/// DAEMON's process and no CLI read can establish it.
fn cmd_robinhood_route(args: &[String], enabled: bool) -> Result<(), String> {
    let db = require(args, "--db");
    let route: glc_reserve_bridge_service::routes::Route = require(args, "--route")
        .parse()
        .map_err(|e| format!("--route: {e}"))?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    // Which routes an operator may switch, and the audit row, both live
    // in the shared audited implementation — one place, so the CLI cannot
    // drift from any other surface that ever grows this control.
    audited_set_route_enabled(&mut ledger, route, enabled, note, &cli_actor())
        .map_err(|e| e.to_string())?;
    println!(
        "ledger route state for {} set to enabled={enabled} (note: {note})",
        route.as_str()
    );
    println!(
        "This is ONE of three service-side gates. The route is open only if the service config, \
         both chain adapters and the contract's own flags all agree — run `glc-admin \
         robinhood-routes --config PATH` to read the config and ledger gates together, and \
         `glc-admin robinhood-preflight --config PATH` for the contract's own flags."
    );
    Ok(())
}

/// `robinhood-reserve`
/// `robinhood-local-pause` — the ONE operator control for
/// `reserve_ledger.paused` on the `RobinhoodReserve` row.
///
/// # Why this exists as its own command
///
/// The flag has always been read: it is a term of
/// `InboundAdmissionGates`, so it is a term of the `available` verdict
/// `GET /chains` publishes for `GlcToRhn`, and of every
/// `fold_robinhood_deposit`. Until this command it was not WRITABLE by
/// any operator surface — `pause`/`unpause` and the admin API's
/// `POST /pause` both parse `goldcoin|solana` and reject anything else,
/// so a `RobinhoodReserve` row left `paused=1` (by a migration seed, a
/// bootstrap, or a hand-written row) could close `GlcToRhn` with no
/// supported way to reopen it.
///
/// # Why one `--paused <true|false>` rather than a pause/unpause pair
///
/// It matches its nearest neighbour, `robinhood-governance-pause
/// --paused <true|false>`, and keeps "which pause am I setting?" a
/// property of the COMMAND NAME rather than of a flag value — the
/// distinction that matters most here, since this binary can set four
/// different things an operator might call "the Robinhood pause".
///
/// # Scope, printed as well as documented
///
/// The output names the affected scope and states that `RhnToGlc` is not
/// controlled by this flag, because reading "Robinhood reserve paused" as
/// "the Robinhood leg is paused" is the exact misread this reserve's own
/// `robinhood-status` line already carries a warning about.
fn cmd_robinhood_local_pause(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let paused = parse_bool_flag(args, "--paused")?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;

    // Through the shared audited implementation, so this leaves the same
    // `admin_audit_log` row shape (actor `cli:<user>`, action
    // `pause`/`unpause`, target `robinhood`) a Goldcoin or Solana pause
    // does — one audit trail across all three reserves — and so the
    // unpause guard cannot be bypassed by reaching for `Ledger` directly.
    // A refusal is audited too; it is reported here as the error it is.
    let receipt = audited_set_robinhood_local_pause(&mut ledger, paused, note, &cli_actor())
        .map_err(|e| e.to_string())?;

    println!("RobinhoodReserve local reserve gate");
    println!(
        "  before           {}",
        receipt.old_value.as_deref().unwrap_or("(unknown)")
    );
    println!(
        "  after            {}",
        receipt.new_value.as_deref().unwrap_or("(unknown)")
    );
    println!("  audit id         {}", receipt.audit_id);
    println!("  note             {note}");
    println!("\nAffected scope: GlcToRhn local reserve gate only.");
    println!(
        "RhnToGlc is NOT controlled by this flag — it settles out of the GOLDCOIN reserve, so \n\
         its local gate is GoldcoinReserve's paused/admission_closed (`glc-admin status`)."
    );
    println!(
        "Untouched by this command: the GlcRobinhoodBridge contract's depositsPaused/\n\
         payoutsPaused and routeEnabled flags, ledger bridge_routes enablement, route_admission,\n\
         reserve-wide Goldcoin admission, GoldcoinReserve/SolanaReserve pause, and config.toml."
    );
    println!(
        "Nothing is cached — the gate is re-read on every request — so no daemon restart is \n\
         needed. Read it back with `glc-admin robinhood-status --db PATH`."
    );
    Ok(())
}

/// `robinhood-reserve-init` — see the usage text and
/// `robinhood::reserve_init`.
fn cmd_robinhood_reserve_init(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::robinhood::reserve_init::{self, ReserveInitOutcome};

    let config = Config::load(Path::new(require(args, "--config"))).map_err(|e| e.to_string())?;
    let indexer = config
        .robinhood_indexer
        .as_ref()
        .ok_or("this config has no [robinhood.indexer] section, so it names no contract")?;
    let settlement = config.robinhood_settlement.as_ref().ok_or(
        "this config has no [robinhood.settlement] section, so the deployment cannot \
                be verified the way the daemon verifies it",
    )?;
    let db_path: std::path::PathBuf = match flag(args, "--db") {
        Some(explicit) => std::path::PathBuf::from(explicit),
        None => config.service.db_path.clone(),
    };

    println!("Robinhood reserve init");
    println!("  ledger      {}", db_path.display());
    println!(
        "  contract    {} (chain {})",
        indexer.bridge_contract.to_checksum_string(),
        indexer.chain_id.get()
    );
    println!(
        "  token       {} (expected by config)",
        indexer.expected_token.to_checksum_string()
    );

    let rpc = robinhood_rpc(&config)?;
    let mut ledger = Ledger::open(&db_path).map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let outcome = rt
        .block_on(reserve_init::run(
            &rpc,
            &mut ledger,
            indexer,
            settlement,
            config.reserve.robinhood,
            now_unix(),
        ))
        .map_err(|e| e.to_string())?;

    let report = outcome.report();
    let glc =
        glc_reserve_bridge_service::chain_policy::human::format_glc(report.balance_canonical.0);
    match &outcome {
        ReserveInitOutcome::Initialized(_) => println!("\nINITIALIZED."),
        ReserveInitOutcome::AlreadyInitialized(_) => {
            println!("\nALREADY INITIALIZED — the row already said exactly this; nothing written.")
        }
    }
    println!(
        "  balance             {glc}   = {} canonical 8dp   = {} (18dp, balanceOf on chain)",
        report.balance_canonical.0,
        report
            .observed_robinhood_atomic
            .try_to_u128()
            .map(|v| v.to_string())
            .unwrap_or_else(|_| report.observed_robinhood_atomic.to_word_hex())
    );
    println!(
        "  protected minimum   {} canonical",
        report.bounds.protected_minimum
    );
    println!(
        "  critical / warning / target   {} / {} / {} canonical",
        report.bounds.critical_reserve, report.bounds.warning_reserve, report.bounds.target_reserve
    );
    println!("  reserved liquidity  0\n  pending outbound    0\n  accrued fees        0");
    println!(
        "\nThe deployment was verified (chain id, contract code, protocol family, token, \
         decimals, signer set, EIP-712 domain) before the row was written. No daemon was \
         started, no signer was contacted, nothing was broadcast."
    );
    Ok(())
}

fn cmd_robinhood_reserve(args: &[String]) -> Result<(), String> {
    let config = Config::load(Path::new(require(args, "--config"))).map_err(|e| e.to_string())?;
    let ledger = Ledger::open(&config.service.db_path).map_err(|e| e.to_string())?;
    let Some(report) =
        glc_reserve_bridge_service::robinhood::admin::reserve_report(&ledger, now_unix())
            .map_err(|e| e.to_string())?
    else {
        println!(
            "the Robinhood reserve is NOT CONFIGURED (no [reserve.robinhood] section). An \
             unconfigured reserve has no reserve_ledger row at all, so nothing can be reserved \
             against it and no Robinhood settlement can pass admission."
        );
        return Ok(());
    };

    println!("Robinhood reserve — ledger (canonical 8dp)");
    println!("  balance             {}", report.balance_atomic);
    println!("  protected minimum   {}", report.protected_minimum_atomic);
    println!("  reserved liquidity  {}", report.reserved_liquidity_atomic);
    println!(
        "  pending outbound    {}",
        report.pending_obligations_atomic
    );
    println!("  accrued fees        {}", report.accrued_fees_atomic);
    println!("  available capacity  {}", report.available_capacity_atomic);
    println!("  invariant holds     {}", report.invariant_holds);
    println!("  paused              {}", report.paused);
    print_robinhood_local_pause_legend();
    println!(
        "\nAccounted SEPARATELY from the Goldcoin and Solana reserves and never netted against \
         either: they are different physical pools on different chains."
    );

    let settlement = match &config.robinhood_settlement {
        None => {
            println!("\n(no [robinhood.settlement] section — the on-chain half was not read)");
            return Ok(());
        }
        Some(s) => s,
    };
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        use glc_reserve_bridge_service::robinhood::calls::{BridgeReader, TokenReader};
        use glc_reserve_bridge_service::robinhood::rpc::EvmBlockTag;
        let rpc = robinhood_rpc(&config)?;
        let reader = BridgeReader::new(settlement.bridge_contract);
        let limits = reader
            .limits(&rpc, EvmBlockTag::Latest)
            .await
            .map_err(|e| format!("reading limits(): {e}"))?;
        let inbound = reader
            .inbound_window(&rpc, EvmBlockTag::Latest)
            .await
            .map_err(|e| format!("reading inboundWindow(): {e}"))?;
        let outbound = reader
            .outbound_window(&rpc, EvmBlockTag::Latest)
            .await
            .map_err(|e| format!("reading outboundWindow(): {e}"))?;
        let encumbered = reader
            .encumbered_reserve(&rpc, EvmBlockTag::Latest)
            .await
            .map_err(|e| format!("reading encumberedReserve(): {e}"))?;
        let indexer = config
            .robinhood_indexer
            .as_ref()
            .expect("checked by robinhood_rpc");
        let balance = TokenReader::new(indexer.expected_token)
            .balance_of(&rpc, settlement.bridge_contract, EvmBlockTag::Latest)
            .await
            .map_err(|e| format!("reading balanceOf(bridge): {e}"))?;
        let now = now_unix() as u64;

        println!("\nRobinhood reserve — on-chain (Robinhood native 18dp)");
        println!("  contract balance    {balance}");
        println!("  encumbered          {encumbered}");
        println!("  protected minimum   {}", limits.protected_min_reserve);
        println!(
            "\n  inbound  limit {} used {} remaining {} (bucket resets at {})",
            limits.inbound_rolling_limit,
            inbound.total,
            inbound.remaining(limits.inbound_rolling_limit, now),
            inbound.resets_at()
        );
        println!(
            "  outbound limit {} used {} remaining {} (bucket resets at {})",
            limits.outbound_rolling_limit,
            outbound.total,
            outbound.remaining(limits.outbound_rolling_limit, now),
            outbound.resets_at()
        );
        println!(
            "\nRolling limits are per DIRECTION, shared by both routes on that side, and the \
             bucket is FIXED rather than sliding: the whole limit returns at once when the \
             bucket expires, not gradually."
        );
        Ok::<(), String>(())
    })
}

// ===================================================================
// Chain policy
// ===================================================================
//
// One network's fee rate and transfer ceilings. Four commands, of which
// three cannot write anything at all and the fourth is a dry run unless
// told otherwise.
//
// The division of labour with `scripts/chain-policy.sh` is deliberate:
// everything that PARSES, VALIDATES, CONVERTS or WRITES lives here, in
// Rust, behind the same types and the same config parser the daemon
// uses. The script only draws menus and asks questions. A shell script
// that did its own arithmetic on a fee rate, or its own substitution on
// a config file, would be a second implementation of the rules — and the
// second implementation is always the one that is wrong.

/// Resolves a `--network` argument against the route registry, so the
/// set of accepted names is the set of real bridge networks and cannot
/// drift from it.
fn require_network(args: &[String]) -> Result<glc_reserve_bridge_service::routes::Chain, String> {
    let raw = require(args, "--network");
    let networks = glc_reserve_bridge_service::chain_policy::bridge_networks();
    networks
        .iter()
        .copied()
        .find(|c| c.as_str() == raw)
        .ok_or_else(|| {
            format!(
                "unsupported network {raw:?} — this bridge's networks are: {}",
                networks
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Reads one policy figure from either its exact flag or its
/// operator-friendly one, refusing both at once.
///
/// Refusing both is not pedantry: `--fee-bps 600 --fee-percent 3` is two
/// different intentions in one command line, and picking either would be
/// guessing which one the operator meant about money.
fn policy_figure<T>(
    args: &[String],
    exact_flag: &str,
    human_flag: &str,
    parse_human: impl Fn(
        &str,
    )
        -> Result<T, glc_reserve_bridge_service::chain_policy::human::HumanParseError>,
    from_exact: impl Fn(u64) -> T,
) -> Result<T, String> {
    match (flag(args, exact_flag), flag(args, human_flag)) {
        (Some(_), Some(_)) => Err(format!(
            "{exact_flag} and {human_flag} are two ways to say the same thing — pass exactly one"
        )),
        (None, None) => Err(format!("one of {exact_flag} or {human_flag} is required")),
        (Some(exact), None) => exact
            .parse::<u64>()
            .map(&from_exact)
            .map_err(|e| format!("{exact_flag} must be a non-negative integer: {e}")),
        (None, Some(human)) => parse_human(human).map_err(|e| format!("{human_flag}: {e}")),
    }
}

/// Builds the candidate policy named on the command line.
fn requested_policy(
    args: &[String],
    chain: glc_reserve_bridge_service::routes::Chain,
) -> Result<glc_reserve_bridge_service::chain_policy::ChainPolicy, String> {
    use glc_reserve_bridge_service::amount_conversion::CanonicalAtomic;
    use glc_reserve_bridge_service::chain_policy::{human, ChainPolicy};

    let fee_bps = policy_figure(
        args,
        "--fee-bps",
        "--fee-percent",
        human::parse_fee_percent,
        |v| v,
    )?;
    let per_transfer = policy_figure(
        args,
        "--per-transfer-limit",
        "--per-transfer-glc",
        human::parse_glc,
        CanonicalAtomic,
    )?;
    let rolling = policy_figure(
        args,
        "--rolling-daily-limit",
        "--rolling-glc",
        human::parse_glc,
        CanonicalAtomic,
    )?;

    ChainPolicy::new(chain, fee_bps, per_transfer, rolling).map_err(|e| e.to_string())
}

/// Renders one policy as the three lines an operator reads.
fn print_policy(policy: &glc_reserve_bridge_service::chain_policy::ChainPolicy) {
    use glc_reserve_bridge_service::chain_policy::human;
    println!(
        "  Fee:                 {:<22} ({} bps)",
        human::format_percent(policy.fee_bps()),
        policy.fee_bps()
    );
    println!(
        "  Per-transfer limit:  {:<22} ({} canonical 8dp)",
        human::format_glc(policy.per_transfer_limit().0),
        policy.per_transfer_limit().0
    );
    println!(
        "  24h rolling limit:   {:<22} ({} canonical 8dp, STRICT)",
        human::format_glc(policy.rolling_daily_limit().0),
        policy.rolling_daily_limit().0
    );
}

/// The fixed-bucket relationship, spelled out. Printed for every
/// Robinhood policy an operator looks at or proposes, because the number
/// that belongs on chain is NOT the number in the config file and that is
/// the single easiest thing to get wrong about this launch.
fn print_rolling_bucket_note(
    policy: &glc_reserve_bridge_service::chain_policy::ChainPolicy,
) -> Result<(), String> {
    use glc_reserve_bridge_service::chain_policy::human;
    use glc_reserve_bridge_service::robinhood::RobinhoodPolicyBinding;

    let binding = RobinhoodPolicyBinding::new(*policy).map_err(|e| e.to_string())?;
    let bucket = binding.expected_onchain_rolling_limit_canonical().0;
    println!();
    println!("  Rolling window (GlcRobinhoodBridge uses a FIXED bucket, not a sliding window;");
    println!("  a bucket refilling exactly at the boundary lets up to 2x the configured amount");
    println!("  move within one 86,400s span, so the on-chain number is HALF the policy):");
    println!(
        "    Requested strict 24h policy:       {}",
        human::format_glc(policy.rolling_daily_limit().0)
    );
    println!(
        "    Recommended on-chain bucket limit:  {}",
        human::format_glc(bucket)
    );
    println!(
        "      inboundRollingLimit  = outboundRollingLimit = {} (18dp)",
        binding.expected_onchain_rolling_limit().get()
    );
    println!(
        "      inboundMax           = outboundMax          = {} (18dp)",
        binding.per_transfer_limit().get()
    );
    println!(
        "  Installing that is a setLimits(...) governance action under a 2-of-3 signer quorum."
    );
    println!("  THIS TOOL DOES NOT SEND IT and holds no signer key.");
    Ok(())
}

/// `chain-policy-networks`
fn cmd_chain_policy_networks(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::chain_policy::{bridge_networks, governance};

    let networks = bridge_networks();
    // A deliberately dull, line-oriented format for scripts. `--json`'s
    // key ORDER is serde's business, not this command's, so a shell that
    // matched on it would break the first time a field was added; this
    // shape is a contract.
    if args.iter().any(|a| a == "--porcelain") {
        for chain in &networks {
            println!(
                "{}\t{}\t{}",
                chain.as_str(),
                if governance(*chain).configurable {
                    "configurable"
                } else {
                    "fixed"
                },
                chain.display_name()
            );
        }
        return Ok(());
    }
    if args.iter().any(|a| a == "--json") {
        let rows: Vec<serde_json::Value> = networks
            .iter()
            .map(|chain| {
                let g = governance(*chain);
                serde_json::json!({
                    "network": chain.as_str(),
                    "display_name": chain.display_name(),
                    "configurable": g.configurable,
                    "fee_governance": g.fee,
                    "limit_governance": g.limits,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "networks": rows }))
                .map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    println!("Bridge networks with a chain policy:\n");
    for (index, chain) in networks.iter().enumerate() {
        let g = governance(*chain);
        println!(
            "{}. {} ({}) — policy {}",
            index + 1,
            chain.display_name(),
            chain.as_str(),
            if g.configurable {
                "CONFIGURABLE in this config file"
            } else {
                "NOT configurable here"
            }
        );
    }
    println!(
        "\nDerived from the route registry (routes::Route::ALL), not from a separate list, so a \
         new route's network appears here automatically."
    );
    Ok(())
}

/// `chain-policy-show`
fn cmd_chain_policy_show(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::chain_policy::{governance, human};
    use glc_reserve_bridge_service::routes::Chain;

    let config = load_policy_config(Path::new(require(args, "--config")))?;
    let chain = require_network(args)?;
    let g = governance(chain);
    let configured = config.chain_policies.get(chain).copied();
    let json = args.iter().any(|a| a == "--json");

    let onchain = if chain == Chain::Robinhood && !args.iter().any(|a| a == "--no-onchain") {
        read_robinhood_onchain_limits(&config)
    } else {
        None
    };

    if args.iter().any(|a| a == "--porcelain") {
        println!("network\t{}", chain.as_str());
        println!("configurable\t{}", g.configurable);
        match &configured {
            Some(policy) => {
                println!("configured\ttrue");
                println!("fee_bps\t{}", policy.fee_bps());
                println!("per_transfer_limit\t{}", policy.per_transfer_limit().0);
                println!("rolling_daily_limit\t{}", policy.rolling_daily_limit().0);
            }
            None => println!("configured\tfalse"),
        }
        return Ok(());
    }
    if json {
        return print_chain_policy_json(chain, configured.as_ref(), onchain.as_ref());
    }

    println!("Goldcoin Bridge — Chain Policy");
    println!("Network: {} ({})\n", chain.display_name(), chain.as_str());

    println!("Backend configured policy:");
    match &configured {
        Some(policy) => print_policy(policy),
        None if chain == Chain::Solana => {
            println!(
                "  Fee:                 {:<22} ({} bps, compiled in)",
                human::format_percent(
                    glc_reserve_bridge_service::amount_conversion::BRIDGE_FEE_BPS
                ),
                glc_reserve_bridge_service::amount_conversion::BRIDGE_FEE_BPS
            );
            println!("  Per-transfer limit:  read from the Solana program's config account");
            println!("  24h rolling limit:   read from the Solana program's config account");
        }
        None => {
            println!(
                "  NONE — this config file has no [{}.policy] section. New requests price at the \
                 compiled-in {} bps and this service states no transfer limits of its own.",
                chain.as_str(),
                glc_reserve_bridge_service::amount_conversion::BRIDGE_FEE_BPS
            );
        }
    }

    // The fee in `[<chain>.policy]` no longer prices anything once the
    // config states `[fees]`. Saying so HERE is the difference between an
    // operator changing the right number and changing a number that is
    // now only a record of what they once intended.
    print_fee_authority_note(&config, chain);

    println!("\nHow this network's policy is governed:");
    println!("  Fee:    {}", g.fee);
    println!("  Limits: {}", g.limits);
    if !g.configurable {
        println!(
            "\n  NOT CHANGEABLE by this tool: {}. `chain-policy-apply --network {}` refuses.",
            g.why_not_configurable,
            chain.as_str()
        );
    }

    if let Some(policy) = &configured {
        if chain == Chain::Robinhood {
            print_rolling_bucket_note(policy)?;
        }
    }

    if chain == Chain::Robinhood {
        print_robinhood_onchain_section(&config, configured.as_ref(), onchain.as_ref(), args);
    }
    Ok(())
}

/// The deployed contract's `limits()`, or `None` when this deployment
/// cannot make the read.
///
/// Never invents a value: an absent read is reported as absent, with the
/// reason, and every comparison that depended on it is skipped rather
/// than assumed to pass.
fn read_robinhood_onchain_limits(
    config: &Config,
) -> Option<Result<glc_reserve_bridge_service::robinhood::calls::BridgeLimits, String>> {
    use glc_reserve_bridge_service::robinhood::calls::BridgeReader;
    use glc_reserve_bridge_service::robinhood::rpc::EvmBlockTag;

    let indexer = config.robinhood_indexer.as_ref()?;
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => return Some(Err(e.to_string())),
    };
    Some(rt.block_on(async {
        let rpc = robinhood_rpc(config)?;
        BridgeReader::new(indexer.bridge_contract)
            .limits(&rpc, EvmBlockTag::Latest)
            .await
            .map_err(|e| e.to_string())
    }))
}

fn print_robinhood_onchain_section(
    config: &Config,
    configured: Option<&glc_reserve_bridge_service::chain_policy::ChainPolicy>,
    onchain: Option<&Result<glc_reserve_bridge_service::robinhood::calls::BridgeLimits, String>>,
    args: &[String],
) {
    use glc_reserve_bridge_service::robinhood::RobinhoodPolicyBinding;

    println!("\nOn-chain enforcement (GlcRobinhoodBridge):");
    let Some(indexer) = config.robinhood_indexer.as_ref() else {
        println!(
            "  UNAVAILABLE — this config has no [robinhood.indexer] section, so there is no \
             contract address and no endpoint to read. The contract's limits are unknown to this \
             command; they are NOT assumed to match."
        );
        return;
    };
    println!(
        "  Contract: {}",
        indexer.bridge_contract.to_checksum_string()
    );
    println!("  Chain id: {}", indexer.chain_id.get());

    if args.iter().any(|a| a == "--no-onchain") {
        println!("  SKIPPED — --no-onchain was passed. No comparison was made.");
        return;
    }

    let limits = match onchain {
        None => {
            println!("  UNAVAILABLE — no RPC endpoint is configured.");
            return;
        }
        Some(Err(e)) => {
            println!("  UNAVAILABLE — the limits() read failed: {e}");
            println!("  This is NOT a pass. Nothing here has been compared.");
            return;
        }
        Some(Ok(limits)) => limits,
    };

    println!("  inboundMax           {} (18dp)", limits.inbound_max);
    println!("  outboundMax          {} (18dp)", limits.outbound_max);
    println!(
        "  inboundRollingLimit  {} (18dp)",
        limits.inbound_rolling_limit
    );
    println!(
        "  outboundRollingLimit {} (18dp)",
        limits.outbound_rolling_limit
    );
    println!(
        "  protectedMinReserve  {} (18dp)",
        limits.protected_min_reserve
    );

    let Some(policy) = configured else {
        println!(
            "\n  No backend policy is configured, so there is nothing to compare the contract \
             against. The contract's limits above are the only ones in force."
        );
        return;
    };
    match RobinhoodPolicyBinding::new(*policy) {
        Err(e) => println!("\n  Cannot compare: {e}"),
        Ok(binding) => {
            let mismatches = binding.compare(limits);
            if mismatches.is_empty() {
                println!(
                    "\n  MATCH — the deployed contract enforces exactly the configured policy."
                );
            } else {
                println!("\n  MISMATCH — {} disagreement(s):", mismatches.len());
                for m in &mismatches {
                    println!("    - {m}");
                }
                println!(
                    "\n  Reconciling these is a setLimits(...) governance action under a 2-of-3 \
                     quorum. This command has not sent, signed or prepared one."
                );
            }
        }
    }
}

fn print_chain_policy_json(
    chain: glc_reserve_bridge_service::routes::Chain,
    configured: Option<&glc_reserve_bridge_service::chain_policy::ChainPolicy>,
    onchain: Option<&Result<glc_reserve_bridge_service::robinhood::calls::BridgeLimits, String>>,
) -> Result<(), String> {
    use glc_reserve_bridge_service::chain_policy::{governance, human};
    use glc_reserve_bridge_service::robinhood::RobinhoodPolicyBinding;

    let g = governance(chain);
    let mut root = serde_json::json!({
        "network": chain.as_str(),
        "display_name": chain.display_name(),
        "configurable": g.configurable,
        "fee_governance": g.fee,
        "limit_governance": g.limits,
        "configured": serde_json::Value::Null,
    });
    if let Some(policy) = configured {
        root["configured"] = serde_json::json!({
            "fee_bps": policy.fee_bps(),
            "fee_percent": human::format_percent(policy.fee_bps()),
            "per_transfer_limit": policy.per_transfer_limit().0,
            "per_transfer_glc": human::format_glc(policy.per_transfer_limit().0),
            "rolling_daily_limit": policy.rolling_daily_limit().0,
            "rolling_daily_glc": human::format_glc(policy.rolling_daily_limit().0),
        });
        if let Ok(binding) = RobinhoodPolicyBinding::new(*policy) {
            root["recommended_onchain_rolling_bucket"] = serde_json::json!({
                "canonical": binding.expected_onchain_rolling_limit_canonical().0,
                "glc": human::format_glc(binding.expected_onchain_rolling_limit_canonical().0),
                "robinhood_atomic_18dp": binding.expected_onchain_rolling_limit().get().to_string(),
            });
        }
    }
    match onchain {
        None => root["onchain"] = serde_json::Value::Null,
        Some(Err(e)) => root["onchain"] = serde_json::json!({ "available": false, "error": e }),
        Some(Ok(limits)) => {
            let mismatches: Vec<String> = configured
                .and_then(|p| RobinhoodPolicyBinding::new(*p).ok())
                .map(|b| b.compare(limits).iter().map(|m| m.to_string()).collect())
                .unwrap_or_default();
            root["onchain"] = serde_json::json!({
                "available": true,
                "inbound_max": limits.inbound_max.to_string(),
                "outbound_max": limits.outbound_max.to_string(),
                "inbound_rolling_limit": limits.inbound_rolling_limit.to_string(),
                "outbound_rolling_limit": limits.outbound_rolling_limit.to_string(),
                "protected_min_reserve": limits.protected_min_reserve.to_string(),
                "mismatches": mismatches,
            });
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?
    );
    Ok(())
}

/// Reports which routes on `chain` actually price from `[fees]`, and
/// whether that disagrees with the chain policy's own `fee_bps`.
fn print_fee_authority_note(config: &Config, chain: glc_reserve_bridge_service::routes::Chain) {
    use glc_reserve_bridge_service::chain_policy::human;
    use glc_reserve_bridge_service::fees::executable_routes;

    let routes: Vec<_> = executable_routes()
        .filter(|route| route.source_chain() == chain || route.destination_chain() == chain)
        .collect();
    if routes.is_empty() {
        return;
    }

    println!("\nWhat each ROUTE on this network actually charges:");
    for route in &routes {
        match config.route_fees.fee_bps(*route) {
            Ok(bps) => println!(
                "  {:<9} {:<8} ({} bps)",
                route.as_str(),
                human::format_percent(bps),
                bps
            ),
            Err(e) => println!("  {:<9} UNPRICED — {e}", route.as_str()),
        }
    }

    if let Some(policy) = config.chain_policies.get(chain) {
        let stated = policy.fee_bps();
        let disagrees = routes
            .iter()
            .filter_map(|route| config.route_fees.fee_bps(*route).ok())
            .any(|effective| effective != stated);
        if disagrees {
            println!(
                "\n  NOTE — the fee shown under 'Backend configured policy' above is\n                   [{}.policy].fee_bps, and it is NOT what prices these routes. Fees are per\n                   ROUTE now ([fees]); change one with `glc-admin fees-set --route <ROUTE>`.\n                   Changing fee_bps here would move a number that no longer prices anything.",
                chain.as_str()
            );
        }
    }
}

/// `chain-policy-validate`
fn cmd_chain_policy_validate(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::chain_policy::governance;
    use glc_reserve_bridge_service::routes::Chain;

    // The config is loaded even though nothing is written: validating a
    // policy against a config file that does not itself load would be
    // answering a question about a file nobody can run.
    let path = require(args, "--config");
    load_policy_config(Path::new(path))?;
    let chain = require_network(args)?;
    let g = governance(chain);
    if !g.configurable {
        return Err(format!(
            "{} has no configurable policy — {}",
            chain.as_str(),
            g.why_not_configurable
        ));
    }

    let policy = requested_policy(args, chain)?;
    println!("VALID — this policy would be accepted by the config parser.\n");
    print_policy(&policy);
    if chain == Chain::Robinhood {
        print_rolling_bucket_note(&policy)?;
    }
    println!("\nNothing was written. `chain-policy-validate` cannot modify a file.");
    Ok(())
}

/// `chain-policy-apply`
fn cmd_chain_policy_apply(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::chain_policy::{edit, governance};
    use glc_reserve_bridge_service::routes::Chain;

    let note = require_note(args)?;
    let path = Path::new(require(args, "--config"));
    let chain = require_network(args)?;
    let g = governance(chain);
    if !g.configurable {
        return Err(format!(
            "{} has no configurable policy — {}. Nothing was written.",
            chain.as_str(),
            g.why_not_configurable
        ));
    }
    let execute = args.iter().any(|a| a == "--execute");
    let dry_run = args.iter().any(|a| a == "--dry-run");
    if execute && dry_run {
        return Err("--dry-run and --execute contradict each other — pass one".to_string());
    }

    // Classified BEFORE planning: `edit::plan` loads the existing file
    // through the real parser and would otherwise report a policy
    // fragment as "the EXISTING config file does not load", which is
    // true and says nothing about which file was wrong.
    load_policy_config(path)?;
    let after = requested_policy(args, chain)?;
    let plan = edit::plan(path, chain, after).map_err(|e| e.to_string())?;

    println!(
        "Chain policy change — {} ({})",
        chain.display_name(),
        chain.as_str()
    );
    println!("Config: {}", plan.path().display());
    println!("Note:   {note}\n");

    println!("BEFORE:");
    match plan.before() {
        Some(before) => print_policy(before),
        None => println!(
            "  NONE — this file has no [{}.policy] section yet.",
            chain.as_str()
        ),
    }
    println!("\nAFTER:");
    print_policy(plan.after());

    if plan.is_noop() {
        println!("\nNO CHANGE — the file already states exactly this policy.");
    }
    if chain == Chain::Robinhood {
        print_rolling_bucket_note(plan.after())?;
    }

    if !execute {
        println!(
            "\nDRY RUN — nothing was written. The candidate file was validated by the real \
             config parser and then removed. Re-run with --execute to install it."
        );
        plan.discard();
        return Ok(());
    }

    let report = edit::commit(plan, now_unix()).map_err(|e| e.to_string())?;
    println!("\nAPPLIED.");
    println!("  Backup:  {}", report.backup.display());
    println!("  Config:  {}", report.path.display());
    println!(
        "\nThe running daemon has NOT been restarted and has NOT reloaded anything — it still \
         holds the previous policy until an operator restarts it deliberately. No route was \
         enabled, no secret was read, and no on-chain transaction was signed or sent."
    );
    if chain == Chain::Robinhood {
        println!(
            "Run `glc-admin robinhood-preflight --config {}` to check the new backend policy \
             against the deployed contract before restarting anything.",
            report.path.display()
        );
    }
    Ok(())
}

// -------------------------------------------------------------------
// Is that even a config file?
// -------------------------------------------------------------------
//
// `--config` is a path an operator typed, and the most common wrong
// answer is a real, sensible-looking file that is not a bridge config:
// `docs/robinhood/launch-policy.toml.example`, which states the approved
// policy and nothing else. Handed to `Config::load` it produces
//
//     missing field `solana`
//
// which is true, unhelpful, and identical however many times it is
// retried. `chain_policy::inspect` classifies the file first so the
// answer can name what the file actually IS; these functions only
// render that answer.

/// Loads a config for a chain-policy command, replacing a bare parser
/// error with one that names the kind of file it was handed.
///
/// The parser still decides: this only reaches for the classifier once
/// `Config::load` has already refused.
fn load_policy_config(path: &Path) -> Result<Config, String> {
    Config::load(path).map_err(|e| {
        let kind = glc_reserve_bridge_service::chain_policy::inspect::inspect(path);
        if kind.is_usable() {
            // The classifier disagrees with the parser, which can only
            // happen if the file changed underneath us. Report the
            // parser.
            return e.to_string();
        }
        format!(
            "{} is not usable as a bridge config.\n\n{}\n{}\n\nUnderlying parser error: {e}",
            path.display(),
            kind.headline(),
            chain_policy_file_explanation(path, &kind)
        )
    })
}

/// The prose for one classification: what the file is, why it cannot be
/// used, and what to do instead.
fn chain_policy_file_explanation(
    path: &Path,
    kind: &glc_reserve_bridge_service::chain_policy::inspect::FileKind,
) -> String {
    use glc_reserve_bridge_service::chain_policy::inspect::{FileKind, REQUIRED_SECTIONS};

    let required = REQUIRED_SECTIONS.join(", ");
    match kind {
        FileKind::FullConfig => format!(
            "{} is the file the daemon loads. Every chain-policy command can act on it.",
            path.display()
        ),
        FileKind::PolicyFragment { policies, .. } => {
            let sections: Vec<String> = policies
                .iter()
                .map(|p| format!("[{}]", p.section()))
                .collect();
            format!(
                "It is valid TOML and it states {}, but a bridge config file must also carry the \
                 sections the parser requires — {} — and none of them is here.\n\n\
                 A fragment like this is DOCUMENTATION: the daemon never loads it, no policy is \
                 read from it at run time, and no command in this tool will edit it. Pointing \
                 --config at it cannot work, so nothing here pretends it did.\n\n\
                 Pass the FULL bridge config the daemon loads (typically \
                 /etc/glc-bridge/config.toml) instead. The policy stated below is what this \
                 fragment says; the flags underneath put exactly that into a real config file.",
                sections.join(" and "),
                required,
            )
        }
        FileKind::IncompleteConfig { missing_sections } => format!(
            "It is valid TOML, but the required section(s) {} are absent and it states no \
             [<chain>.policy] section either — so it is neither a bridge config nor a policy \
             fragment.\n\n\
             Pass the full bridge config the daemon loads (typically \
             /etc/glc-bridge/config.toml).",
            missing_sections.join(", "),
        ),
        FileKind::InvalidConfig { detail } => format!(
            "Every required section ({required}) is present, so this IS shaped like a config \
             file — the parser refuses it for another reason:\n\n  {detail}\n\n\
             Fix that first. No chain-policy command may act on a file the daemon itself could \
             not load: the policy it would report, and the policy it would write, are both \
             defined by that parser.",
        ),
        FileKind::NotToml { detail } => format!(
            "The TOML parser could not read it at all:\n\n  {detail}\n\n\
             Pass the full bridge config the daemon loads (typically \
             /etc/glc-bridge/config.toml).",
        ),
        FileKind::Missing => format!(
            "There is no file at {}. Pass the full bridge config the daemon loads (typically \
             /etc/glc-bridge/config.toml).",
            path.display()
        ),
        FileKind::Unreadable { detail } => format!(
            "{} exists but could not be read:\n\n  {detail}",
            path.display()
        ),
    }
}

// ===================================================================
// Per-route fees
// ===================================================================
//
// One rate per executable route, in the config's `[fees]` table. Two
// commands: one that reads, and one that changes exactly one entry.
//
// Everything that PARSES, VALIDATES, CONVERTS or WRITES lives in
// `crate::fees` and `crate::fees::edit`, behind the same config parser
// the daemon runs at startup. These functions render an answer and pass
// flags along; there is no fee arithmetic in this file.

/// Resolves `--route` against the executable routes, so the accepted set
/// is the set of routes that can actually be priced and cannot drift from
/// it.
fn require_fee_route(args: &[String]) -> Result<glc_reserve_bridge_service::routes::Route, String> {
    use glc_reserve_bridge_service::fees::executable_routes;
    use glc_reserve_bridge_service::routes::Route;

    let raw = require(args, "--route");
    let route: Route = raw.parse().map_err(|_| {
        format!(
            "--route {raw:?} is not a route this bridge models — expected one of: {}",
            executable_routes()
                .map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    if route.as_direction().is_none() {
        return Err(format!(
            "{} has no settlement machinery in this build, so it cannot be priced. Configuring a \
             fee for it would state a price for a path that cannot move value",
            route.as_str()
        ));
    }
    Ok(route)
}

/// Where one route's effective rate actually came from.
///
/// An operator reading `6%` beside `RhnToGlc` needs to know whether that
/// is a line in their file or a fallback they have never seen, because
/// only one of those survives the next edit unchanged.
fn fee_provenance(
    config_text: &str,
    route: glc_reserve_bridge_service::routes::Route,
) -> &'static str {
    let has_section = config_text
        .lines()
        .any(|line| line.trim_start().starts_with("[fees]"));
    if !has_section {
        return "migration fallback (no [fees] section in this file)";
    }
    let key = format!("{} ", route.as_str());
    if config_text
        .lines()
        .any(|line| line.trim_start().starts_with(&key))
    {
        "[fees] in this config file"
    } else {
        // `resolve_route_fees` refuses a partial table, so a loaded config
        // whose `[fees]` section omits a route cannot exist. Reported
        // rather than asserted: this is a display function.
        "[fees] (key not found — report this)"
    }
}

/// `fees-show`
fn cmd_fees_show(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::chain_policy::human;
    use glc_reserve_bridge_service::fees::executable_routes;
    use glc_reserve_bridge_service::routes::{Chain, Route};

    let path = Path::new(require(args, "--config"));
    let config = load_policy_config(path)?;
    let text = std::fs::read_to_string(path).unwrap_or_default();

    let only: Option<Route> = if flag(args, "--route").is_some() {
        Some(require_fee_route(args)?)
    } else {
        None
    };
    let routes: Vec<Route> = match only {
        Some(route) => vec![route],
        None => executable_routes().collect(),
    };

    // A Solana<->Robinhood route may legitimately have no rate in force
    // (it may go unpriced while disabled); it is reported as such, never
    // as an error and never as a number.
    let rate_of = |route: Route| -> Result<Option<u64>, String> {
        match config.route_fees.fee_bps(route) {
            Ok(bps) => Ok(Some(bps)),
            Err(glc_reserve_bridge_service::fees::FeeError::MissingFee { .. })
                if route.is_solana_robinhood() =>
            {
                Ok(None)
            }
            Err(e) => Err(e.to_string()),
        }
    };

    if args.iter().any(|a| a == "--porcelain") {
        for route in &routes {
            match rate_of(*route)? {
                Some(bps) => println!(
                    "fee\t{}\t{}\t{}",
                    route.as_str(),
                    bps,
                    human::format_percent(bps)
                ),
                None => println!("fee\t{}\tunpriced\t-", route.as_str()),
            }
        }
        return Ok(());
    }
    if args.iter().any(|a| a == "--json") {
        let mut rows: Vec<serde_json::Value> = Vec::with_capacity(routes.len());
        for route in &routes {
            rows.push(match rate_of(*route)? {
                Some(bps) => serde_json::json!({
                    "route": route.as_str(),
                    "fee_bps": bps,
                    "fee_percent": human::format_percent(bps),
                    "provenance": fee_provenance(&text, *route),
                }),
                None => serde_json::json!({
                    "route": route.as_str(),
                    "fee_bps": serde_json::Value::Null,
                    "fee_percent": serde_json::Value::Null,
                    "provenance": "unpriced (no [fees] entry; the route is disabled)",
                }),
            });
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "fees": rows }))
                .map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    println!("Goldcoin Bridge — per-route fees");
    println!("Config: {}\n", path.display());
    println!(
        "  {:<9}  {:<8}  {:<7}  {:<24}",
        "ROUTE", "FEE", "BPS", "FROM"
    );
    let mut unpriced = Vec::new();
    for route in &routes {
        match rate_of(*route)? {
            Some(bps) => println!(
                "  {:<9}  {:<8}  {:<7}  {}",
                route.as_str(),
                human::format_percent(bps),
                bps,
                fee_provenance(&text, *route)
            ),
            None => {
                unpriced.push(route.as_str());
                println!(
                    "  {:<9}  {:<8}  {:<7}  no [fees] entry — the route is disabled and folds \
                     nothing",
                    route.as_str(),
                    "UNPRICED",
                    "-",
                );
            }
        }
    }

    println!(
        "\nEvery rate above is resolved BY ROUTE. There is no global fee and no per-chain\n\
         default behind them: a route with no rate is a startup error, never another\n\
         route's number."
    );

    if !unpriced.is_empty() {
        println!(
            "\n{} unpriced: a Solana<->Robinhood route may go unpriced only while it is\n\
             disabled in [robinhood]. Enabling one requires an explicit `[fees]` entry\n\
             (`fees-set --route <route>`); there is no pre-existing rate to carry forward.",
            unpriced.join(" and ")
        );
    }

    // Two numbers for the same thing is how the wrong one gets read.
    if let Some(policy) = config.chain_policies.get(Chain::Robinhood) {
        let stated = policy.fee_bps();
        let mut disagreements = Vec::new();
        for route in [Route::GlcToRhn, Route::RhnToGlc] {
            if let Ok(effective) = config.route_fees.fee_bps(route) {
                if effective != stated {
                    disagreements.push((route, effective));
                }
            }
        }
        if !disagreements.is_empty() {
            println!(
                "\nNOTE — [robinhood.policy].fee_bps still states {} ({}). That value no longer\n\
                 prices anything: `[fees]` is authoritative. It is left alone because it is the\n\
                 operator's own record, and because this command changes exactly what it was\n\
                 asked to. Disagreements:",
                stated,
                human::format_percent(stated)
            );
            for (route, effective) in disagreements {
                println!(
                    "    {:<9} prices at {} — [robinhood.policy] says {}",
                    route.as_str(),
                    human::format_percent(effective),
                    human::format_percent(stated)
                );
            }
        }
    }

    println!(
        "\nThe deployed GlcRobinhoodBridge contract stores NO fee — its Limits struct carries\n\
         minimums, maximums, rolling limits and a protected minimum, and nothing else — so a\n\
         fee change needs no governance transaction and no on-chain reconciliation."
    );
    println!("\nNothing was written. `fees-show` cannot modify a file.");
    Ok(())
}

/// `fees-set`
fn cmd_fees_set(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::chain_policy::human;
    use glc_reserve_bridge_service::fees::edit;

    let note = require_note(args)?;
    let path = Path::new(require(args, "--config"));
    let route = require_fee_route(args)?;
    let execute = args.iter().any(|a| a == "--execute");
    let dry_run = args.iter().any(|a| a == "--dry-run");
    if execute && dry_run {
        return Err("--dry-run and --execute contradict each other — pass one".to_string());
    }

    // Classified BEFORE planning, so a policy fragment is reported as one
    // rather than as "the EXISTING config file does not load".
    load_policy_config(path)?;

    let fee_bps = policy_figure(
        args,
        "--fee-bps",
        "--fee-percent",
        human::parse_fee_percent,
        |v| v,
    )?;

    let plan = edit::plan(path, route, fee_bps).map_err(|e| e.to_string())?;

    println!("Per-route fee change — {}", route.as_str());
    println!("Config: {}", plan.path().display());
    println!("Note:   {note}\n");
    match plan.before() {
        Some(before) => println!(
            "BEFORE: {:<9} {:<8} ({} bps)",
            route.as_str(),
            human::format_percent(before),
            before
        ),
        None => println!(
            "BEFORE: {:<9} NONE — this route has no rate in force yet",
            route.as_str()
        ),
    }
    println!(
        "AFTER:  {:<9} {:<8} ({} bps)",
        route.as_str(),
        human::format_percent(plan.after()),
        plan.after()
    );
    if plan.is_noop() {
        println!("\nNO CHANGE — this route already prices at exactly that rate.");
    }

    println!("\nEvery OTHER route, unchanged — re-read from the edited file, not asserted:");
    for (other, bps) in plan.resulting().iter() {
        if other == route {
            continue;
        }
        println!(
            "  {:<9} {:<8} ({} bps)",
            other.as_str(),
            human::format_percent(bps),
            bps
        );
    }

    if !plan.seeded_routes().is_empty() {
        println!(
            "\nThis config had no [fees] section, so one is being CREATED. A [fees] section is\n\
             authoritative and cannot be created half-empty, so these keys are written out at\n\
             the rates ALREADY IN FORCE — this states what the deployment is doing, it does not\n\
             change it:"
        );
        for seeded in plan.seeded_routes() {
            println!("    {}", seeded.as_str());
        }
    }

    if !execute {
        println!(
            "\nDRY RUN — nothing was written. The candidate file was validated by the real config\n\
             parser, checked to move exactly one route, and then removed. Re-run with --execute\n\
             to install it."
        );
        plan.discard();
        return Ok(());
    }

    let report = edit::commit(plan, now_unix()).map_err(|e| e.to_string())?;
    println!("\nAPPLIED.");
    println!("  Backup:  {}", report.backup.display());
    println!("  Config:  {}", report.path.display());
    println!(
        "\nThe running daemon has NOT been restarted and has NOT reloaded anything — it still\n\
         prices at the previous rate until an operator restarts it deliberately. No route was\n\
         enabled, no secret was read, and no on-chain transaction was signed or sent: the\n\
         Robinhood contract holds no fee, so there is nothing on chain to reconcile."
    );
    println!(
        "\nIn-flight requests are unaffected. Each one snapshotted its rate at creation and\n\
         settles at THAT rate (amount_conversion::verify_fee_breakdown), so this change applies\n\
         to new requests only."
    );
    Ok(())
}

/// `chain-policy-check-config`
///
/// The preflight the interactive manager runs BEFORE it draws a menu, so
/// an unusable path is named once, up front, instead of producing the
/// same parse error under every action.
///
/// Reads one file. Writes nothing, contacts nothing, and never touches a
/// key, a database or a chain.
fn cmd_chain_policy_check_config(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::chain_policy::inspect::{self, FileKind};

    let path = Path::new(require(args, "--config"));
    let kind = inspect::inspect(path);

    if args.iter().any(|a| a == "--porcelain") {
        println!("path\t{}", path.display());
        println!("kind\t{}", kind.tag());
        println!("usable\t{}", kind.is_usable());
        match &kind {
            FileKind::PolicyFragment {
                policies,
                missing_sections,
            } => {
                for section in missing_sections {
                    println!("missing_section\t{section}");
                }
                for fragment in policies {
                    println!("fragment_network\t{}", fragment.chain.as_str());
                    match &fragment.policy {
                        Ok(policy) => {
                            println!("fragment_fee_bps\t{}", policy.fee_bps());
                            println!(
                                "fragment_per_transfer_limit\t{}",
                                policy.per_transfer_limit().0
                            );
                            println!(
                                "fragment_rolling_daily_limit\t{}",
                                policy.rolling_daily_limit().0
                            );
                        }
                        Err(detail) => println!("fragment_error\t{detail}"),
                    }
                }
            }
            FileKind::IncompleteConfig { missing_sections } => {
                for section in missing_sections {
                    println!("missing_section\t{section}");
                }
            }
            FileKind::InvalidConfig { detail }
            | FileKind::NotToml { detail }
            | FileKind::Unreadable { detail } => {
                println!("detail\t{}", detail.replace('\n', " "));
            }
            FileKind::FullConfig | FileKind::Missing => {}
        }
        return usable_or_refused(path, &kind);
    }

    println!("Goldcoin Bridge — config file check");
    println!("File: {}\n", path.display());
    println!("{}\n", kind.headline());
    println!("{}", chain_policy_file_explanation(path, &kind));

    if let FileKind::PolicyFragment { policies, .. } = &kind {
        for fragment in policies {
            println!(
                "\nThe policy [{}] states, read out of the fragment for reference only:",
                fragment.section()
            );
            match &fragment.policy {
                Ok(policy) => {
                    print_policy(policy);
                    if fragment.chain == glc_reserve_bridge_service::routes::Chain::Robinhood {
                        print_rolling_bucket_note(policy)?;
                    }
                    println!("\nTo state exactly this in the config the daemon loads:");
                    println!("  scripts/chain-policy.sh --config /etc/glc-bridge/config.toml");
                    println!("or, without the menus:");
                    println!(
                        "  glc-admin chain-policy-apply --config /etc/glc-bridge/config.toml \\\n    \
                         --network {} --fee-bps {} --per-transfer-limit {} \\\n    \
                         --rolling-daily-limit {} --note \"why\" --dry-run",
                        fragment.chain.as_str(),
                        policy.fee_bps(),
                        policy.per_transfer_limit().0,
                        policy.rolling_daily_limit().0,
                    );
                    println!("(drop --dry-run for --execute once the dry run reads correctly)");
                }
                Err(detail) => println!(
                    "  UNREADABLE — {detail}. The fragment is still not a config file; that is \
                     the answer either way."
                ),
            }
        }
    }

    println!("\nNothing was written. `chain-policy-check-config` cannot modify a file.");
    usable_or_refused(path, &kind)
}

/// A usable config exits 0; anything else exits non-zero, so a shell can
/// branch on the exit status alone.
fn usable_or_refused(
    path: &Path,
    kind: &glc_reserve_bridge_service::chain_policy::inspect::FileKind,
) -> Result<(), String> {
    if kind.is_usable() {
        Ok(())
    } else {
        Err(format!(
            "{} is not usable as a bridge config ({}) — see the explanation above",
            path.display(),
            kind.tag()
        ))
    }
}

// ===================================================================
// Robinhood governance
// ===================================================================
//
// The three actions an operator may propose against the deployed
// `GlcRobinhoodBridge`: the limit set, the pause flags, and one route's
// enable flag. Every one of them is a DRY RUN unless `--execute` is
// passed, and every one of them requires a 2-of-3 quorum of the
// production custody domains — this binary holds no authorization key
// and cannot manufacture one.
//
// Nothing here contains a fee or a limit. `setLimits`'s values come from
// `[robinhood.policy]` in the supplied config, through
// `RobinhoodPolicyBinding`, which is also what derives the on-chain
// fixed-bucket figure from the configured strict policy. Changing the
// policy is `scripts/chain-policy.sh`'s job; this reconciles the CONTRACT
// to whatever that policy currently says.

/// The deployment identity every governance command verifies before it
/// builds anything: one chain id, one contract, agreed by the config and
/// by the endpoint itself.
struct GovernanceDeployment {
    domain: glc_reserve_bridge_service::robinhood::auth::BridgeDomain,
    chain_id: glc_reserve_bridge_service::evm::EvmChainId,
    reader: glc_reserve_bridge_service::robinhood::calls::BridgeReader,
    rpc: glc_reserve_bridge_service::robinhood::rpc::EvmRpcClient,
}

/// Resolves and CROSS-CHECKS the deployment this config points at.
///
/// The indexer section and the settlement section each name a chain id
/// and a bridge contract. They are required to agree with each other and
/// with the endpoint's own `eth_chainId`, because a governance signature
/// is bound to exactly one (chain id, contract) pair and a disagreement
/// between two config sections is precisely how a signature gets gathered
/// for the wrong one.
async fn governance_deployment(config: &Config) -> Result<GovernanceDeployment, String> {
    use glc_reserve_bridge_service::robinhood::auth::BridgeDomain;
    use glc_reserve_bridge_service::robinhood::calls::BridgeReader;
    use glc_reserve_bridge_service::robinhood::rpc::EvmRpc;

    let indexer = config.robinhood_indexer.as_ref().ok_or(
        "this config has no [robinhood.indexer] section, so there is no contract address \
                and no endpoint to govern through",
    )?;
    let settlement = config.robinhood_settlement.as_ref().ok_or(
        "this config has no [robinhood.settlement] section, so it names no submitter and \
                no authorized signers — a governance action could be neither signed nor sent",
    )?;

    if settlement.chain_id != indexer.chain_id {
        return Err(format!(
            "[robinhood.indexer].chain_id is {} but [robinhood.settlement].chain_id is {} — \
             refusing to build a governance authorization while the config disagrees with itself \
             about which network this is",
            indexer.chain_id.get(),
            settlement.chain_id.get()
        ));
    }
    if settlement.bridge_contract != indexer.bridge_contract {
        return Err(format!(
            "[robinhood.indexer].bridge_contract is {} but [robinhood.settlement].bridge_contract \
             is {} — refusing to govern while the config disagrees with itself about which \
             contract this is",
            indexer.bridge_contract.to_checksum_string(),
            settlement.bridge_contract.to_checksum_string()
        ));
    }

    let rpc = robinhood_rpc(config)?;
    let live = rpc
        .chain_id()
        .await
        .map_err(|e| format!("could not read the endpoint's chain id: {e}"))?;
    if live != indexer.chain_id {
        return Err(format!(
            "the configured chain id is {} but the endpoint reports {} — this RPC is not the \
             network this deployment governs",
            indexer.chain_id.get(),
            live.get()
        ));
    }

    Ok(GovernanceDeployment {
        domain: BridgeDomain::new(indexer.chain_id, indexer.bridge_contract),
        chain_id: indexer.chain_id,
        reader: BridgeReader::new(indexer.bridge_contract),
        rpc,
    })
}

/// Renders one limit set in both units an operator reads.
fn print_limits(label: &str, limits: &glc_reserve_bridge_service::robinhood::calls::BridgeLimits) {
    use glc_reserve_bridge_service::amount_conversion::robinhood::RobinhoodAtomic;
    use glc_reserve_bridge_service::chain_policy::human;

    println!("{label}");
    for (name, value) in [
        ("inboundMin", limits.inbound_min),
        ("inboundMax", limits.inbound_max),
        ("inboundRollingLimit", limits.inbound_rolling_limit),
        ("outboundMin", limits.outbound_min),
        ("outboundMax", limits.outbound_max),
        ("outboundRollingLimit", limits.outbound_rolling_limit),
        ("protectedMinReserve", limits.protected_min_reserve),
    ] {
        let rendered = value
            .try_to_u128()
            .ok()
            .map(RobinhoodAtomic::new)
            .and_then(|a| a.to_canonical().ok())
            .map(|c| human::format_glc(c.0))
            .unwrap_or_else(|| "(not a canonical amount)".to_string());
        println!(
            "    {name:<22} {rendered:<20} {value} (18dp)",
            value = value.to_word_hex()
        );
    }
}

/// The before/after block every governance command prints, in both
/// postures, before anything is signed.
fn print_governance_plan(
    plan: &glc_reserve_bridge_service::robinhood::governance_session::GovernancePlan,
    note: &str,
) {
    use glc_reserve_bridge_service::robinhood::governance::GovernancePayload;

    println!("Robinhood governance proposal");
    println!(
        "  Contract:   {}",
        plan.domain.verifying_contract.to_checksum_string()
    );
    println!("  Chain id:   {}", plan.domain.chain_id.get());
    println!("  Action:     {}", plan.auth.payload.kind_str());
    println!("  Nonce:      {}", plan.auth.nonce.to_word_hex());
    println!("  Epoch:      {}", plan.auth.signer_epoch);
    println!("  Expiry:     {}", plan.auth.expiry);
    println!("  Note:       {note}");
    println!(
        "  Digest:     {}",
        glc_reserve_bridge_service::evm::hex::encode_lower(&plan.digest)
    );
    println!("\nThis is the digest a 2-of-3 quorum of custody domains must each independently");
    println!("rebuild from the proposal's structured fields and sign. This tool holds no");
    println!("authorization key and cannot produce a signature itself.\n");

    match &plan.auth.payload {
        GovernancePayload::SetLimits(after) => {
            print_limits("BEFORE (on chain now):", &plan.before.limits);
            println!();
            print_limits("AFTER (proposed):", after);
        }
        GovernancePayload::SetPaused { .. } => {
            println!(
                "BEFORE:  depositsPaused = {}, payoutsPaused = {}",
                plan.before.deposits_paused, plan.before.payouts_paused
            );
            println!(
                "AFTER:   depositsPaused = {}, payoutsPaused = {}",
                plan.after.deposits_paused, plan.after.payouts_paused
            );
            println!(
                "\nClearing a pause does NOT enable any route: the two gates are independent, \
                 and a\nroute governance never enabled stays closed with both directions open."
            );
        }
        GovernancePayload::SetRouteEnabled { route, .. } => {
            println!(
                "BEFORE:  routeEnabled(GlcToRhn) = {}, routeEnabled(RhnToGlc) = {}, \
                 routeEnabled(SolToRhn) = {}, routeEnabled(RhnToSol) = {}",
                plan.before.glc_to_rhn_enabled,
                plan.before.rhn_to_glc_enabled,
                plan.before.sol_to_rhn_enabled,
                plan.before.rhn_to_sol_enabled
            );
            println!(
                "AFTER:   routeEnabled(GlcToRhn) = {}, routeEnabled(RhnToGlc) = {}, \
                 routeEnabled(SolToRhn) = {}, routeEnabled(RhnToSol) = {}",
                plan.after.glc_to_rhn_enabled,
                plan.after.rhn_to_glc_enabled,
                plan.after.sol_to_rhn_enabled,
                plan.after.rhn_to_sol_enabled
            );
            println!(
                "\nOnly {} changes. Enabling a route does not unpause anything: a route is live",
                route.as_str()
            );
            println!("only when governance has enabled it AND its direction is unpaused.");
        }
        GovernancePayload::CommitMigration { successor } => {
            println!(
                "BEFORE:  migrationCommitted = {}, migrationSuccessor = {}",
                plan.before.migration_committed,
                plan.before.migration_successor.to_checksum_string()
            );
            println!(
                "AFTER:   migrationCommitted = true, migrationSuccessor = {}",
                successor.to_checksum_string()
            );
            println!(
                "         depositsPaused = {}, payoutsPaused = {} (both were already true; the \
                 contract requires it)",
                plan.before.deposits_paused, plan.before.payouts_paused
            );
            println!(
                "\nTERMINAL FOR THE ROUTES. Once this lands no deposit can ever be created on this \
                 contract\nagain and no pause can be cleared; the only ways out are \
                 finalizeMigration (a second\n2-of-3 at the next nonce) or any ONE guardian's \
                 vetoMigration(). Pending obligations:\n  count = {}, principal = {} (18dp) — every \
                 one must reach Settled/Refunded/Abandoned\nbefore finalize is possible. \
                 migrationFinalizableAt will be reported by the chain after\nthe commit; on a \
                 deployment that carries a MIGRATION_DELAY it is commit + delay, and\nnothing \
                 off chain shortens it.",
                plan.before.outstanding_refundable_count.to_word_hex(),
                plan.before.outstanding_refundable_principal.to_word_hex()
            );
        }
        GovernancePayload::FinalizeMigration { successor } => {
            println!(
                "BEFORE:  migrated = false, migrationSuccessor = {}, migrationFinalizableAt = {}",
                plan.before.migration_successor.to_checksum_string(),
                plan.before.migration_finalizable_at
            );
            println!(
                "AFTER:   migrated = true — the ENTIRE reserve balance moves to {} and this \
                 contract is terminal",
                successor.to_checksum_string()
            );
            println!(
                "\nThe contract will transfer balanceOf(bridge) in full; there is no amount \
                 argument. Pending\nobligations are {} / {} (18dp), which the chain requires to \
                 be zero. After the receipt,\nre-point the config and every custody domain at \
                 the successor and run robinhood-preflight.",
                plan.before.outstanding_refundable_count.to_word_hex(),
                plan.before.outstanding_refundable_principal.to_word_hex()
            );
        }
    }

    if plan.is_noop() {
        println!("\nNO CHANGE — the contract already holds exactly this.");
    }
}

/// Shared tail: dry run by default, `--execute` gathers a quorum,
/// simulates, broadcasts and verifies.
async fn run_governance(
    args: &[String],
    config: &Config,
    payload: glc_reserve_bridge_service::robinhood::governance::GovernancePayload,
) -> Result<(), String> {
    use glc_reserve_bridge_service::robinhood::governance_session::{
        execute, plan as build_plan, read_state, ReceiptWait,
    };
    use glc_reserve_bridge_service::robinhood::rpc::EvmBlockTag;
    use glc_reserve_bridge_service::robinhood::submitter::Submitter;

    let note = require_note(args)?;
    let execute_it = args.iter().any(|a| a == "--execute");
    let deployment = governance_deployment(config).await?;

    let before = read_state(&deployment.reader, &deployment.rpc, EvmBlockTag::Latest)
        .await
        .map_err(|e| e.to_string())?;

    // An authorization's life is bounded by the settlement config's own
    // TTL, which every custody domain independently re-checks against its
    // own ceiling. Not a new knob: a domain that bounded one
    // authorization's lifetime has bounded them all.
    let settlement = config
        .robinhood_settlement
        .as_ref()
        .expect("governance_deployment required it");
    let now = now_unix() as u64;
    let expiry = now + settlement.authorization_ttl.as_secs();

    let plan = build_plan(
        before,
        deployment.domain,
        deployment.chain_id,
        payload,
        expiry,
        now,
    )
    .map_err(|e| e.to_string())?;
    print_governance_plan(&plan, note);

    if !execute_it {
        println!(
            "\nDRY RUN — no custody domain was contacted, no signature was gathered, no \
             transaction\nwas built or sent, and the governance nonce was not consumed. Re-run \
             with --execute to\ngather a quorum and install this."
        );
        return Ok(());
    }

    if plan.is_noop() {
        return Err(
            "refusing to spend a governance nonce and a quorum's attention on a change that \
             would alter nothing"
                .to_string(),
        );
    }

    let signers = config
        .load_robinhood_governance_signers()
        .await
        .map_err(|e| format!("could not connect the custody domains: {e}"))?;
    let refs: Vec<&glc_reserve_bridge_service::signing::remote::RemoteEvmAuthSigner> =
        signers.iter().collect();
    let submitter = Submitter::load(settlement)
        .map_err(|e| format!("the configured submitter key is not usable: {e}"))?;

    println!("\nGathering a {}-of-{} quorum...", THRESHOLD, refs.len());
    let outcome = execute(
        &plan,
        &deployment.reader,
        &deployment.rpc,
        &submitter,
        &refs,
        THRESHOLD,
        ReceiptWait::default(),
        |secs| {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            })
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    println!("\nINSTALLED and VERIFIED.");
    println!("  Transaction: {}", outcome.tx_hash);
    println!("  Gas used:    {}", outcome.gas_used);
    println!("  Signers:     {}", outcome.signers.join(", "));
    println!(
        "\nThe contract was re-read after the receipt and holds exactly what this proposal \
         said.\nNothing else changed: no route was enabled as a side effect, no config file was \
         edited,\nand the daemon was NOT restarted — it still holds whatever [robinhood.policy] \
         says until\nan operator restarts it deliberately."
    );
    Ok(())
}

/// The contract's `SIGNER_THRESHOLD`. Exactly this many, never "at least".
const THRESHOLD: usize = glc_reserve_bridge_service::robinhood::SIGNER_THRESHOLD;

/// `robinhood-governance-set-limits`
fn cmd_robinhood_governance_set_limits(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::robinhood::governance::{
        limits_from_policy, GovernancePayload, MinimumOverrides,
    };
    use glc_reserve_bridge_service::robinhood::governance_session::read_state;
    use glc_reserve_bridge_service::robinhood::rpc::EvmBlockTag;
    use glc_reserve_bridge_service::robinhood::RobinhoodPolicyBinding;
    use glc_reserve_bridge_service::routes::Chain;
    use glc_reserve_bridge_service::signing::evm_governance::robinhood_atomic_from_decimal;

    let config = load_policy_config(Path::new(require(args, "--config")))?;

    // The ONLY source of the fee and the ceilings.
    let policy = config.chain_policies.get(Chain::Robinhood).copied().ok_or(
        "this config has no [robinhood.policy] section, so there is no approved policy to \
             reconcile the contract to. Set one with `scripts/chain-policy.sh --config <this \
             file>` first — this command installs what that policy says and has no figures of \
             its own",
    )?;
    let binding = RobinhoodPolicyBinding::new(policy).map_err(|e| e.to_string())?;

    let mut overrides = MinimumOverrides::default();
    for (name, slot) in [
        ("--inbound-min", 0usize),
        ("--outbound-min", 1),
        ("--protected-min", 2),
    ] {
        if let Some(raw) = flag(args, name) {
            let value = robinhood_atomic_from_decimal(raw).map_err(|e| format!("{name}: {e}"))?;
            match slot {
                0 => overrides.inbound_min = Some(value),
                1 => overrides.outbound_min = Some(value),
                _ => overrides.protected_min_reserve = Some(value),
            }
        }
    }

    tokio_block_on(async move {
        let deployment = governance_deployment(&config).await?;
        let current = read_state(&deployment.reader, &deployment.rpc, EvmBlockTag::Latest)
            .await
            .map_err(|e| e.to_string())?;

        println!("Backend policy (from [robinhood.policy] in this config):");
        println!(
            "  fee                  {} ({} bps)",
            glc_reserve_bridge_service::chain_policy::human::format_percent(policy.fee_bps()),
            policy.fee_bps()
        );
        println!(
            "  per transfer         {}",
            glc_reserve_bridge_service::chain_policy::human::format_glc(
                policy.per_transfer_limit().0
            )
        );
        println!(
            "  strict 24h           {}",
            glc_reserve_bridge_service::chain_policy::human::format_glc(
                policy.rolling_daily_limit().0
            )
        );
        println!("\nOn chain required (derived from that policy, not configured separately):");
        println!(
            "  max                  {}",
            glc_reserve_bridge_service::chain_policy::human::format_glc(
                policy.per_transfer_limit().0
            )
        );
        println!(
            "  fixed rolling bucket {}   (= strict 24h / 2; the contract's window is a fixed",
            glc_reserve_bridge_service::chain_policy::human::format_glc(
                binding.expected_onchain_rolling_limit_canonical().0
            )
        );
        println!(
            "                                            bucket whose reachable worst case is 2x)"
        );
        if overrides.is_empty() {
            println!(
                "\nMinimums and the protected minimum are PRESERVED from the contract's current \
                 state.\nPass --inbound-min / --outbound-min / --protected-min (18dp atomic) to \
                 change one."
            );
        } else {
            println!(
                "\nMinimum overrides supplied on the command line will replace the current values."
            );
        }
        println!();

        let proposed =
            limits_from_policy(&binding, &current.limits, overrides).map_err(|e| e.to_string())?;
        run_governance(args, &config, GovernancePayload::SetLimits(proposed)).await
    })
}

/// `robinhood-governance-pause`
fn cmd_robinhood_governance_pause(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::robinhood::governance::GovernancePayload;
    use glc_reserve_bridge_service::robinhood::governance_session::read_state;
    use glc_reserve_bridge_service::robinhood::rpc::EvmBlockTag;

    let config = load_policy_config(Path::new(require(args, "--config")))?;
    let scope = require(args, "--scope").to_string();
    let paused = parse_bool_flag(args, "--paused")?;

    tokio_block_on(async move {
        let deployment = governance_deployment(&config).await?;
        let current = read_state(&deployment.reader, &deployment.rpc, EvmBlockTag::Latest)
            .await
            .map_err(|e| e.to_string())?;

        // The contract takes BOTH flags, so the direction not named is
        // carried across from the chain's current state rather than
        // defaulted — a proposal that quietly unpaused the other
        // direction would be the worst possible surprise here.
        let payload = match scope.as_str() {
            "deposits" => GovernancePayload::SetPaused {
                deposits_paused: paused,
                payouts_paused: current.payouts_paused,
            },
            "payouts" => GovernancePayload::SetPaused {
                deposits_paused: current.deposits_paused,
                payouts_paused: paused,
            },
            other => {
                return Err(format!(
                    "--scope must be `deposits` or `payouts`, not {other:?}. The contract holds \
                     one flag per direction and this tool changes exactly the one you name, \
                     carrying the other across unchanged"
                ))
            }
        };
        run_governance(args, &config, payload).await
    })
}

/// `robinhood-governance-route`
fn cmd_robinhood_governance_route(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::robinhood::governance::GovernancePayload;

    let config = load_policy_config(Path::new(require(args, "--config")))?;
    let route = parse_governable_route(require(args, "--route"))?;
    let enabled = parse_bool_flag(args, "--enabled")?;

    tokio_block_on(async move {
        run_governance(
            args,
            &config,
            GovernancePayload::SetRouteEnabled { route, enabled },
        )
        .await
    })
}

/// `robinhood-governance-commit-migration`
fn cmd_robinhood_governance_commit_migration(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::robinhood::governance::GovernancePayload;
    use glc_reserve_bridge_service::robinhood::governance_session::check_successor;

    let config = load_policy_config(Path::new(require(args, "--config")))?;
    let successor = parse_successor(require(args, "--successor"))?;

    tokio_block_on(async move {
        let deployment = governance_deployment(&config).await?;
        // The contract's own structural checks, re-stated with reasons,
        // before a quorum is asked. They prove the successor is not an
        // obvious mistake; they do not prove it is correct.
        check_successor(&deployment.rpc, deployment.reader.bridge, successor)
            .await
            .map_err(|e| e.to_string())?;
        println!(
            "Successor {} has code, custodies the bridge's token and reports this protocol \
             family.\nThat is every check the CONTRACT makes. It is not a proof the successor is \
             correct:\nverify its deployed bytecode against this repository's build before \
             --execute.\n",
            successor.to_checksum_string()
        );
        run_governance(
            args,
            &config,
            GovernancePayload::CommitMigration { successor },
        )
        .await
    })
}

/// `robinhood-governance-finalize-migration`
fn cmd_robinhood_governance_finalize_migration(args: &[String]) -> Result<(), String> {
    use glc_reserve_bridge_service::robinhood::governance::GovernancePayload;

    let config = load_policy_config(Path::new(require(args, "--config")))?;
    // Required, not read from the chain: the quorum approves THIS address,
    // and the session refuses the plan if the chain holds a different one.
    let successor = parse_successor(require(args, "--successor"))?;

    tokio_block_on(async move {
        run_governance(
            args,
            &config,
            GovernancePayload::FinalizeMigration { successor },
        )
        .await
    })
}

fn parse_successor(raw: &str) -> Result<glc_reserve_bridge_service::evm::EvmAddress, String> {
    let address: glc_reserve_bridge_service::evm::EvmAddress = raw
        .parse()
        .map_err(|e| format!("--successor {raw:?} is not an EVM address: {e}"))?;
    if address.is_zero() {
        return Err("--successor is the zero address; the contract refuses it".to_string());
    }
    Ok(address)
}

/// The `--route` of `robinhood-governance-route`: exactly the routes the
/// custody contract models, i.e. those with a
/// [`glc_reserve_bridge_service::routes::Route::contract_route_id`].
/// Refused HERE as well as in the encoder
/// (`governance::governance_route_byte`), so the message an operator sees
/// names the reason rather than an encoding failure.
///
/// The two Solana<->Goldcoin routes have no contract discriminator — the
/// contract is Robinhood-side custody and never sees them — so there is no
/// on-chain flag to set. Every route WITH a discriminator is governable
/// here, including `SolToRhn`/`RhnToSol` since Phase H gave them
/// settlement machinery. Governing a route says nothing about whether it
/// opens: config, `bridge_routes`, adapter capability and the local pauses
/// all still stand in front of it.
fn parse_governable_route(raw: &str) -> Result<glc_reserve_bridge_service::routes::Route, String> {
    use glc_reserve_bridge_service::routes::Route;

    let route: Route = raw.parse().map_err(|_| {
        format!(
            "--route {raw:?} is not a route this bridge models — expected one of GlcToRhn, \
             RhnToGlc, SolToRhn or RhnToSol"
        )
    })?;
    if route.contract_route_id().is_none() {
        return Err(format!(
            "{} cannot be enabled or disabled by this tool: the custody contract does not model \
             it (it has no route discriminator), so there is no on-chain flag to set. Its \
             controls are the local pause and admission commands",
            route.as_str()
        ));
    }
    Ok(route)
}

fn parse_bool_flag(args: &[String], name: &str) -> Result<bool, String> {
    match require(args, name) {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!(
            "{name} must be exactly `true` or `false`, not {other:?} — a pause or an enable flag \
             is not a value to guess at"
        )),
    }
}

/// Runs one async operator command on a throwaway runtime.
fn tokio_block_on<F>(future: F) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), String>>,
{
    tokio::runtime::Runtime::new()
        .map_err(|e| format!("could not start a runtime: {e}"))?
        .block_on(future)
}

#[cfg(test)]
mod governance_route_tests {
    use super::parse_governable_route;
    use glc_reserve_bridge_service::robinhood::governance::governance_route_byte;
    use glc_reserve_bridge_service::routes::Route;

    /// The two Goldcoin<->Robinhood routes are accepted exactly as before.
    #[test]
    fn goldcoin_robinhood_pair_is_still_accepted() {
        assert_eq!(parse_governable_route("GlcToRhn"), Ok(Route::GlcToRhn));
        assert_eq!(parse_governable_route("RhnToGlc"), Ok(Route::RhnToGlc));
    }

    /// Phase H: the two Solana<->Robinhood routes are accepted, and map to
    /// the contract's 0x03/0x04 discriminators.
    #[test]
    fn solana_robinhood_pair_is_accepted_with_its_contract_bytes() {
        let sol_to_rhn = parse_governable_route("SolToRhn").expect("SolToRhn is governable");
        let rhn_to_sol = parse_governable_route("RhnToSol").expect("RhnToSol is governable");
        assert_eq!(sol_to_rhn, Route::SolToRhn);
        assert_eq!(rhn_to_sol, Route::RhnToSol);
        assert_eq!(sol_to_rhn.contract_route_id(), Some(0x03));
        assert_eq!(rhn_to_sol.contract_route_id(), Some(0x04));
        // The encoder agrees, so the CLI can never admit a route the payload
        // then refuses.
        assert_eq!(governance_route_byte(sol_to_rhn), Ok(0x03));
        assert_eq!(governance_route_byte(rhn_to_sol), Ok(0x04));
    }

    /// Routes the contract does not model are still refused, with a message
    /// that names the reason.
    #[test]
    fn routes_without_a_contract_discriminator_are_refused() {
        for raw in ["GlcToSol", "SolToGlc"] {
            let err = parse_governable_route(raw).expect_err("no discriminator");
            assert!(err.starts_with(raw), "{err}");
            assert!(err.contains("does not model"), "{err}");
            assert!(governance_route_byte(raw.parse().unwrap()).is_err());
        }
    }

    /// Unknown spellings are refused before any route logic runs, and the
    /// hint lists every governable route.
    #[test]
    fn unknown_route_spellings_are_refused() {
        for raw in ["", "soltorhn", "SolToRHN", "0x03", "GlcToRhn "] {
            let err = parse_governable_route(raw).expect_err("not a route");
            assert!(err.contains("not a route this bridge models"), "{err}");
            for name in ["GlcToRhn", "RhnToGlc", "SolToRhn", "RhnToSol"] {
                assert!(err.contains(name), "{err}");
            }
        }
    }

    /// The CLI's accepted set is exactly the contract-modelled set — pinned
    /// against the registry so a new route variant cannot drift between
    /// the two.
    #[test]
    fn accepted_set_equals_contract_modelled_set() {
        for route in Route::ALL {
            let accepted = parse_governable_route(route.as_str()).is_ok();
            assert_eq!(
                accepted,
                route.contract_route_id().is_some(),
                "{}",
                route.as_str()
            );
            assert_eq!(
                accepted,
                governance_route_byte(route).is_ok(),
                "{}",
                route.as_str()
            );
        }
    }
}
