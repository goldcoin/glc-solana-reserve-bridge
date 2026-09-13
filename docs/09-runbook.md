# Operational Runbook (Draft)

Structured after the old bridge's `docs/runbooks.md` discipline: every procedure here should eventually be backed by an executable `glc-admin`/`glc-audit` command, asserted by CI to actually exist and behave as documented (reused practice — the old bridge's `runbook_commands.rs` caught real drift between docs and binaries repeatedly; ported to `service/tests/runbook_commands.rs`). [07-implementation-plan.md](07-implementation-plan.md) Phase 5 landed a first, deliberately partial set of real commands — see "Executable commands" below for exactly what exists today and what is explicitly still a paper procedure.

## Executable commands (Phase 5)

What actually exists, so this document never claims more than the binaries do:

- `glc-admin status --db PATH` — reserve snapshots (both directions, including cumulative accrued bridge-fee revenue — docs/20-bridge-fee.md) and the `ManualReview` backlog count.
- `glc-admin pause --db PATH --direction <goldcoin|solana> --note TEXT` / `glc-admin unpause ...` — this service's own local ledger admission gate (independent of the on-chain pause below). Reaches the `GoldcoinReserve` and `SolanaReserve` rows only; `--direction robinhood` is deliberately not accepted, because the third reserve's local gate has its own command and its own unpause guard (next line).
- `glc-admin robinhood-local-pause --db PATH --paused <true|false> --note TEXT` — the LOCAL `GlcToRhn` reserve gate: `reserve_ledger.paused` on the `RobinhoodReserve` row, and nothing else. Contacts no chain, no signer and no config file. `--paused true` is an unconditional emergency stop; `--paused false` is refused unless the reserve invariant holds and the route would actually be fundable once the flag clears. See "The local `RobinhoodReserve.paused` gate" below — **this is not the contract's `depositsPaused`/`payoutsPaused`, and it does not gate `RhnToGlc`.**
- `glc-admin show-config --rpc-url URL` — decodes and prints the on-chain `BridgeConfig`.
- `glc-admin onchain-pause --rpc-url URL --keypair PATH --scope <global|release|deposit> --note TEXT` / `glc-admin onchain-unpause ...` — submits the admin-gated-immediate `set_paused` instruction (docs/12-management-decisions.md's Phase 2 scoping decision: pause is admin-gated-immediate, not threshold-gated).
- `glc-admin set-limit --rpc-url URL --keypair PATH --field <min-transfer|per-transfer|protected-minimum|rolling-volume> --value N --note TEXT` — submits the admin-gated-immediate `set_limit` instruction (same posture as `onchain-pause` above). `--value` is atomic units of the Solana-side mint; `set_limit`'s on-chain check is against the NET release amount (`release_from_reserve`'s `limits.rs::enforce_transfer_amount`), so a `min-transfer` value must already account for the 3% bridge fee being deducted before comparison.
- `glc-admin show-authorities --rpc-url URL` — read-only. Prints, in one place, who can currently do what: `BridgeConfig.admin`, any pending admin handover, the program's real BPF-loader upgrade authority, and whether the on-chain upgrade timelock has been armed. **These are independent authorities** — changing one never changes the other; they coincided on this deployment only because `initialize` seeds the admin from whoever holds the upgrade authority at genesis. The command warns loudly if they are still the same key. Run it before and after any rotation, and as part of any incident triage. Added after the 2026-09-02 reserve-withdrawal incident, where answering "who can move reserve funds right now?" required hand-decoding two accounts owned by two different programs.
- `glc-admin transfer-admin --rpc-url URL --keypair PATH --new-admin PUBKEY --note TEXT` — step 1 of 2 of the admin handover, signed by the CURRENT admin. Nothing changes on chain until the new admin runs `accept-admin`: the two-step shape is the safeguard against handing governance to a typo. Prints the exact follow-up command to run. See "Rotating the BridgeConfig admin" below.
- `glc-admin accept-admin --rpc-url URL --keypair PATH --note TEXT` — step 2 of 2, signed by the NEW admin **on the machine that holds that key**. This is the call that actually moves `BridgeConfig.admin`. Verify with `show-authorities` afterwards.
- `glc-admin rebalance-policy-show --rpc-url URL` — read-only. Prints the on-chain `RebalancePolicy`: the treasury-destination allowlist (which is the whole policy — there is deliberately no amount ceiling, rate limit or rolling withdrawal budget on a treasury withdrawal; `protected_minimum`, set via `set-limit`, remains the one accounting floor), and any queued policy change sitting in its governance timelock. **A queued change you did not expect is an incident**: a quorum of attestation keys is proposing to change where reserve funds may be sent, and there is still time to cancel it before its `eta`. If no policy exists at all, the command says so and explains that this is the safe state (no allowlisted destination means `treasury_withdraw` fails closed for every destination) rather than a broken one.
- `glc-rebalance-policy plan|attest|execute|apply|verify` — the staged tool that CREATES and GOVERNS the `RebalancePolicy` (`docs/30-reserve-policy-deployment-runbook.md`). Authorized by threshold attestation only: there is deliberately **no `--admin-keypair` flag anywhere in it**, because an allowlist the admin key can create or edit is not an allowlist. The keypair it does take pays fees/rent and confers no authority. Same three-host separation as `glc-treasury-withdraw` — `plan` needs no key and is the dry run, `attest` runs on the approval host, `execute` needs only a fee payer and always simulates before broadcasting (and only broadcasts with `--execute`). `apply` runs a timelocked change once its `eta` has passed; `verify` asserts the live policy equals the values you intended, allowlist order included, and exits non-zero on any mismatch. **`treasury_withdraw` fails closed for every destination until `plan --action init` has been executed**, so this tool is a mandatory step of the reserve-withdrawal-hardening deployment, not an optional extra.
- `glc-admin retry-goldcoin-payout --config PATH --request-id N --note TEXT` — recovers a Solana->Goldcoin payout stuck in `goldcoin_payouts.state = 'Signed'` after its broadcast was rejected (e.g. request #8, Goldcoin RPC `-26: 64: non-mandatory-script-verify-flag (Non-canonical signature: S value is unnecessarily high)` — see the low-S signing fix). Never invoked automatically: `Orchestrator::tick_goldcoin_payouts` always skips a request that already has a `goldcoin_payouts` row, by design, so a stuck payout needs this explicit command, and this command alone. It never rebroadcasts the previously stored `signed_tx_hex` as-is, never selects a new UTXO, and never builds a second payout row — it independently reconstructs the exact same plan from the already-persisted `goldcoin_payouts`/`goldcoin_payout_inputs` rows (refusing on any mismatch against freshly recomputed request data, or if the reconstructed unsigned transaction does not byte-for-byte match what was originally built), re-runs the real independent multi-signer signing path (`signing::goldcoin_vault::independently_sign_all_inputs`, the same function a normal payout build uses), and only calls `Ledger::record_goldcoin_payout_broadcast` after the Goldcoin RPC actually accepts the resulting transaction (or reports it already known). If the broadcast fails again, the payout stays exactly in `Signed` and `bridge_requests` stays in `SettlementAuthorized` — nothing is marked done on a failed attempt. Safe to re-run: a payout already `Broadcast`/`Confirmed`/`Completed` is reported and left untouched. Unlike every other command above, this one needs `--config` (the same config file `glc-bridge-daemon` uses), not `--db` — recovery signs and broadcasts a real transaction, so it needs the configured vault signers and Goldcoin RPC, not just ledger access. The command prints whether the re-signed transaction differs from what was previously stored; if it does **not** differ, that's a strong signal the original rejection has a cause other than signature canonicalization, and needs separate investigation before assuming a retry will succeed.
- `glc-admin refund-manual-review --config PATH --request-id N --note TEXT [--keypair ADMIN_KEYPAIR] [--execute]` — refunds a fold-parked SolToGlc deposit to its ORIGINAL Solana depositor and permanently closes the request. Without `--execute` this is a strict read-only dry run (contacts no signer, loads no keypair, writes nothing, broadcasts nothing). With `--execute` it requires the bridge to be **already globally paused on-chain**, re-verifies everything against fresh state, always simulates before broadcasting, and confirms at `finalized` commitment before marking the request `Refunded`. See "ManualReview refunds (Solana->Goldcoin)" below for the full procedure — do not run this from this list alone.
- `glc-admin refund-list --db PATH [--open-only]` — read-only listing of every refund lifecycle.
- `glc-admin manual-review-settle --config PATH --request-id N --note TEXT [--execute]` — the OPPOSITE decision to a refund: completes the user's original bridge request onto Goldcoin L1 by re-admitting it into the existing payout pipeline. Dry run by default; needs no keypair in either mode. See "ManualReview -> L1 settlement recovery" below.
- `glc-admin manual-review-settle-list (--config PATH | --db PATH)` — read-only recovery-candidate listing. Each candidate is shown with the verdict of the same dry run `manual-review-settle` performs on it (with `--config`, the on-chain deposit proof included), so the listing and that command can never disagree. Candidates currently refused are listed too, with the reason.
- `glc-admin refund-glc-manual-review --config PATH --request-id N --note TEXT [--execute]` — returns a GOLDCOIN deposit that was accepted on chain but can never settle (a `GlcToSol` request parked in `ManualReview` for `deposit_amount_mismatch`) to the wallet that sent it. Dry run by default. The refund amount and destination are derived from verified chain data and **cannot** be supplied by an operator — there is no `--destination` and no `--amount`. See "Goldcoin-sourced ManualReview refunds (Goldcoin side)" below.
- `glc-admin glc-refund-list --db PATH [--open-only]` — read-only listing of Goldcoin refunds. A `Broadcast` row means a refund transaction ALREADY EXISTS (its txid is printed) — never that one still needs sending; the listing says so per row.
- `glc-audit --db PATH [--quiet]` — offline integrity auditor: re-verifies every frozen attestation-claim commitment plus `PRAGMA integrity_check`. Exit 0 = clean, 1 = findings, 2 = could not run.
- `scripts/backup-ledger.sh <db path> <backup dir>` — safe online SQLite backup (`sqlite3 .backup`, never a plain file copy) of the ledger, timestamped. Prints the backup's path on success.
- `scripts/restore-ledger.sh <backup file> <destination>` — restores a backup produced by `backup-ledger.sh`, after verifying `PRAGMA integrity_check` on it. Refuses to overwrite an existing destination.
- `scripts/run-audit-cron.sh <db path> <backup dir> [glc-audit path]` — the cron/systemd-timer entry point: takes a fresh backup, then runs `glc-audit` against it (not the live database — see the script's own comments). Exit code is `glc-audit`'s own; wire it directly into your scheduler's failure notification.
- `glc-admin rebalance-status --db PATH` — read-only imbalance assessment for both reserves against their own configured target/warning/critical thresholds (docs/22-production-readiness-review.md P1 "rebalancing").
- `glc-admin rebalance-list --db PATH [--direction <goldcoin|solana>] [--open-only]` — lists rebalance requests.
- `glc-admin rebalance-propose --db PATH --direction <goldcoin|solana> --kind <deposit|withdraw> --amount N --by IDENTITY --required-approvals N --note TEXT` — creates a request in `Proposed`.
- `glc-admin rebalance-approve --db PATH --id N --by IDENTITY` — records an approval; idempotent per identity.
- `glc-admin rebalance-reject --db PATH --id N --by IDENTITY --note TEXT` / `glc-admin rebalance-cancel ...` — terminal off-ramps before execution.
- `glc-admin rebalance-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT` — records evidence of a real transfer already authorized and executed through real custody tooling **outside this system** — this command (and this entire service) never constructs, signs, or broadcasts a fund-moving transaction itself. `tx_reference` is a Goldcoin txid or a Solana signature, as plain text, and is unique across every rebalance request ever recorded (structural replay guard).
- `glc-admin rebalance-confirm --db PATH --id N --by IDENTITY --observed-amount N` — records the independently-observed real effect of the executed transfer and updates the cached reserve balance in the same step, so the next reconciliation tick does not misclassify it as an unexplained breach.
- `glc-admin rebalance-fail --db PATH --id N --by IDENTITY --note TEXT` — routes an executed-but-unconfirmed rebalance to manual resolution.
- `glc-admin custody-list --db PATH [--kind <attestation-rotation|vault-sweep>] [--open-only]` — lists custody transitions (docs/22-production-readiness-review.md P1 "key rotation / vault sweep tooling").
- `glc-admin custody-propose --db PATH --kind <attestation-rotation|vault-sweep> --old-identities CSV --new-identities CSV [--new-threshold N] --by IDENTITY --required-approvals N --note TEXT` — creates a transition in `Proposed`. `--new-threshold` only applies to `vault-sweep`.
- `glc-admin custody-verify-identity --db PATH --id N --by IDENTITY` — records that the claimed new signer identity/vault descriptor was independently verified. Required before any approval: `custody-approve` rejects anything still in `Proposed`.
- `glc-admin custody-approve --db PATH --id N --by IDENTITY` — records an approval; idempotent per identity.
- `glc-admin custody-reject --db PATH --id N --by IDENTITY --note TEXT` / `glc-admin custody-cancel ...` — terminal off-ramps before execution.
- `glc-admin custody-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT` — records evidence of a real rotation/sweep already authorized and executed through real custody tooling **outside this system** — this command (and this entire service) never generates keys, signs, or performs a rotation/sweep itself. Enforces the "pause requirements" invariant as a precondition, not documentation: fails unless `GoldcoinReserve` is already paused (`vault-sweep`) or both reserves are already paused (`attestation-rotation`, since attestation authorizes both bridge directions). `tx_reference` is unique across every custody transition ever recorded (structural replay guard).
- `glc-admin custody-confirm --db PATH --id N --by IDENTITY` — records independent confirmation that the new custody identity is active and correct post-transition.
- `glc-admin custody-fail --db PATH --id N --by IDENTITY --note TEXT` — routes an executed-but-unconfirmed transition to manual resolution.
- `glc-admin custody-rollback --db PATH --id N --by IDENTITY --note TEXT` — records that a `Failed` transition's effect was reverted back to the old identity out of band; only ever an audit marker, never performs the rollback itself.

**Rebalancing's and key-rotation/vault-sweep's off-chain engineering layers are now both built** (the commands above: imbalance detection, proposal/approval/execution-evidence/confirmation state machines, identity-verification and pause-requirement gates for custody transitions, structural separation from settlement accounting, replay protection, reconciliation interaction, restart recovery, and audit trail). **No procedure exists yet** for staging the actual out-of-band collection of multi-operator signatures/approvals outside this CLI (the old bridge's equivalent depended on a P2P federation transport this bridge does not have and does not need — see IMPLEMENTATION_LOG.md's Phase 5 entry) — `custody-approve`/`custody-verify-identity` record decisions made elsewhere, they do not collect them. The dedicated on-chain `rebalance_deposit`/`rebalance_withdraw` instructions docs/03-architecture.md originally envisioned for the Solana leg specifically (an atomic, on-chain-enforced structural separation between a rebalance transfer and an arbitrary one) are also not yet built; until those exist, the real fund movement a rebalance or custody transition evidences is an ordinary SPL/Goldcoin transfer or key/vault change executed through whatever wallet/custody tooling already holds the relevant keys, not a bespoke program instruction. These gaps are named explicitly below wherever the procedure that needs them is described, so they stay visible rather than silently assumed away.

## Startup/commissioning sequencing (cold start)

**Required order when bringing up the daemon against a newly-funded or freshly-restarted reserve — do not start the daemon before this sequence completes:**

1. Fund the reserve (send GLC to the Goldcoin vault address; transfer the reserve mint's tokens into the Solana reserve token account).
2. Wait for that funding transaction to reach the same confirmation/finality depth reconciliation itself requires before it is trusted — Goldcoin: `vault_min_confirmations` confirmations on the funding UTXO, read the same way reconciliation reads it (`listunspent` against the vault address, filtered to `solvable` entries — importing a watch-only address triggers a node-side wallet rescan that is not guaranteed to be instantaneous, so confirming the block is mined is not sufficient by itself); Solana: `finalized` commitment on the reserve token account's balance.
3. Only then start `glc-bridge-daemon`.

**Why this matters, concretely**: reconciliation's fail-closed design (see [05-reserve-accounting.md](05-reserve-accounting.md)) has no exception for "just started, haven't observed reality yet" — the ledger's configured starting balance is treated as the baseline from the very first reconciliation tick. If the daemon starts before step 2 has genuinely completed, the very first reconciliation tick can read a real chain balance that has not yet caught up to the full funded amount, classify the entire un-caught-up portion as an unexplained drop, and auto-pause the reserve before the bridge ever processes a single request — a real, once-observed failure mode during this service's own load/soak testing (docs/22-production-readiness-review.md item 7, docs/24-load-soak-harness.md), not a hypothetical one. Because auto-pause is deliberately never automatic to clear (see "Auto-pause triggers" below), a cold-start breach like this requires a manual operator unpause even though nothing was ever actually wrong with the funds.

**Verifying step 2 without running the daemon**: `goldcoin-cli listunspent <vault_min_confirmations> 9999999 '["<vault address>"]'` and confirm the `solvable` entries sum to the funded amount; for Solana, poll the reserve token account at `finalized` commitment until its balance matches what was transferred.

## Goldcoin indexer initial checkpoint (added 2026-08-22)

**The problem this solves**: a brand-new `service.db_path` ledger has no
`goldcoin_indexed_blocks` rows at all, and `goldcoin::indexer::Indexer`
has always started a ledger in that state at height 0
(`Ledger::goldcoin_chain_tip() == None => start at 0`) — correct and
harmless against a regtest/testnet chain a few hundred blocks tall, but a
real launch blocker against the live production chain (~2.58M blocks at
time of writing): at this indexer's current per-block RPC rate, a full
resync from 0 would take many hours before the bridge could accept its
first deposit against the new reserve vault
(`ML79m57inAWBeqWfrXxXpi7ncA74k49GJa`). Goldcoin 0.15 does not support
`scantxoutset`, so there is no way to shortcut this by having the node
itself scan history for us.

**What the checkpoint means, precisely**: configuring one asserts "every
Goldcoin deposit *before* this height is intentionally outside the
bridge's supported history" — deposits *at* the checkpoint height itself
are indexed completely normally, exactly like any other block. This is
not a performance shortcut that might miss something; it is a stated,
operator-verified policy boundary. It is also consulted **only once**: the
moment the ledger has any indexed block at all (including right after a
checkpoint is first accepted), the checkpoint config is never looked at
again — the normal persisted cursor and reorg-detection logic always wins
from then on, exactly as it always has (`service/src/goldcoin/
indexer.rs::Indexer::tick`, `bootstrap_from_checkpoint_or_genesis`'s own
docs).

**Safety guard — do not skip this step.** Because Goldcoin 0.15 cannot
`scantxoutset` its own history, this service has no way to independently
verify that the configured vault never received a bridge deposit before
the checkpoint height — that is a claim only an operator can make, from
knowing the vault's real provenance (e.g. it is a freshly generated
address that has never appeared in any bridge configuration before this
launch). `initial_checkpoint_operator_acknowledged_no_prior_deposits`
exists specifically to make that claim explicit and machine-checked
(`false`, including simply leaving it unset, fails the whole checkpoint
closed — see the malformed-config behavior below); it is never inferred,
guessed, or defaulted to `true`.

### Exact operator procedure

1. **Get the live tip height** — run this against the SAME node the
   `[goldcoin].rpc_url` in the config file being commissioned will
   actually point at, not just any node claiming to be Goldcoin mainnet:
   ```
   goldcoin-cli getblockcount
   ```
2. **Pick a checkpoint height** at or below that tip. Using the tip
   itself is fine but leaves zero reorg buffer against the last few
   blocks; subtracting a modest safety margin (e.g. a few hundred blocks,
   comfortably above `max_reorg_depth`) is more conservative and is what
   was actually done for this vault's launch.
3. **Get that height's block hash**:
   ```
   goldcoin-cli getblockhash <HEIGHT>
   ```
4. **Verify the block independently** before trusting it — do not simply
   copy the hash from step 3 straight into config without looking at it:
   ```
   goldcoin-cli getblock <HASH>
   ```
   Confirm the returned `height` field matches what was requested, and
   that `confirmations` is comfortably above `max_reorg_depth` (a
   too-recent block is a poor checkpoint choice — see step 2).
5. **Confirm the vault has no prior bridge history.** This is the one
   step this service cannot do for you (no `scantxoutset` on Goldcoin
   0.15) — confirm from the vault's own provenance (e.g. it was generated
   fresh for this launch and has never been configured as a bridge vault
   before) that it received no bridge deposit before the chosen height.
6. **Configure height, hash, and the explicit acknowledgement together**
   in the service config file (`service/config.pilot-template.toml`'s
   commented-out `[goldcoin]` block shows the exact field names):
   ```toml
   initial_checkpoint_height = <HEIGHT from step 2>
   initial_checkpoint_hash = "<HASH from step 3/4>"
   initial_checkpoint_operator_acknowledged_no_prior_deposits = true
   ```
   All three must be set together — a partial pair (e.g. height without
   hash) is rejected at config-load time, before the daemon ever starts,
   never silently ignored or treated as "no checkpoint".
7. **Start the daemon.** Its first tick re-verifies the configured hash
   live (`getblockhash(height)`, exact byte-for-byte comparison — never
   trusting the config file alone) before indexing anything; a
   mismatch, an above-tip height, a malformed hash, or a missing
   acknowledgement all refuse the tick outright rather than silently
   falling back to height 0. Watch the first tick's log for the
   `"Goldcoin indexer verified an operator-configured initial
   checkpoint"` line confirming acceptance.

**This is a one-time procedure per ledger.** Once step 7's first tick has
indexed anything, the ledger is no longer "brand new" and this whole
config block is permanently irrelevant to it, even if left in the config
file — restarting the daemon, or ever re-running this procedure's steps
against the same `service.db_path`, has no effect once past that point.

## Reserve sizing

Per management's stated principle: **reserve levels should cover the largest expected net outflow between operational rebalances.** Concretely:

```
target_reserve(direction) =
    expected_peak_directional_volume_per_rebalance_interval
  + safety_margin
  + protected_minimum
```

`expected_peak_directional_volume_per_rebalance_interval` and `safety_margin` are operational judgment calls informed by observed volume once the bridge is live; no value is asserted here. `rebalance_interval` itself is a policy choice (fixed schedule vs. threshold-triggered) — see [12-management-decisions.md](12-management-decisions.md).

**`protected_minimum` for the pilot launch is approved: 20,000 GLC** (raw `20000000000`, 6 decimals) — see [22-production-readiness-review.md](22-production-readiness-review.md) P0-6's "Approved pilot bridge-policy parameters" for the full pilot policy table and where each value is consumed (this is the same value passed to `initialize` via `glc-mainnet-bootstrap --protected-minimum`). This is the on-chain floor releases are refused below — it does **not** by itself resolve the formula above: `target_reserve`/`warning_reserve`/`critical_reserve` (the off-chain service's own `reserve.{solana,goldcoin}` config) still need real expected-volume data before `expected_peak_directional_volume_per_rebalance_interval`/`safety_margin` can be set, and remain open (docs/12 item 5).

**Update 2026-08-21: exact pilot initial-funding plan set.** Planning
reference price: 1 GLC = $0.002160. Reserves split ~equally, ~$400 total
intended exposure:

| Reserve | Planned initial funding | Approx. value |
|---|---|---|
| Goldcoin L1 reserve | **92,600 GLC** | ~$200.016 |
| Solana GLC reserve | **92,600 GLC** | ~$200.016 |
| **Total** | **185,200 GLC** | **~$400.032** |

($200 / $0.002160 = 92,592.592593 GLC; rounded up to 92,600 GLC per side
for operational simplicity.) **This replaces the previous 200,000-GLC-
per-side pilot planning figure.** Still a plan, not funding that has
happened — nothing has been transferred yet.

**Update 2026-08-21: `service/src/config.rs`'s `reserve.{solana,goldcoin}.{target_reserve,warning_reserve,critical_reserve}` — pilot placeholder values set, resolving the previously-open item and the daemon-startup blocker it caused.** These are the off-chain reserve-ledger monitoring bands (docs/05-reserve-accounting.md's Normal/Warning/Critical/Floor-breach table), distinct from — but required to be consistent with — the on-chain `protected_minimum`. Since no real observed pilot volume exists yet, these are conservative, simply-reasoned placeholders sized directly off the approved 92,600 GLC reserve, not the `expected_peak_volume + safety_margin` formula above (that formula's inputs remain genuinely open, per docs/12 item 5, until real volume data exists — these placeholders are what let the daemon start in the meantime, not a claim that real volume analysis has been done).

**Critical unit-conversion note, checked directly against the code, not assumed:** the two `reserve.*` sections are **not** in the same raw-unit convention. `reserve.solana.*` amounts are raw SPL-token units, 6 decimals (`amount × 1,000,000` — same convention as the on-chain `protected_minimum`). `reserve.goldcoin.*` amounts are raw native-Goldcoin atomic units, **8 decimals** (`amount × 100,000,000` — confirmed against `service/src/goldcoin/deposit.rs::glc_to_atomic`, the same conversion the indexer itself uses for every real deposit it observes). The same GLC quantity is therefore a *different* raw integer on each side — using the 6-decimal conversion for the Goldcoin side (or vice versa) would silently misconfigure the reserve bands by two orders of magnitude without any error, since both are just `u64` fields to the parser.

| GLC amount (both sides) | `reserve.solana.*` raw (×1,000,000) | `reserve.goldcoin.*` raw (×100,000,000) | Reasoning |
|---|---|---|---|
| `protected_minimum` = 20,000 GLC | `20000000000` | `2000000000000` | Mirrors the approved on-chain floor exactly (P0-6) — the off-chain band must agree with the hard on-chain floor, not invent a different number. |
| `critical_reserve` = 30,000 GLC | `30000000000` | `3000000000000` | 10,000 GLC (one full max-transfer) of buffer above the hard floor before the auto-pause band engages — small but non-zero headroom, appropriate for a reserve this size. |
| `warning_reserve` = 100,000 GLC | `100000000000` | `10000000000000` | Set equal to the approved rolling 24h volume cap: if the reserve drops to the size of one full day's *legitimate maximum* volume, that's a reasonable, easy-to-explain point to start planning a rebalance. |
| `target_reserve` = 92,600 GLC | `92600000000` | `9260000000000` | The full initial funded amount — with no real volume history yet, "rebalance back to what we started with" is the simplest defensible target, not an invented number. |

`reconciliation_tolerance`: **0** (raw units) — no tolerance for unexplained drift at pilot scale; any discrepancy at all should surface, not be silently absorbed, matching the reconciliation design's own fail-closed intent (docs/05-reserve-accounting.md, docs/10-threat-model.md).

A checked-in template reflecting these exact values is at
[`service/config.pilot-template.toml`](../service/config.pilot-template.toml)
— every reserve-bounds/network/confirmation-depth field is a real
pilot value; every identity/endpoint field (RPC credentials, admin/
attestation/vault pubkeys, key paths) is an explicit
`<REPLACE_WITH_...>` placeholder, never a real or invented key. See
that file's own header comment for exactly what must be supplied
before it can be used for a real deployment, and "Attestation
signer provenance" below for the attestation-pubkey placeholders
specifically.

**One additional field this exercise surfaced, not previously
addressed in any confirmation-depth approval:**
`goldcoin.required_payout_confirmations` (consumed as
`required_goldcoin_confirmations` in `glc-bridge-daemon.rs`) — how
many confirmations *our own outgoing* Goldcoin payout needs before
being treated as settled, the outgoing-leg sibling of the incoming
`confirmation_depth`. This was never set previously (test fixtures
only ever used `3`, explicitly non-production). **Proposed pilot
value: 200 — the same conservative depth as `confirmation_depth`**,
for the same reason (no real Goldcoin hashrate/reorg data reviewed
yet) applied symmetrically to the outgoing leg. This is a new
value, not one of the three previously-approved confirmation
settings (`confirmation_depth`/`max_reorg_depth`/
`vault_min_confirmations`) — flagged here for explicit sign-off
rather than silently folded into "unchanged."

**Update 2026-08-21: recalculated and approved.** `protected_minimum`
and `rolling_volume_limit` were sized against the old 200,000-GLC-
per-side plan and have been replaced with values recalculated against
the new 92,600-GLC-per-side plan:

- **`protected_minimum`: 50,000 GLC → 20,000 GLC** — roughly the same
  proportion of the reserve as before (~21.6% vs. ~25%), rounded down
  slightly to leave real usable liquidity for a reserve this small.
- **`rolling_volume_limit`: 100,000 GLC/24h → 50,000 GLC/24h** — the
  old value exceeded an entire single-side reserve outright and could
  never actually bind; the new value sits under the resulting usable
  liquidity while still being a real constraint.

Resulting usable/releasable liquidity per side: 92,600 − 20,000 =
**72,600 GLC**. Resulting max full-size (10,000 GLC) transfers per
rolling 24h: 50,000 / 10,000 = **5**. `min_transfer_amount` and
`per_transfer_limit` are unchanged. See
[22-production-readiness-review.md](22-production-readiness-review.md)
item 28 and P0-6 for the full reasoning.

**Update 2026-08-22: `rolling_volume_limit` raised to the approved
pilot value of 100,000 GLC/24h (raw `100000000000`), GLOBAL and PER
DIRECTION — the same single `rolling_volume_limit` field bounds both
Goldcoin→Solana and Solana→Goldcoin volume, each tracked in its own
`RollingVolumeWindow` (see `programs/glc-reserve-bridge/src/limits.rs`);
there is no separate per-direction field to configure.** This is a
volume cap, not a transaction-count cap — a user may still bridge any
valid amount up to `per_transfer_limit` (10,000 GLC, unchanged) per
transfer, and successive transfers accumulate toward the 100,000 GLC
rolling-24h ceiling per direction. `protected_minimum` (20,000 GLC),
`per_transfer_limit` (10,000 GLC), `min_transfer_amount` (100 GLC), and
`rolling_window_seconds` (86,400) are all unchanged. `warning_reserve`
(table above) is raised to 100,000 GLC alongside it, per this section's
own "set equal to the approved rolling 24h volume cap" rule — a reserve
monitoring band, not a change to `target_reserve`/`protected_minimum`/
`critical_reserve` or to the actual funded reserve amount, none of
which moved.

Worth stating plainly rather than silently omitting: resulting
usable/releasable liquidity per side is still 92,600 − 20,000 =
**72,600 GLC** (unchanged, since neither the reserve plan nor
`protected_minimum` moved) — so, exactly as the 2026-08-21 update above
noted about the *original* 100,000 GLC value, this cap again exceeds
this reserve's own usable liquidity per side and is unlikely to ever
actually bind before usable liquidity itself becomes the limiting
factor at the current 92,600-GLC-per-side reserve size. Recorded here
as the approved policy value regardless, per explicit pilot-policy
sign-off — not a claim that it is the binding constraint at today's
reserve size.

**Update 2026-08-29: `rolling_volume_limit` raised on-chain to 500,000
GLC/24h per direction (raw `500000000000`) in production.** Applied via
the supported `glc-admin set-limit --field rolling-volume` path; nothing
in this repository hardcodes the value. The live
`BridgeConfig.rolling_volume_limit` read (`glc-admin show-config`, the
admin control plane's `GET /onchain`, and the public `GET /limits`
projection) is ALWAYS the authoritative current value — the historical
figures in the updates above are policy history, not current
configuration, and no dashboard or document should present them as
today's limit.

## Confirmation-depth values (pilot, approved 2026-08-21)

**These are the actual values to put in the pilot's Goldcoin config
section — not a placeholder, not "TBD."** They are a deliberately
conservative, hand-picked interim choice for the bounded pilot
specifically, made **without** the real Goldcoin hashrate/historical
reorg-depth data docs/12 item 4 calls for — that data collection remains
open and is now explicitly a scale gate (see
[22-production-readiness-review.md](22-production-readiness-review.md),
"Pilot Launch Policy"). The reasoning for picking a number now rather
than waiting: the pilot's settlement speed does not matter at this
volume, so there is no cost to erring far on the side of caution, and an
explicit conservative number closes the one real up-to-the-reserve
attack mechanism (an under-confirmed deposit reorged out after the
Solana-side release already happened) that the earlier "no default"
stance otherwise left open indefinitely.

| Field | Pilot value | Reasoning |
|---|---|---|
| `confirmation_depth` (Goldcoin deposit finality — the security-critical one) | **200** | Chosen with a large margin over what a well-hashrate-secured chain would need, specifically because this repository has not reviewed Goldcoin's actual real-world hashrate/reorg history. Trades settlement latency for safety margin; acceptable because pilot volume/urgency is low. |
| `max_reorg_depth` (reorg-walk safety valve — halts rather than silently reconciling past this) | **250** | Set above `confirmation_depth` so the indexer can actually walk back and resolve an ordinary reorg approaching the finality depth, rather than hard-halting on anything close to 200; still bounded, so a reorg deeper than 250 correctly halts the indexer and pages an operator instead of being silently absorbed. |
| `vault_min_confirmations` (payout-side vault UTXO spendability) | **20** | Governs the bridge's *own* change/reserve outputs, not an external depositor's — a reorg here is an operational hiccup (resubmit), not a loss-cap issue, so it does not need `confirmation_depth`'s margin; still well above the `1`–`3` values used only in test fixtures. |

**These are pilot-interim values, not the final production numbers.**
Replacing them with values backed by real Goldcoin hashrate/historical
reorg data (docs/12 item 4) is required before reserves, limits, or
usage are increased past the pilot — it is a scale gate, not a pilot
launch blocker; see "Pilot Launch Policy" in
[22-production-readiness-review.md](22-production-readiness-review.md)
for the full reasoning. Update this table (and the deployed config) when
that data-driven pass happens — do not silently carry these numbers
forward into a scaled deployment.

## Attestation signer provenance (checked 2026-08-21)

**Status: UNCONFIRMED — human decision required before deployment.**
Three attestation pubkeys appear in this codebase
(`6b27qC3fxrReuU4hL6u8iZ9AwkdngnjDxXUPwicR8WLe`,
`G7dJ2HiEkcfJqtPGa8gQrErLaQfdZ7hcbnA173A8Y4yL`,
`4uYKxwpWrPDyoaxjmdmJoWYLxmq2AziNMctSjTDFmynT`), but only ever inside
one illustrative example bootstrap command (duplicated between
`docs/22-production-readiness-review.md` and
`service/src/bin/glc-mainnet-bootstrap.rs`'s own module doc comment).
No separate provenance/custody record anywhere in the repository
confirms these as real, intended production attestation signers rather
than an illustrative placeholder set. Both example commands now use an
explicit `<REPLACE_WITH_ATTESTATION_PUBKEY_N>` placeholder instead of
these three literals, so nothing is accidentally copied as if real.

**Before the real `glc-mainnet-bootstrap` invocation, supply:**
- 3 real production attestation pubkeys (2-of-3 threshold, per the
  approved pilot policy) — the signers authorizing GLC⇄SOL settlement.
- 3 real production Goldcoin vault pubkeys (2-of-3 threshold) — the
  payout-side custody signers.
- The real production admin pubkey.
- The real production submitter/fee-payer keypair (not a custody
  authority — see `Config::load_submitter`).

None of these were invented, guessed, or filled with placeholder/test
values anywhere production code or documentation reads from. Private
key material for any of the above is never generated or held by this
repository — only public keys are ever configuration inputs.

## Threshold bands and responses

| Band | Condition | Automatic response | Operator action |
|---|---|---|---|
| Normal | `balance ≥ warning_reserve` | None | None |
| Warning | `critical_reserve ≤ balance < warning_reserve` | Alert fired | Plan a rebalance; no urgency |
| Critical | `balance < critical_reserve` (but `≥ protected_minimum`) | **Automatic directional pause** (new requests rejected on that direction; in-flight requests continue to settle) | Execute rebalance before unpausing |
| Floor breach | `balance` would drop below `protected_minimum` for a specific request | Request rejected at capacity-check time (never accepted in the first place — see [05-reserve-accounting.md](05-reserve-accounting.md)) | None routine; investigate if this triggers unexpectedly, since it implies capacity accounting drifted from the live balance |

## Rebalancing procedure

The off-chain engineering layer (state machine, approvals, execution-evidence recording, confirmation, structural separation from settlement accounting) is built and executable today via the `glc-admin rebalance-status`/`rebalance-list`/`rebalance-propose`/`rebalance-approve`/`rebalance-reject`/`rebalance-cancel`/`rebalance-record-executed`/`rebalance-confirm`/`rebalance-fail` commands above. **What's still a manual, out-of-system step is the real fund transfer itself** — this service never constructs, signs, or broadcasts one; step 4 below is performed entirely through whatever real Goldcoin/Solana wallet or custody tooling already holds the relevant keys, same as any other operator-initiated transfer, and only its evidence is recorded here.

1. Operator determines direction and amount needing rebalance — `glc-admin rebalance-status --db PATH` gives a read-only severity assessment (Normal/Warning/Critical) and a suggested deposit size against the operator's own configured `target_reserve`, computed from already-configured values only, never an invented one.
2. Stage the rebalance: `glc-admin rebalance-propose --db PATH --direction ... --kind <deposit|withdraw> --amount N --by IDENTITY --required-approvals N --note TEXT`. Creates a `Proposed` request — moves no funds, touches no settlement accounting.
3. Required custody-domain approvals collected: `glc-admin rebalance-approve --db PATH --id N --by IDENTITY`, once per approving identity, until `required_approvals` is reached (per the ratified trust model — e.g. 2-of-3 for whichever reserve is being topped up). The request moves to `Approved`.
4. **Execute the real transfer entirely outside this system**, through the real custody tooling for the relevant reserve (Goldcoin vault multisig / Solana reserve-authority-adjacent wallet), then record the resulting evidence: `glc-admin rebalance-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT` (a Goldcoin txid or Solana signature). `tx_reference` is unique across every rebalance ever recorded — the same real transfer can never be recorded twice.
5. Once the real transfer is independently confirmed (on-chain/on-Goldcoin), record it: `glc-admin rebalance-confirm --db PATH --id N --by IDENTITY --observed-amount N`. This updates the cached `total_reserve_balance` in the same step, so the very next reconciliation tick sees an already-explained balance rather than misclassifying the confirmed, operator-authorized change as a breach. If the rebalance clears `critical_reserve`, any reserve-triggered pause on that direction still requires an explicit `glc-admin unpause`/`glc-admin onchain-unpause` — reconciliation and rebalancing never auto-clear a pause (see below), regardless of how healthy the balance now looks.
6. If the recorded transfer's effect is never confirmed, or is confirmed wrong: `glc-admin rebalance-fail --db PATH --id N --by IDENTITY --note TEXT` routes it to manual resolution rather than leaving it `Executed` indefinitely.

## Vault UTXO splitting (added 2026-08-24)

### Why this exists

`goldcoin::coin::select` already prefers a bounded combination of smaller mature vault UTXOs over one oversized one when a smaller combination exists (the fix for the incident where a ~9,900 GLC payout consumed the vault's one ~100,000 GLC UTXO — see the "Reserve sizing" section above and `docs/22-production-readiness-review.md`). That fix cannot manufacture liquidity that doesn't exist: if the vault's mature UTXOs are still concentrated in one very large one, a payout still has no choice but to consume it, producing a large immature change output and temporarily starving spendable reserve below `protected_minimum` — exactly the scenario that recurred in production even after the selection fix (request #18: a ~90,100 GLC UTXO consumed for a ~9,900 GLC payout, mature reserve dropping below the 20,000 GLC floor and auto-pausing again).

`glc-admin split-vault-utxo` answers this proactively: an operator, having noticed (via `glc-admin status`'s `immature_vault_utxo_total`/vault UTXO inspection, or after an incident like the one above) that the root vault's mature liquidity is concentrated in one disproportionately large UTXO, can fragment it into several smaller ones ahead of the next large-vs-small payout collision — all before any payout is even attempted.

### What it does, and does not, change

- Uses the exact same 2-of-3 vault signer path (`VaultSigner`, `crate::goldcoin::multisig::assemble`) every real payout uses — `DevVaultSigner` in dev/pilot mode, `RemoteVaultSigner` in production mode (`operators.mode` in the service config). Signer secrets/private keys never enter this command's process; production signing happens entirely on the remote signer's own side, identical to `retry-goldcoin-payout` and the orchestrator's own payout path.
- Every output of a split pays the vault's own script — never a derived per-request address, never an external destination. Splitting is scoped to root-vault UTXOs only; a per-request derived deposit-address UTXO is refused (it's already narrowly scoped to one funding request).
- Does not touch `vault_min_confirmations`, the hard reserve invariant `reconcile()` enforces, or `coin::select` itself. It only ever adds more, smaller mature UTXOs for that unchanged selector to choose from later.
- Idempotent and auditable: a dedicated `vault_utxo_splits` table (`UNIQUE(source_txid, source_vout)`, `Built -> Signed -> Broadcast` state machine, mirroring `goldcoin_payouts`) means a given outpoint can be split at most once, structurally — re-running the command against an already-split outpoint is a safe no-op, reported and left alone.
- Every one of the (2 of 3) signers independently re-derives the entire plan — amount, chunk count, chunk sizes, and the reserve-safety check below — from its own ledger view before contributing a signature (`crate::signing::goldcoin_split::LedgerSplitSource`), the same "never trust a handed-in plan" discipline every other fund-moving operation in this service already has.

### The reserve-safety check (unconditional, no override)

Splitting spends the source UTXO's full value; until every resulting output re-matures (`vault_min_confirmations`), that value briefly leaves spendable/mature reserve — the exact mechanism that caused the incident above. Before ever contacting a signer, and again independently by each signer:

```
mature_reserve_after = current_mature_reserve_balance - source_utxo_amount
refuse unless mature_reserve_after >= protected_minimum + pending_obligations
```

This is the same formula `reconciliation::reconcile` enforces reactively (see "Reserve sizing" above), checked here proactively. There is no `--force` or other override — if liquidity is too tight to safely split right now, the answer is to wait or replenish reserve first, not to bypass the check.

### Exact operator procedure

1. Identify the oversized UTXO — `glc-admin status --db PATH` for a quick reserve overview, or direct `vault_utxos` inspection for the specific `txid`/`vout` and amount. There is no auto-pick; the operator must name the exact outpoint.
2. Dry run: `glc-admin split-vault-utxo --config PATH --txid TXID --vout N --note TEXT` (no `--execute`). Prints the full plan — source UTXO, output count, each output's amount, total fee, and the reserve-safety check (current mature reserve, protected minimum, pending obligations, mature reserve after the split, and PASS/FAIL) — without contacting any signer or broadcasting anything.
3. Review the printed plan. A `FAIL` safety check refuses regardless of `--execute`; wait for reserve to recover (a rebalance deposit, or a prior split's outputs maturing) before retrying.
4. Execute: re-run the identical command with `--execute` appended. Prints the same plan first, then contacts the configured vault signers, assembles and broadcasts the transaction, and prints the resulting txid.
5. Re-running the same command again (with or without `--execute`) after a successful split is a safe no-op — the source outpoint is already recorded in `vault_utxo_splits` and is reported, not re-processed.

`--chunk-target-atomic` defaults to 1,250,000,000,000 (12,500 GLC, 8 decimals) — originally chosen with headroom over an earlier per-transfer limit so a single resulting chunk could always individually cover the largest possible payout via `coin::select`'s cheap single-UTXO paths without needing a multi-input combination. **As of the 2026-08-29 limit raise (`per_transfer_limit` = 20,000 GLC gross, 19,400 GLC maximum net payout at the 3% fee), a single 12,500 GLC chunk no longer covers a maximum-size payout — two chunks do (2 x 12,500 = 25,000 > 19,400 + tx fee), which is still a cheap 2-input selection, but re-tuning this default for the new maximum is an open operational decision (same sign-off process as the incident-era tuning below), deliberately not folded into the limit raise itself. Revisit this default if `per_transfer_limit` (on-chain, `glc-admin set-limit --field per-transfer`) ever changes materially again.** Every output is required to be at least 1,000 GLC (`goldcoin::split::MIN_CHUNK_FLOOR_ATOMIC`) — a UTXO too small to produce at least 2 useful chunks at the requested target is refused outright (`SplitError::NotWorthSplitting`/`ChunkBelowFloor`), rather than producing a fragment too small to matter.

### Recovery from a split stuck in Signed (fixed 2026-08-27)

Real production incident: two splits reached `Signed` (a valid `signed_tx_hex` persisted) but their broadcast attempt never got a definitive answer from the node — `transport error contacting Goldcoin RPC: error decoding response body` — leaving `txid`/`broadcast_at` `NULL` and the source UTXO still `Available`. Before this fix, every later re-run of `split-vault-utxo --execute` for that same outpoint was a guaranteed no-op forever: the idempotency check found the existing row and reported "already split, nothing to do" without ever looking at its state.

`split-vault-utxo --execute` now recovers automatically: finding an existing `Signed` row for the requested outpoint re-submits the EXACT stored `signed_tx_hex` (`goldcoin::split_recovery::recover_stuck_vault_utxo_split`) — never rebuilt, never re-signed, no new signer round-trip. This is deliberately the opposite of `payout_recovery`'s (Goldcoin payout) recovery, which always re-signs — that module recovers from a signature-canonicalization *rejection*, where the stored bytes are the suspected cause; this one recovers from a *transport* failure that never rendered a verdict on the transaction at all, so the signed bytes are presumed fine. Without `--execute`, a `Signed`-but-unbroadcast split is reported, not acted on. `sendrawtransaction`'s "already known"/"already in mempool" (`-26`, specific known messages only, never the generic code) and "already in chain" (`-27`) responses are both treated as success, same as a fresh accept; "missing inputs" (`-25`) is still refused as a conflict needing operator investigation. The recovered txid is always computed independently from the exact submitted bytes (`goldcoin::tx::txid_of_serialized`), never trusted from the RPC's own reported string. The structural `UNIQUE(source_txid, source_vout)` protection is untouched — this path only ever reads the existing row and moves it `Signed -> Broadcast` via the same idempotent `Ledger::record_vault_utxo_split_broadcast` a fresh split already used, never a second `INSERT`.

Both the fresh-build broadcast call and the recovery resubmit call are now wrapped in a bounded 3-attempt retry (`goldcoin::rpc::call_with_retry`), so an isolated transient blip resolves within a single command invocation rather than requiring a manual re-run. `RpcClient::call`'s own diagnostics were also improved to distinguish a genuine transport failure from a response that arrived but failed to parse as JSON — the latter now includes the HTTP status and a body snippet (with any run of 40+ hex characters redacted, so a misbehaving proxy reflecting a submitted signed hex back in an error page can never leak it) instead of the old, undiagnosable "error decoding response body" with no further detail.

Recovering a split still in `Built` (never signed at all) is explicitly out of scope for this path — refused with a clear message, not silently attempted; that would require re-signing, which this module deliberately never does.

### Split outputs and the indexer (fixed 2026-08-25)

A split transaction's outputs all pay the vault's own script with no OP_RETURN, by construction — indistinguishable, to `goldcoin::indexer`'s legacy request-binding check (built for GlcToSol deposit attribution, unrelated to splits), from an unexplained vault payment. Before this fix, every split output was recorded in `unmatched_goldcoin_deposits` with `reason = 'no_request_binding'`, a false alarm: `vault_utxos`/reserve capacity were never actually wrong, since those are populated separately by `Orchestrator::tick_vault_utxos` (`list_unspent`-based), independent of this per-block scan.

The indexer now checks, for any vault-owned output with no usable request binding, whether `(txid, vout, amount)` exactly matches an expected output of a known `Broadcast` `vault_utxo_splits` transaction (`goldcoin::split::matches_expected_split_output`, reproducing the exact deterministic output distribution from the split's own persisted `source_amount_atomic`/`fee_atomic`/`chunk_count` — never re-derived from a possibly-since-changed `fee_rate_per_kb`). An exact match is logged and skipped, never recorded unmatched; anything else — a genuinely unexplained payment, or even a single mismatched output on an otherwise-real split — is recorded exactly as before.

A row recorded before this fix shipped stays recorded (never auto-cleaned by a rescan): `glc-admin reconcile-unmatched-deposit --db PATH --txid TXID --vout N --note TEXT` marks it reconciled, using the identical exact-match check, refusing (no override) if it doesn't match a known split output. Never deletes the row — reconciliation is additive (`reconciled_at`/`reconciliation_note` columns), preserving full audit history either way.

## UTXO liquidity (permanent fix, added 2026-08-26)

### The incident this closes

Vault UTXO splitting (above) reduces the odds of a large-UTXO/small-payout collision, but does not change what happens to the *change* a payout itself produces. Production still hit this: a ~95,000 GLC vault UTXO was manually split into 20 chunks of ~4,770 GLC. Traffic then consumed more than 20 of those mature chunks — one per Solana->Goldcoin payout — faster than their single, large change outputs could clear `vault_min_confirmations` (6). Funds were never at risk (every atomic unit was accounted for, either spent to the destination or sitting in this service's own broadcast change), but the *mature, spendable* UTXO pool collapsed toward zero, `reconcile()`'s hard invariant tripped, and Solana->Goldcoin auto-paused. Manually pre-splitting more chunks only raises the number of payouts it takes to reproduce the same collapse — it does not change the shape of the problem: a payout was still building **one** oversized change output per transaction, so mature liquidity was always one confirmation-depth away from starving under sustained traffic.

### What changed

1. **Deterministic change fan-out** (`goldcoin::coin::finalize_fanout`, replacing `finalize` for real payout construction — `finalize` itself is untouched and still used by `glc-rebalance-withdraw`'s manual single-change flow). Instead of one large change output, a payout with meaningful leftover value splits it into multiple vault-owned outputs sized around `goldcoin.change_fanout_target_atomic` (production default: sized off the *current* 1,880 GLC maximum net payout, not the stale 10,000 GLC historical limit), capped at `goldcoin.change_fanout_max_outputs`. It reuses `goldcoin::split::distribute_evenly` — the same near-equal-integer-division formula `split-vault-utxo` already uses — rather than a second, inconsistent splitting implementation. The candidate output count is reduced (never increased) until every output clears `dust_threshold`, exactly generalizing `finalize`'s existing single-change dust behavior; the real fee for that exact output count is recomputed at each step, never assumed. Value is conserved exactly; every output pays the vault's own script; the algorithm is a pure function of already-independently-derived inputs, so all 2-of-3 signers reproduce a byte-identical transaction without coordinating.
2. **`PayoutPlan.change_atomic: u64` -> `change_outputs: Vec<u64>`** (with `total_change_atomic()` for the aggregate). `goldcoin_payout_change_outputs` (new table, schema v12) persists each output's amount and order; `goldcoin_payouts.change_atomic` is kept as the SUM for backward compatibility with existing queries.
3. **UTXO pool health accounting** (`Ledger::utxo_pool_health`) distinguishes, at read time, real spendable liquidity from value merely *waiting*:
   - **Reserve value** — `total_reserve_balance` (unchanged meaning).
   - **Mature spendable capacity** — `mature_available_atomic` / `available_utxo_count`: the exact pool `coin::select` draws from right now.
   - **Temporarily immature internal change** — `own_unconfirmed_change_atomic` / `unconfirmed_change_utxo_count`: value this service already knows is its own broadcast-but-immature payout change (any `vault_utxos` row whose `txid` matches a known `goldcoin_payouts` broadcast — the external destination output is never a watched address, so this match is unambiguous). Never counted as spendable capacity; also folded additively into `pending_destination_settlement_amount` so reconciliation's "unexplained drop" check stops seeing it as unexplained the moment a payout leaves `Broadcast` state, without weakening the hard invariant itself.
   - **UTXO liquidity** — the count-based figures above, a faster-reacting leading indicator than either value figure: the accounting can look healthy while the pool itself is down to one oversized UTXO.
   `glc-admin status` prints all four; `/health`'s Prometheus output exports each as its own `glc_goldcoin_utxo_pool_*` gauge plus a `glc_goldcoin_utxo_pool_warning` gauge — deliberately a gauge, never an `Invariant`, so a thin-but-self-recovering pool never flips `/health` to 503 or reads as "Goldcoin reserves disappeared."
4. **Admission backpressure before exhaustion** (`goldcoin.utxo_pool_min_available_count`, `reserve_ledger.utxo_pool_min_available_count`/`utxo_pool_warning_count`, set via `Ledger::set_utxo_pool_thresholds` at every daemon startup). `fold_sol_deposit` now also refuses to admit a new SolToGlc obligation — routing it to `ManualReview` with reason `utxo_liquidity_low_at_fold` (added to the resumable-reason allowlist, same as the three pre-existing fold-time reasons) — once live `available_utxo_count` would fall to or below the configured floor. `0` disables it (default off; no behavior change on upgrade). Recovers automatically once a payout's change matures back past `vault_min_confirmations` and `available_utxo_count` rises again — no operator action needed, mirroring the existing `admission_closed`/`paused` distinction: this is a *third*, independent, physical-liquidity-aware admission gate, not a replacement for either.

### Tuning `utxo_pool_min_available_count` — this is vault-shape-specific, not a universal constant

There is no single safe default: the floor must engage *before* `reconcile()`'s own hard invariant would trip, and that break point depends on the relationship between actual vault chunk size, per-payout net amount, and the protected minimum. Worked from the incident's own numbers (4,770 GLC chunks, 1,880 GLC maximum net payout, 20,000 GLC protected minimum, ~75,400 GLC initial slack above the floor): each single-UTXO payout removes one ~4,770 GLC chunk from the mature pool *and* commits ~1,880 GLC of `pending_obligations` against it, so the hard invariant's own survival limit is `floor(75,400 / (4,770 + 1,880)) = 11` payouts fully admitted — count-based backpressure at the shipped default of 8 free chunks remaining would only start blocking at payout 12, one *past* that break point.

**This was empirically verified, and has since been fixed at its root cause** — see "PR #35 maintainer-review fixes" below. `service/tests/utxo_liquidity_production_tuning.rs::test_prod_defaults_floor_8_no_longer_breaches_thanks_to_the_sticky_pause_fix` (originally named `..._floor_8_breaches_before_backpressure_engages`, before the fix) still runs the literal historical shipped default (`utxo_pool_min_available_count = 8`, `vault_min_confirmations = 6`, `fee_rate_per_kb = 100_000` — the real pilot-template fee rate, not a toy value) against this exact vault shape, but now proves the opposite of what it originally found: obligations 0-11 (12 total) are admitted and finalized, and reconciling before obligation 12 no longer finds a breach — the shortfall (observed mature balance 38,160 GLC vs. protected_minimum 20,000 + pending_obligations 22,560) is fully explained by known internal change, so `reconciliation::reconcile`'s hard invariant holds and the direction never pauses. Count-based backpressure now gets to be the thing that actually engages at index 12, correctly reported as `utxo_liquidity_low_at_fold`, never masked by a spurious `reserve_paused_at_fold`.

`service/tests/utxo_liquidity_production_tuning.rs::test_prod_defaults_recovery_after_maturity_diagnostics` confirms the flip side: against a correctly-sized pool (9 UTXOs, same 20,000 GLC protected minimum), `floor = 8` engages exactly at the floor with the correct `utxo_liquidity_low_at_fold` reason, well clear of the hard invariant, and admission recovers automatically the moment change matures — so `8` is not universally wrong, only for a vault carrying enough *total* balance relative to its protected minimum to let many admissions through before the count-based floor would matter. **Recompute the floor for the vault's actual current total mature balance and chunk size, not just its chunk size in isolation**, before trusting the shipped default; `goldcoin.utxo_pool_warning_count` should sit comfortably above `utxo_pool_min_available_count` so an operator sees the warning gauge well before backpressure itself engages.

**Final recommendation (2026-08-26):** for a vault currently shaped like the incident (many ~4,770 GLC chunks against a 20,000 GLC protected minimum), configure `utxo_pool_min_available_count = 10` — not the shipped default of `8`, and higher than the `9` initially considered — deployed in `service/config.pilot-template.toml` and validated by `service/tests/utxo_liquidity_incident.rs`'s Tests A-D and by `service/tests/utxo_liquidity_production_tuning.rs::test_prod_recommended_floor_10_survives_the_25_burst_with_margin` (the same 25-obligation burst, at the real production fee rate): backpressure engages at obligation 10, leaving a full payout of margin before the hard invariant's own 11-payout survival limit, and the hard invariant never breaches. `utxo_pool_warning_count = 15` is kept as-is (it already sits comfortably above 10). The `2,500 GLC` change-fanout target and `change_fanout_max_outputs = 10` also stay as shipped: `test_change_outputs_for_a_very_large_97000_glc_input` shows the `change_fanout_max_outputs = 10` cap (not the target) is what determines output size once a UTXO is very large (~9,512 GLC per output on a ~97,000 GLC input, close to the original incident's own manually-split chunk size) — raising `change_fanout_max_outputs` would shrink those outputs further at the cost of a larger transaction; the target size itself is already correctly production-aware for the common case (see the same test file's `test_change_outputs_for_a_typical_4770_glc_input`, which produces exactly 2 change outputs of ~1,445 GLC each from a 4,770.8999317 GLC input).

**Separately:** `vault_min_confirmations = 6` was used in these tuning tests to match the incident's own stated assumption, but the deployed pilot template currently sets `vault_min_confirmations = 20` — an explicitly approved, reasoned pilot-interim value (see "Confirmation-depth values" above), not a placeholder. Changing it to `6` is a distinct decision from anything in this section and has not been made here; it needs the same explicit sign-off process that value's own guard comment calls for, not a silent edit alongside an unrelated liquidity-tuning change.

### PR #35 maintainer-review fixes (2026-08-26)

A maintainer review of this fix's own PR surfaced four findings, all fixed on the same branch before merge:

1. **Safe default.** `default_utxo_pool_min_available_count` (`service/src/config.rs`) shipped as `8` — the exact value this PR's own tests proved insufficient. Fixed to `10`, matching the "Final recommendation" above; `missing_utxo_liquidity_config_defaults_to_the_verified_safe_floor` (`service/src/config/tests.rs`) proves a config file predating this fix (none of the 4 new fields present) now loads with the safe value.
2. **Manual-review resume, and `open-admission`, must respect UTXO liquidity.** Both `Ledger::resume_manual_review_sol_to_glc` and `glc-admin open-admission` used to check only the value-based reserve invariant before letting more `SolToGlc` demand back in — never the count-based `utxo_pool_min_available_count` gate `fold_sol_deposit` applies to a brand-new obligation. An operator resuming a `utxo_liquidity_low_at_fold` request, or reopening admission, the moment value accounting looked sufficient — while the mature UTXO count was still at or below the floor — could re-admit exactly the demand backpressure exists to hold back. Fixed in both places: resume re-runs the identical count-based check first, refusing with a dedicated `LedgerError::UtxoLiquidityLow` (leaving the request untouched, reserving nothing); `open-admission` refuses with `LedgerError::UtxoLiquidityLowForAdmission` (also naming `own_unconfirmed_change_atomic`, so the error itself shows whether the "missing" liquidity is already known and en route to maturing) via the new `Ledger::check_utxo_liquidity_for_admission` — additive to, never a replacement for, the existing hard-invariant check. Both succeed normally the instant liquidity recovers, with no special-casing. See `service/tests/manual_review_resume_liquidity.rs` (Tests A-E) and `service/tests/open_admission_liquidity.rs` (Tests A-D). Solana admission is untouched either way — `check_utxo_liquidity_for_admission` is a no-op for `SolanaReserve`, and `cmd_admission` already refuses `--direction solana` before reaching either check.
3. **The sticky-pause path for explained internal change.** `reconciliation::reconcile`'s hard invariant (`observed_balance >= protected_minimum + pending_obligations`) only ever looked at the raw mature balance — so the exact chunk consumed to cover a much smaller payout could show up as an unexplained shortfall the instant reconciliation ran, even though every atomic unit was known, ledger-tracked, unconfirmed payout change, auto-pausing a reserve that was never actually short. Fixed: the hard invariant now adds `Ledger::own_unconfirmed_change_atomic` (GoldcoinReserve only; always `0` for SolanaReserve) to `observed_balance` before comparing against `protected_minimum + pending_obligations` — grounded entirely in independently-observed chain state matched against this service's own already-broadcast payouts, so it can never paper over genuine, unexplained loss. See `service/tests/utxo_liquidity_incident.rs`'s Test G.
4. **Signer/config-mismatch diagnostic.** `glc-bridge-daemon` now logs the effective `utxo_pool_min_available_count`/`utxo_pool_warning_count`/`change_fanout_target_atomic`/`change_fanout_max_outputs`/`vault_min_confirmations` at startup (`tracing::info!`), so an operator can directly diff what each independent signer instance actually loaded rather than only seeing an opaque stuck-payout/signing failure if two signers' configs silently drift. This is a visibility improvement only — the existing cryptographic signature-verification-at-assembly behavior that already fails closed on a real mismatch is unchanged.

### Batching — audited, deferred (not implemented in this pass)

Paying several finalized SolToGlc obligations in one Goldcoin transaction (one recipient output per obligation, fragmented vault change) would reduce UTXO churn further and was explicitly considered. Not implemented here: `goldcoin_payouts.request_id INTEGER PRIMARY KEY` structurally enforces exactly one payout per request throughout persistence, verification, and independent re-derivation — batching would need a genuine schema/protocol change (a payout-to-requests join, multi-request signer re-derivation, and multi-request replay/accounting semantics), not an incremental change alongside fan-out. Change fan-out + admission backpressure close the production incident on their own (see the regression tests below); batching remains a well-scoped, separately-reviewable follow-up rather than something to fold in here.

### Regression coverage

`service/tests/utxo_liquidity_incident.rs` reproduces the production incident directly, using `utxo_pool_min_available_count = 10` (the final recommended production value for this vault shape, not the shipped default): a burst of 25 consecutive 2,000 GLC gross obligations against a freshly-split pool (proving the service neither misclassifies reserve as unexplained-zero nor exhausts liquidity, applying backpressure before the hard invariant could ever trip instead); automatic admission recovery once change matures; several full maturity cycles with exact conservation and no permanent pause; randomized payout sizes with no double-spend and an always-preserved protected floor; two independent signers re-deriving a byte-identical multi-change transaction; and a daemon restart mid-fan-out reconstructing state correctly from Goldcoin RPC plus the ledger alone.

`service/tests/utxo_liquidity_production_tuning.rs` runs the same vault shape and fee-rate-realistic numbers against the HISTORICAL shipped config default (`utxo_pool_min_available_count = 8`, now superseded by `10` — `fee_rate_per_kb = 100_000`) to prove it no longer breaches or pauses post-fix (see "PR #35 maintainer-review fixes" above), confirms the mechanism works correctly on an appropriately-sized pool, validates the final `10` recommendation itself against the real production fee rate, verifies the real change-fan-out output shapes for both a typical ~4,770 GLC input and a very large ~97,000 GLC input, and verifies the production fee calculation at the real fee rate.

`service/tests/manual_review_resume_liquidity.rs` covers the resume-must-respect-UTXO-liquidity fix directly: a resume attempt refused while the pool sits at the floor; no duplicate obligation or payout created across repeated refused attempts; the triggering payout's change maturing and the pool recovering; resume succeeding normally once it does; and the protected reserve invariant never breaching throughout.

## Zero-conf payout change (added 2026-08-30)

Bridge-created payout CHANGE outputs are spendable for the next payout at
0 confirmations; everything else in the vault still waits
`vault_min_confirmations` (unchanged at its configured value). The
mechanics, in the order the guarantees stack:

- **Provenance is authoritative, never inferred.** A change output
  qualifies ONLY via its exact `(txid, vout)` row in
  `goldcoin_payout_change_outpoints`, written by
  `Ledger::record_goldcoin_payout_broadcast` in the same ledger
  transaction as the broadcast fact itself (change outputs are
  `outputs[1..]` of the payout; the destination is always output 0 and
  never gets a row, so a payout whose destination pays a watched script
  cannot be misclassified). Paying the vault script, or appearing in a
  vault-touching transaction, is NOT provenance. External deposits,
  vault-split outputs (`vault_utxo_splits` is a separate relation), and
  outputs broadcast before schema v14 all stay on the full-threshold
  policy — fail closed, no backfill.
- **Confirmed liquidity is always preferred.** Selection runs against
  confirmed (`Available`) UTXOs alone first; the 0-conf pool joins only
  when they cannot fund the payout (`signing::goldcoin_vault`'s
  two-phase selection, identical and deterministic across every
  independent signer).
- **Parent validation before use.** Each tick, before any payout
  building, the orchestrator re-checks every candidate's parent payout
  transaction against the live node (`getrawtransaction`). A parent the
  node no longer knows/accepts — evicted, conflicted, replaced, or an
  RPC failure, all treated identically — puts a persisted hold on its
  change (`vault_utxos.zero_conf_hold_reason`), honored by both the
  eligibility query and the reservation guard; re-acceptance clears it.
  A change output that disappears from `listunspent 0` entirely is
  marked Spent by the ordinary sync on the very next tick.
- **Chaining is capped — two modes** (`goldcoin.zero_conf_change_mode`,
  added 2026-08-30):
  - `"depth_limited"` (default): `goldcoin.zero_conf_change_max_depth`
    bounds the unconfirmed OWN-payout ancestor depth a 0-conf input may
    carry. At the shipped depth of 1, change whose only unconfirmed
    ancestor is its own parent payout is spendable, and the resulting
    payout's change records depth 2 — not spendable until a confirmation
    lands (chains stall every second generation). This mode is the
    rollback target for the recursive mode below.
  - `"bridge_owned_recursive"`: recursive reuse of VERIFIED
    bridge-created payout change — confirmed UTXO -> payout -> 0-conf
    change -> payout -> 0-conf change -> ... with no confirmations in
    between. The per-input cap becomes
    `goldcoin.zero_conf_change_recursive_chain_limit` (default 20,
    validated 1..=24), and selection additionally enforces a
    per-transaction budget: the sum of the selected still-unconfirmed
    inputs' recorded depths must stay within that same limit, so a
    constructed transaction always stays safely below the node's
    mempool chain policy (Goldcoin Core v0.17 `-limitancestorcount`:
    reject at 25 in-mempool ancestors including the new transaction,
    `too-long-mempool-chain`; the 101kB ancestor-size limit is orders
    of magnitude above these transactions). When a selection would
    exceed the budget, the deepest 0-conf candidate is dropped and
    selection re-runs (deterministically, so independent signers
    agree); if nothing within budget can fund the payout, the build
    fails closed for that tick and the request retries once a
    confirmation lands — a rejectable transaction is never constructed,
    and nothing is ever marked lost. Eligibility, provenance,
    parent-validation holds, and the reservation re-check are IDENTICAL
    to depth-limited mode: only authoritative payout change ever
    qualifies, at any depth.

  In BOTH modes `zero_conf_change_max_depth = 0` remains the kill
  switch that disables 0-conf change spending outright. Depth is
  recorded at broadcast and is an upper bound; from an output's first
  confirmation, its whole own-chain ancestry is buried and the caps no
  longer apply.

  **Deliberately unchanged by the recursive mode:** admission capacity
  (`fold_sol_deposit`) still follows the CONFIRMED reserve book only
  (reconciliation's >= `vault_min_confirmations` observed balance), and
  the `utxo_pool_min_available_count` floor still counts confirmed
  UTXOs only — under sustained demand, new SolToGlc obligations can
  still park in `ManualReview` while the payout engine is happily
  settling already-admitted ones from recursive change; they
  auto-resume as change confirms. Crediting
  `own_unconfirmed_change_atomic` into admission capacity would be the
  coherent next step if that latency matters, but it admits obligations
  against unconfirmed backing and is a separate, explicit decision —
  not bundled here.
- **Failure/recovery posture is unchanged.** A payout built on 0-conf
  change whose dependency later fails keeps every existing guarantee:
  one payout per request (the `goldcoin_payouts` PK),
  `retry-goldcoin-payout` re-derives the byte-identical transaction and
  reports `BroadcastConflict` if its inputs are genuinely gone — never a
  second, independent payout. Recovering the PARENT payout (its own
  normal recovery path) restores the child's inputs.
- **Operator visibility.** `glc-admin status` prints the 0-conf policy
  pool on its own line ("zero-conf payout change (policy candidates, not
  confirmed liquidity)"), separate from `mature_spendable_capacity` —
  0-conf change is never counted as confirmed reserve liquidity, never
  enters reconciliation's observed balance (still confirmed-only), and
  never satisfies the `utxo_pool_min_available_count` admission floor. A
  nonzero "on parent-validation hold" count deserves attention: a parent
  payout may have been evicted or conflicted.

## Automatic UTXO liquidity shaping (added 2026-08-30)

### The incident this closes

After the per-transfer maximum was raised from 2,000 to 20,000 GLC (2026-08-29), production SolToGlc payouts began repeatedly failing with `coin selection failed: selection would require more than 10 inputs`. Root cause was a **configuration mismatch the limit raise had explicitly deferred re-tuning** (see the former `default_change_fanout_target_atomic` doc comment): the mature pool was shaped into ~2,500 GLC chunks — `change_fanout_target_atomic = 250000000000` atomic units, sized for the *former* 2,000 GLC maximum — so a maximum 19,400 GLC net payout needed 9–10 of them, permanently riding the `max_inputs = 10` edge. Under sustained traffic, with each payout's own change out of the pool for the full `vault_min_confirmations` maturity window, the largest 10 *mature* candidates repeatedly summed below the target and selection correctly failed closed. This was **not a selector defect**: `goldcoin::coin::select`'s largest-first accumulation is feasibility-complete within `max_inputs` (if any `<= max_inputs` combination covers the target plus its own fee, the largest-`k` subset does — pinned exhaustively by `selection_never_reports_too_many_inputs_when_any_valid_combination_exists` in `service/src/goldcoin/coin.rs`). The failures were genuine infeasibility at 10 inputs against a mis-shaped pool, and the only remedy was manual, operator-run `glc-admin split-vault-utxo` — unacceptable for 24/7 operation.

### What changed

1. **`max_inputs` 10 -> 25** (`service/config.pilot-template.toml`) — the explicit, tested decision replacing the incident-day emergency edit. Cost quantified in `goldcoin::coin`'s test `twenty_five_input_transaction_size_and_fee_are_modest`: a worst-case 25-input 2-of-3 payout with 11 outputs is ~7.8 KB (far below relay/standardness ceilings) and costs ~0.0079 GLC at the production fee rate — noise against a 20,000 GLC payout. Shaping (below) keeps the pool chunked so this headroom is rarely needed, never routine.
2. **`change_fanout_target_atomic` 2,500 -> 5,000 GLC** (`default_change_fanout_target_atomic`, `service/src/config.rs`) — the deferred re-tune, made as its own reviewed decision: a maximum net payout now needs ~4 chunks (comfortable margin below `max_inputs`), while a typical smaller payout still finds a single covering chunk. This is also the chunk target automatic shaping splits to — one canonical payout-chunk size for the whole service.
3. **The split lifecycle** (`goldcoin::liquidity`, schema v16). Every split — automatic or CLI-initiated — moves through ONE persisted state machine: `Built -> Signed -> Broadcast -> Confirmed`, with `Abandoned` reachable from any non-terminal state. The load-bearing properties:
   - **`Built` is the CLAIM on the source outpoint**, written and validated (source must exist and be `Available`) in one transaction BEFORE any signer round-trip. From that commit, the source is excluded from payout coin selection (`available_vault_utxos`) and from the payout reservation guard (`reserve_vault_utxos` re-checks inside its own write transaction) — a payout and a split can never commit to the same UTXO, regardless of process interleaving (concurrent CLI + daemon included) or restarts.
   - **Broadcast bookkeeping is one transaction** (`Ledger::record_vault_utxo_split_broadcast`): split row -> `Broadcast`, source -> `Spent` (with `spent_by_txid` = the split's own txid), every chunk inserted as an `Unconfirmed` `vault_utxos` row — so `own_unconfirmed_change_atomic` (matching split txids in `Broadcast`/`Confirmed` as well as payout txids) explains the mature-balance dip with no crash window, and a split's fee joins payout fees in the permanent-departures term.
   - **`Broadcast` is driven, not abandoned to fate**: each tick, reaching `vault_min_confirmations` (via this service's own synced chain view) marks the split `Confirmed` (terminal) — the same depth the crate trusts outputs everywhere else, so a shallow reorg never orphans a split nothing maintains; a split the node no longer knows (mempool eviction, e.g. a node restart) is re-broadcast from its exact stored bytes. A transport-level RPC failure always defers to the next tick — an unreachable node never changes any state.
   - **Automatic abandonment only where provably safe; ambiguity defers to the operator**: a `Built` row (nothing signed yet) whose source is gone, or a fresh broadcast rejected outright, is auto-`Abandoned`. Every ambiguous case — a missing-inputs refusal (reorg races produce these transiently for transactions that later confirm), a `Signed` split the local node has forgotten, a transient floor refusal — is surfaced loudly (`lifecycle_error` in the tick report / CLI output) and DEFERRED: fully signed bytes are never walked away from automatically. Deferral is never silent masking, though: a `Broadcast` split persistently refused for missing inputs is FLAGGED (`missing_inputs_since`), and after a grace window (`Ledger::SPLIT_MISSING_INPUTS_GRACE_SECS`, 10 minutes) TWO things happen: the accounting terms stop explaining its phantom chunks, and — decisively, since the delta-based unexplained-drop detector can never re-fire for a drop it already explained at broadcast time — reconciliation raises an EXPLICIT dead-split alarm (`ReconciliationReport::dead_split_ids`): classification `Breach`, auto-pause, and a pause reason naming the split(s), regardless of how much solvency headroom the reserve has. A possible conflicting spend of vault funds always pauses and pages; it is never inferred from arithmetic that cannot see it. The flag clears itself the moment the node knows the transaction again. `glc-admin split-vault-utxo --abandon --execute` is the deliberate, per-outpoint release: for any split with signed bytes it derives the txid from the exact stored bytes (persisting it onto the row, so the re-adoption watch below covers `Signed` abandons too) and probes the node tri-state — refusing while the transaction is KNOWN, refusing (fail closed) when the node is UNREACHABLE, allowing only on a definitive "no such transaction"; `Confirmed` splits are never abandonable. Abandonment mutates NO reserve book (reconciliation's own per-tick refresh already converged the cached book when the split broadcast — a debit would double-count); the audit row is the permanent record. If the node reports the abandoned transaction within a 24-hour watch window (`liquidity::READOPT_WATCH_SECS`, a live probe — never stale ledger confirmation counts), lifecycle maintenance RE-ADOPTS the split automatically (state back to `Broadcast`, chunks re-entering the accounting and, at maturity, the pool); past the window an abandonment is terminal and chain-resurrected value is a reserve-custody runbook decision. The `Abandoned` audit row is kept forever while the partial uniqueness index (`WHERE state != 'Abandoned'`) releases the outpoint for a legitimate later split. Pending-split recovery visits EVERY `Built`/`Signed` row with per-row error isolation (never head-of-line blocking), and a deferring or erroring split never stops maintenance of other splits or new-split consideration; a never-broadcast `Signed` split re-checks the reserve floor against CURRENT state before its bytes ever reach the network, and its recorded chunk amounts are always byte-verified against the persisted unsigned transaction. No state can permanently wedge shaping, and no recovery path involves SQLite.
   - **Crash-window bookkeeping heals BEFORE reconciliation** (`goldcoin::liquidity::heal_split_bookkeeping`, run at the front of every orchestrator tick, per-row error isolation): a `Signed` split whose exact bytes the node already knows — a crash landed between broadcast acceptance and the ledger commit, in the daemon or in a concurrent `glc-admin` run — has its Broadcast bookkeeping recorded probe-only (never signing, never re-sending, so never a duplicate transaction) before any reconciliation pass could read the spent source as an unexplained loss and latch a false auto-pause. If ANY `Signed` row's status cannot be settled (node unreachable, verification refused, write failed), BOTH of the tick's Goldcoin reconciliation passes are skipped — recorded visibly as `SKIPPED`, retried next tick — rather than judged on books that may be mid-heal. Payout liveness has one more guard: a `Built` claim resumed after downtime is released (abandoned, safely — nothing was signed) when the remaining mature pool can no longer cover obligations admitted in the meantime, and a fresh split whose just-signed broadcast is refused for missing inputs stays `Signed` for resume — never auto-abandoned. All split lifecycle events (`lifecycle_error`, abandonments, re-adoptions, re-broadcasts, heals) are logged by the daemon at warn/info level every tick.
   - **Payout liveness outranks shaping, in both entry points**: a split (automatic or CLI) is deferred/refused — non-overridably — while removing its source from the mature pool would leave already-admitted obligations uncoverable; and a claimed source is excluded from admission backpressure counts and pool-health figures, never reported as spendable liquidity.
   - **One shaping tick** (`run_shaping_tick`, wired after the payout pass) = lifecycle maintenance plus at most one transaction-shaped action, in priority order: drive `Broadcast` splits; resume/abandon the oldest pending split; only then — while the payout-ready pool (mature Available UTXOs plus currently-eligible 0-conf payout change, each at half the chunk target or better) is below `utxo_shaping_target_available_count` and no previous split's chunks are still maturing — claim and execute one NEW split of the largest eligible root-vault UTXO (>= `utxo_shaping_min_source_atomic`, at most `utxo_shaping_max_outputs_per_split` chunks, never below the canonical chunk target).
   - **The CLI is the same code**: `glc-admin split-vault-utxo` calls the identical `goldcoin::liquidity` functions (`execute_fresh_split`, `resume_pending_split`, `maintain_broadcast_splits` — scoped to exactly the named outpoint's split, never an unrelated one) — it resumes a pending split instead of falsely reporting it done, and there is no CLI action that can strand lifecycle state. `--abandon --execute` is the operator-decided release valve for a not-yet-`Confirmed` split the automatic lifecycle cannot finish (e.g. the node permanently rejects its stored bytes for a policy reason): audit row kept, source outpoint released, no SQL.
   - **One problematic split never freezes shaping**: a lifecycle step that errors (an RPC surprise, an unrecognized node rejection) is surfaced as `lifecycle_error` in the tick report and the tick continues — maintenance of other splits and new-split consideration proceed. An unreachable node always DEFERS (nothing is abandoned, nothing recorded) rather than being read as "transaction unknown". A split is also deferred, never executed, while removing its source from the mature pool would leave already-admitted obligations uncoverable — payouts keep first claim on mature liquidity.
4. **Solvency-aligned split safety check**: the reserve-floor refusal is `balance - fee >= protected_minimum + pending_obligations`, replacing `balance - source_amount >= floor`. A split never removes value from the vault's custody — every chunk output pays the vault's own script and is ledger-tracked from the instant of broadcast — so only the network fee genuinely leaves. The old formula pretended the whole source had left, which deadlocked the exact bootstrap scenario shaping exists for: a vault whose dominant liquidity IS one oversized deposit could never be restructured at all. The check is pre-run before a claim is ever written, re-run by EVERY signer independently (`signing::goldcoin_split::RecoverySplitSource`, which also independently proves the plan it signs serializes byte-identically to the persisted unsigned transaction, and refuses non-root-vault sources), and re-run before a resumed never-broadcast split's bytes reach the network. Non-overridable everywhere.
5. **Mempool-safe UTXO sync** (`Ledger::sync_vault_utxos`): one missed `listunspent` snapshot can no longer permanently destroy accounting state. Chunk outputs of a split still in `Broadcast` are exempt from the absence flip (their transaction's fate is owned explicitly by the lifecycle above — the 0-conf payout-change policy's own "disappearance removes it from the selectable pools immediately" behavior is deliberately unchanged); and a row the sync itself once inferred `Spent` (`spent_by_txid` NULL — never one spent by a transaction this service signed) is resurrected when a fresh snapshot reports the outpoint unspent again (parent re-broadcast after eviction, reorg restored it). Chain truth wins in both directions; rows this service spent stay `Spent` forever.
6. **`list_unspent` scan stays at min_conf 0** (`Orchestrator::tick_vault_utxos`) — the same 0-conf-inclusive scan the "Zero-conf payout change" feature above already established, which shaping's own accounting (split chunks tracked as `Unconfirmed` from broadcast) equally depends on. **The maturity policy itself is unchanged**: classification against `vault_min_confirmations` still happens in `sync_vault_utxos`, and reconciliation's `observed_balance` keeps its own mature-only read.

### What deliberately did NOT change

- **External deposits still require `vault_min_confirmations` before becoming selectable.** Unchanged everywhere.
- **The "Zero-conf payout change" policy above is preserved exactly as shipped** (`zero_conf_change_max_depth = 1` behavior, provenance table, parent validation, holds — all untouched). Shaping composes with it, on the strict side: a split's chunk outputs get NO `goldcoin_payout_change_outpoints` row — they are not payout change — so they fail closed onto the full `vault_min_confirmations` policy, exactly as that section's provenance rules already state for vault-split outputs. Shaping never widens the 0-conf surface by a single outpoint.
- **Selection preference order is unchanged**: exact match, then the change-minimizing choice between the smallest covering single UTXO and a bounded smallest-first combination (so a huge confirmed deposit is *usable* immediately but not *wastefully consumed* while smaller mature chunks suffice), then largest-first accumulation — fewer inputs always preferred, `Reserved`/`Spent`/deposit-backing outpoints never offered. The zero-conf pool joins only via the pre-existing two-phase selection, exactly as before.
- **No manual SQLite edits**: every state change goes through typed ledger methods.

### Operator workflow (the whole point)

1. Deposit any large GLC refill directly to the reserve vault address.
2. Wait `vault_min_confirmations`.
3. Done. The daemon restructures the deposit into payout-sized chunks itself (bounded by the reserve-floor safety check), payouts select from them automatically, and every payout's own change fans back out at the canonical chunk size. `glc-admin split-vault-utxo` remains available but is no longer part of normal operation; `utxo_shaping_enabled = false` restores the operator-driven flow.

Config (all under `[goldcoin]`). **Two keys are REQUIRED, explicitly, for production** (2026-08-31 review, M2 — a binary upgrade must never silently change an existing deployment's behavior): `utxo_shaping_enabled = true` (the bare default is `false`: autonomous vault self-spends are explicit opt-in; in-flight split lifecycle maintenance runs regardless — only NEW automatic splits are gated) and `change_fanout_target_atomic = 500000000000` (5,000 GLC, the reviewed production chunk sizing; the bare default stays at the pre-existing 2,500 GLC so configs omitting the key keep their old behavior). Both are set in `service/config.pilot-template.toml` with `REQUIRED PRODUCTION KEY` markers. The remaining knobs default sensibly: `utxo_shaping_target_available_count` (15 — matches `utxo_pool_warning_count`), `utxo_shaping_min_source_atomic` (`4 * change_fanout_target_atomic`; validated `>= 2x` the chunk target), `utxo_shaping_max_outputs_per_split` (25). The daemon logs all effective values at startup for cross-signer drift diagnosis.

**Trust-model note — the signer-side split floor (2026-08-31 review, M1, documented decision, no code change).** Each vault signer's non-overridable split refusal is the solvency formula `balance - fee >= protected_minimum + pending_obligations`: it guarantees a signer can never be induced to sign a split that makes the reserve INSOLVENT (chunks stay vault-owned; only the fee leaves), but it deliberately does not encode payout LIVENESS — the guard that a split must not immobilize the mature liquidity already-admitted obligations need lives in the orchestrator/CLI (both non-overridable there). The residual accepted: a buggy or compromised orchestrating process could obtain threshold signatures for a split that stalls payouts for one `vault_min_confirmations` maturity window — a bounded liveness effect, never a solvency one, on par with the same process's existing ability to simply not build payouts at all. The alternative (restoring `balance - source >= floor` in signers) provably deadlocks the bootstrap scenario shaping exists for (test J). Any future change to this posture is a docs/02-trust-model.md amendment requiring its own sign-off, not a code tweak.

### Regression coverage

`service/tests/utxo_liquidity_autoshaping.rs` drives the real production code paths end to end (real ledger, real independent 2-of-3 split signing, real selection/fan-out, a broadcast double with real node-membership/eviction semantics): **A** — one 1,000,000 GLC deposit funds six cycles of repeated 20,000 GLC payouts with zero manual splits, no `TooManyInputs`, no outpoint reuse, and reconciliation clean throughout; **J** — the production bootstrap (the deposit IS the entire reserve, admission floor 10) parks on the count floor, shapes anyway under the solvency-aligned check, and self-recovers after one maturity window; **B** — the incident's exact fragmented pool shape selects within `max_inputs`; **D/E/F** — with the 0-conf policy disabled, unconfirmed internal change and sub-min-conf external deposits are never selectable and become selectable exactly at maturity; **G** — concurrent reservation cannot double-select an input; **H/H2** — restart resumes `Signed` splits by stored bytes and `Built` splits by verified re-signing; **I/I2** — a healthy pool produces no self-transactions, and shaping never stacks a second split while chunks mature; **K** — at the production depth-1 zero-conf setting, split chunks are never 0-conf eligible or reservable while payout change keeps its documented eligibility; **L** — an evicted `Broadcast` split keeps its accounting term through the missed snapshot, is re-broadcast byte-identically, and reaches `Confirmed`; **M** — a conflicted split is abandoned loudly, its phantom chunks cleared, and shaping continues; **N** — a claimed split whose source vanished is abandoned, and the source resurrects and re-splits when the chain restores it; **O** — a claimed source is invisible to payout selection and unreservable, and abandonment releases it. `tests/zero_conf_change_policy.rs` continues to own the 0-conf payout-change policy itself, including that vault-split outputs never receive it. The selector's feasibility-completeness and the `max_inputs = 25` cost decision are pinned in `service/src/goldcoin/coin.rs`'s own tests.

## Admission control (Solana->Goldcoin) (added 2026-08-24)

### Why this exists

The local ledger pause (`glc-admin pause`/`unpause`, above) and payout processing were never actually the same thing: `Orchestrator::tick_goldcoin_payouts` has never checked `paused` — it always continues building/signing/broadcasting for any request already `SourceFinalized`, regardless of pause state. The ONLY thing `paused` gates is `Ledger::fold_sol_deposit`'s decision to admit a newly observed on-chain SolToGlc obligation (`SourceFinalized`) versus park it (`ManualReview`). Because that's the single lever, an operator recovering from an incident (e.g. the vault-UTXO-splitting scenario above) who calls `unpause` to let the reserve return to normal simultaneously reopens admission for brand-new deposits — right when reserve headroom is thinnest, racing the still-draining backlog and risking an immediate re-pause.

`admission_closed` (`reserve_ledger`, separate from `paused`) fixes this by giving admission its own, independent, operator-only switch. **Scoped to `--direction goldcoin`** — `glc-admin close-admission`/`open-admission --direction goldcoin`; `--direction solana` is refused with a clear "not implemented in this version" error rather than silently doing nothing.

> **It governs BOTH inbound routes, not just SolToGlc (corrected 2026-09-10).** The flag is a property of the GOLDCOIN RESERVE, not of a route. Since Phase F, `Ledger::fold_robinhood_deposit` reads the same `reserve_ledger` row as `fold_sol_deposit`, so closing admission parks new `RhnToGlc` deposits too — with the same `admission_closed_at_fold` note. The section title and the `Solana->Goldcoin` wording throughout this section predate that and are kept only because other documents link to them. Both folds now take the decision through one shared evaluator, `crate::ledger::admission::InboundAdmissionGates`.
>
> A related trap for whoever is holding the pager: **`glc-admin robinhood-status` does not show this gate.** Its `Robinhood reserve ... paused` line is the separate `RobinhoodReserve` row, which backs the OUTBOUND `GlcToRhn` route only. For `RhnToGlc`, read `glc-admin status`'s `GoldcoinReserve: ... paused=... admission_closed=...` line.

### What it does, and does not, change

- Only `fold_sol_deposit`'s admission decision reads `admission_closed`. Both gates (`paused` and `admission_closed`) must be clear for a new obligation to be admitted — closing either one alone is enough to route a new fold to `ManualReview`; the pre-existing `paused` behavior is completely unchanged.
- Payout processing, confirmation tracking, the 2-of-3 signer path, reconciliation's breach formula, the rolling-volume quota, and the on-chain program are all untouched. An already-`SourceFinalized`/`SettlementAuthorized`/`DestinationSubmitted` request is never affected by `admission_closed` in any way — it keeps processing exactly as it always has.
- **No automatic reopen, and nothing automatically closes it either**: reconciliation and the rolling-volume quota continue to only ever touch `paused`, exactly as before. `admission_closed` changes ONLY via an explicit operator command. (The confirmed-liquidity safety buffer added 2026-09-02 *does* open and close automatically — but it is a SEPARATE column and a separate axis, and never reads or writes `admission_closed`. See "Confirmed-liquidity admission safety buffer" below.)
- **No manual DB editing** — both directions go through `Ledger::set_admission`, never a raw `UPDATE`.

### Exact operator procedure

1. `glc-admin close-admission --db PATH --direction goldcoin --note TEXT` — always allowed. New SolToGlc **and RhnToGlc** deposits now fold into `ManualReview` instead of `SourceFinalized`; nothing about already-accepted requests changes.
2. Let already-accepted obligations continue draining normally (no action needed — payout processing was never gated by admission or pause in the first place).
3. When ready to accept new transfers again: `glc-admin open-admission --db PATH --direction goldcoin --note TEXT`. Refuses unconditionally (no override) unless ALL THREE: `GoldcoinReserve`'s hard invariant currently holds (`balance >= protected_minimum + reserved_liquidity`, the same check `reconciliation::reconcile` enforces); the mature UTXO count is still above `utxo_pool_min_available_count` (`Ledger::check_utxo_liquidity_for_admission` — the same count-based gate `fold_sol_deposit` applies to a brand-new obligation, added by the "PR #35 maintainer-review fixes" section above); and the automatic confirmed-liquidity gate has already reopened (`Ledger::check_liquidity_buffer_for_admission`, added 2026-09-02 — otherwise clearing the operator flag would appear to succeed while every new fold kept parking). Each error names its own figures: the current count and configured floor plus any known unconfirmed internal change, or the current confirmed headroom and the reopen threshold.
4. `glc-admin status --db PATH` reports `admission_closed=<bool>` per direction alongside the existing `paused=<bool>`. The public `/status` endpoint exposes this gate as **`goldcoin_destination_admission_open`** (added 2026-09-10) and, unchanged for wire compatibility, as `sol_to_glc_admission_open` — one value under two names, because the historical name is narrower than what the gate governs. A UI should read `false` there as "not accepting new transfers right now" (maintenance), distinct from `sol_to_glc_available` being `false` for reserve-health/quota reasons. For a per-route answer that also folds in pause, capacity and the mature-UTXO floor, read `available` on `GET /chains`.

### Route enablement is NOT availability (`/chains`, added 2026-09-10)

`GET /chains` returns two different booleans per route and they answer different questions:

| Field | Means | Source |
|---|---|---|
| `enabled` | the route is SWITCHED ON in this deployment | `RouteGate`: config file + `bridge_routes` + adapter capability |
| `available` | a transfer started now would actually be admitted | `enabled` AND every runtime gate on the route's DESTINATION reserve |

`available` is computed from `crate::ledger::admission::InboundAdmissionGates` — the same evaluator both folds gate on — covering `paused`, `admission_closed`, the confirmed-liquidity gate and its safety buffer, the mature-UTXO pool floor, and capacity. It fails closed on a non-implemented route, a disabled route, an unconfigured destination reserve, or a failed ledger read, and it is strictly read-only (it reports the confirmed-liquidity gate's persisted state and never evaluates the hysteresis, so a public GET can never move an admission gate).

**Why the split exists.** A production launch-blocker on 2026-09-10: `/chains` reported `RhnToGlc` as `enabled: true` — correctly; the route gate was open — while `admission_closed` was set on `GoldcoinReserve`. The UI rendered "Available" from `enabled`, users made irreversible on-chain deposits into the custody contract, and every one folded to `ManualReview` with `admission_closed_at_fold` (requests 4008, 4009, 4010). Unlike `GlcToSol`/`GlcToRhn`, an `RhnToGlc` deposit goes straight to the contract with no `POST /transfers` preflight in front of it, so the published availability signal is the only thing between a user and an unadmittable deposit.

**For UI authors:** gate the "start a transfer" affordance on `available`. Use `enabled`/`implemented` only to choose wording — "Coming soon" for a route this build cannot serve, "temporarily unavailable" for one switched on but currently closed. Two things `available` deliberately does not cover, each with its own endpoint: the per-wallet rolling-24h windows on both legs of every route (`GET /routes/{route}/eligibility`; the older `GET /recipients/{sol,rhn}-to-glc/eligibility` still serve the Goldcoin-bound routes) and, for `GlcToSol`, the on-chain rolling-volume window (`GET /status`'s `glc_to_sol_quota_exhausted`; note `crate::quota` engages the local pause once it observes exhaustion, at which point `available` does go `false`). It is also amount-independent by necessity — it answers "would a minimum-sized deposit be admitted", so a large enough transfer can still be held back by the buffer or capacity.

### Behaviour change: the mature-UTXO count is now one query (2026-09-10)

Collapsing the three admission call sites onto `crate::ledger::admission` also collapsed three copies of the "mature, unreserved vault UTXO" predicate onto `Ledger::count_available_vault_utxos`. Two of them (`Ledger::utxo_pool_health` and `fold_sol_deposit`) already agreed; the third, `fold_robinhood_deposit`, had drifted — it excluded UTXOs backing an unfinalized `GlcToSol` deposit but not an unfinalized `GlcToRhn` one, where the other two exclude every Goldcoin-sourced direction (`Direction::SOURCE_IS_GOLDCOIN_SQL_IN`).

So an `RhnToGlc` fold could count a UTXO as available pool depth while coin selection and `glc-admin status` both treated it as spoken for. The unification fixes that in the safe direction — strictly FEWER UTXOs counted, so the floor engages slightly earlier for `RhnToGlc` than it did — and only where a `GlcToRhn` deposit is mid-confirmation, which no production deployment has today (`GlcToRhn` ships disabled). No `SolToGlc` behaviour changes.

### Resuming an individual request parked in ManualReview

`fold_sol_deposit` routes a new SolToGlc obligation to `ManualReview` (never dropped — the Solana-side deposit is already real and irreversible) whenever `admission_closed`, `paused`, or insufficient capacity was true at the exact moment it was observed. Once the underlying condition clears, that specific request does not automatically resume — `glc-admin resume-manual-review --db PATH --request-id N --note TEXT` moves it back to `SourceFinalized` (reserving its capacity, exactly as a successful fold would have) so normal processing picks it up.

Scoped narrowly and refuses (no override) unless ALL of: the request is `SolToGlc` or `RhnToGlc` (one command, route read from the request; both run the SAME shared body `Ledger::resume_manual_review_inbound`) and currently `ManualReview`; its `manual_review_note` is one of the seven known fold-time reasons (`admission_closed_at_fold`/`reserve_paused_at_fold`/`insufficient_capacity_at_fold`/`utxo_liquidity_low_at_fold`/`liquidity_buffer_low_at_fold`/`recipient_rate_limited`/`source_wallet_rate_limited` — never some other `ManualReview` cause); its source deposit is already finalized; it has no `goldcoin_payouts` row or `destination_txid` yet; NEITHER the Goldcoin destination address NOR the source wallet is still inside its own rolling 24-hour window (see "Goldcoin destination rate limit" — global across inbound routes — and "Source-wallet rate limits, per source network" — one window per source network, never pooled; both checked unconditionally, independently, regardless of the request's own `manual_review_note`); the mature Goldcoin UTXO count is still above `utxo_pool_min_available_count` (the identical count-based gate `fold_sol_deposit` applies to a brand-new obligation — refuses with `LedgerError::UtxoLiquidityLow` otherwise, added by the PR #35 maintainer-review fix above); reserving its capacity now would not breach the `GoldcoinReserve` invariant (the same `available_capacity` check `create_request`/`fold_sol_deposit` use to admit anything new); and reserving it now would still leave the confirmed-liquidity admission safety buffer intact (`LedgerError::AdmissionLiquidityBufferLow` otherwise — the same per-request formula `fold_sol_deposit` applies, for the same reason the UTXO-count floor is re-applied here: a resume re-admits real demand exactly as a fresh fold would, and self-clears the moment headroom recovers). Deliberately does NOT check `admission_closed`/`paused` — admission may stay closed while this resumes something already accepted, since it never admits anything new. **Refuses permanently for any request with a refund lifecycle** — `RefundPending`/`RefundBroadcast`/`Refunded`, or the direction's own durable refund row (`solana_refunds` for `SolToGlc`, a `Refund`-kind `robinhood_transactions` row for `RhnToGlc`) at all (checked against the row, so an out-of-band `bridge_requests.state` edit cannot re-open a refunded request) — see "ManualReview refunds (Solana->Goldcoin)" below. Idempotent: re-running it on an already-resumed request, or retrying while UTXO liquidity is still low or either rate limit still applies, is a safe no-op either way. Preserves the request's id and `source_obligation_index` — it transitions the existing row in place, never creates a new one, so a duplicate obligation is impossible by construction.

### Automatic recovery, without an operator (added 2026-08-26/27, extended 2026-08-28)

`Orchestrator::tick_auto_resume_utxo_liquidity_backlog` runs as the last phase of every tick and automatically resumes `SolToGlc` AND `RhnToGlc` `ManualReview` requests parked for a condition that self-clears over time — `utxo_liquidity_low_at_fold`, `recipient_rate_limited`, `source_wallet_rate_limited`, and (added 2026-09-04, and only while the confirmed-liquidity admission gate is open) `liquidity_buffer_low_at_fold` — oldest first ACROSS BOTH ROUTES in one global `(created_at, id)` order (the destination window is route-global, so a backlog to one Goldcoin address is one queue spanning both), reusing `resume_manual_review_sol_to_glc`/`resume_manual_review_rhn_to_glc` verbatim — two wrappers over one shared body, identical safety checks, no separate logic. Which reasons those are is decided by `Ledger::is_auto_resumable_manual_review_reason`, next to the reason constants themselves, never by a literal list in the orchestrator. It never touches any other `ManualReview` reason (`admission_closed_at_fold`/`reserve_paused_at_fold`/`insufficient_capacity_at_fold` still require `glc-admin resume-manual-review`), stops the whole batch immediately on a paused reserve, closed admission, `OrchestratorConfig::max_auto_resumes_per_tick` being reached, or any unexpected error — except the refusals that are per-request by construction: `recipient_rate_limited`, `source_wallet_rate_limited` (each a per-recipient or per-wallet condition) and `AdmissionLiquidityBufferLow` (amount-dependent — this request does not fit above the buffer, which says nothing about a smaller one). Each of those skips that one candidate (counted in `AutoResumeReport::skipped`) and the pass continues to the next, so one recipient, wallet, or oversized request never stalls unrelated, eligible candidates behind it in the same tick. A request with a refund lifecycle is never a candidate at all (a refund moves it out of `ManualReview`), and is additionally refused-and-skipped by the same per-request rule if one is ever reached through an out-of-band state edit.

## Route-scoped admission (inbound-to-Goldcoin) (added 2026-09-10, schema v25)

### Why this exists

Everything in "Admission control" above is RESERVE-wide. `SolToGlc` and `RhnToGlc` both settle out of `GoldcoinReserve`, so `pause --direction goldcoin` and `close-admission --direction goldcoin` each shut BOTH routes, and there was no supported way to hold one open while the other was closed. The obvious workaround does not exist either: `Route::is_operator_settable` refuses a `bridge_routes` write for `SolToGlc` outright (route ENABLEMENT is a different axis, deliberately not a second spelling of "turn off production traffic"), and the config file has no legacy-route surface.

That is a real operational gap. A Robinhood-side incident — a custody-contract concern, a signer rotation, an indexer halt — has nothing to do with Solana↔Goldcoin traffic, but the only lever big enough to stop `RhnToGlc` also stopped `SolToGlc`. Schema v25's `route_admission` table adds the missing scope.

### The state model

| Axis | Scope | Table / column | Settable for | Command |
|---|---|---|---|---|
| pause | reserve-wide **emergency stop** | `reserve_ledger.paused` | goldcoin, solana | `pause`/`unpause` |
| admission | reserve-wide | `reserve_ledger.admission_closed` | goldcoin | `close-admission`/`open-admission` |
| **route admission** | **one route** | **`route_admission.admission_closed`** | **SolToGlc, RhnToGlc** | **`route-admission-close`/`route-admission-open`** |
| enablement | one route | `bridge_routes.enabled` | GlcToRhn, RhnToGlc | `robinhood-route-enable`/`-disable` |

Note the bottom two rows cover DIFFERENT pairs of routes, overlapping only in `RhnToGlc`. Route admission is settable for the two routes whose DESTINATION reserve is Goldcoin (`Direction::destination_is_goldcoin`); enablement is settable for the two executable Robinhood routes. `GlcToSol` has neither — its control remains the Solana reserve's own pause.

### BOTH axes must be open

A route admits a newly observed deposit only when its own gate AND every reserve-wide gate say yes. The two are ANDed by the same `crate::ledger::admission::InboundAdmissionGates` evaluator both folds and `GET /chains` already use — the route flag is read there, once, rather than in three places, precisely so the fold and the published availability signal cannot drift apart again (that drift is what produced requests 4008-4010).

Consequences worth stating explicitly, because operators reliably assume otherwise:

- **Unpausing the reserve does NOT open a route whose own gate is closed.** `unpause --direction goldcoin` clears the reserve-wide stop and nothing else; the route stays shut until `route-admission-open`.
- **Opening a route does NOT unpause anything.** With the reserve paused or its reserve-wide admission closed, `route-admission-open` succeeds and the route still admits nothing.
- Reserve-wide pause remains the emergency stop. Nothing in this section weakens it.

### What it does, and does not, change

- **The migration changes no behaviour.** v25 seeds both routes at `admission_closed = 0` (open), so a ledger that upgrades through it admits exactly what it admitted before. Re-running the migration uses `INSERT OR IGNORE` and can never reopen a route an operator closed.
- An absent `route_admission` table or row resolves to OPEN. This inverts the fail-closed rule used everywhere else in the service, deliberately: absence IS the pre-v25 state, so resolving it to "closed" would make the migration itself an outage. The gate can only ever subtract from what the reserve-wide gates already allow, so its absence can never admit something the reserve would have refused.
- Payout processing is untouched, as with every other admission flag. An already-`SourceFinalized` request keeps processing regardless.
- **Never automatic.** Nothing closes or opens a route gate except these two commands. Reconciliation and the quota engine still only ever touch `paused`.
- **No manual DB editing** — both directions go through `Ledger::set_route_admission` behind `admin_api::audited_set_route_admission`, so every change (and every refusal) leaves an `admin_audit_log` row. The table's own CHECK independently refuses a row for any route outside the inbound-to-Goldcoin pair.

### A parked deposit keeps both exits

A deposit that folds while its route gate is closed parks in `ManualReview` with `route_admission_closed_at_fold`. That reason is on BOTH `RECOVERABLE_MANUAL_REVIEW_REASONS` and `REFUNDABLE_MANUAL_REVIEW_REASONS`, so `resume-manual-review`, `manual-review-settle`, `refund-manual-review` and `robinhood-refund` all work on it exactly as they do for `admission_closed_at_fold`. It is deliberately NOT auto-resumable: an operator closed the route on purpose, and the unattended pass must not undo that.

### Exact operator procedure

1. `glc-admin route-admission-show --db PATH` — read BOTH axes before touching either. Prints each inbound route's own gate, the reserve-wide pause and admission it is ANDed with, and whether the route admits right now (with the blocking gate named). A pre-v25 ledger is reported as HAVING NO TABLE, not as defaults.
2. `glc-admin route-admission-close --db PATH --route <SolToGlc|RhnToGlc> --note TEXT` — always allowed. New deposits on that route alone park in `ManualReview`; the sibling route keeps settling.
3. Let already-accepted obligations drain normally (no action needed).
4. `glc-admin route-admission-open --db PATH --route <SolToGlc|RhnToGlc> --note TEXT` — refuses unconditionally (no override) unless the route's destination reserve passes the SAME three checks `open-admission` requires: the hard reserve invariant holds, the mature-UTXO count is above `utxo_pool_min_available_count`, and the automatic confirmed-liquidity gate has already reopened. Opening a route is opening admission, so it is not a cheaper way around those checks.
5. Verify with `glc-admin route-admission-show --db PATH` and `glc-admin status --db PATH` (which now prints a `Route admission:` block beneath the per-reserve lines), and confirm the public signal with `GET /chains` — `available` reflects this gate.

### What an operator sees

- `glc-admin status` — a `Route admission:` line per inbound route, printed separately from the per-reserve `paused=`/`admission_closed=` line so the two scopes are never mistaken for one number.
- The admin API's `GET /admin/status` — a `route_admission` array carrying the route's own flag, the reserve-wide flags, the resulting `admits_now`, and the named `blocker`. Empty on a pre-v25 ledger.
- `GET /chains` — `available` goes `false` for the closed route and stays `true` for its sibling. `enabled` is unchanged: this is admission, not enablement.
- `GET /status` — `sol_to_glc_available` and `sol_to_glc_admission_open` now account for `SolToGlc`'s own gate, so `/status` and `/chains` cannot disagree. `goldcoin_destination_admission_open` keeps its reserve-wide meaning and is the half that does NOT include the route flag.

### Regression coverage

`service/tests/route_scoped_admission.rs` drives the real folds and the real audited command path: SolToGlc closed while RhnToGlc settles, the mirror, reserve-wide pause closing both, reopening the reserve failing to override a closed route, durability across a ledger reopen, the audited path refusing every non-inbound route, and the non-executable Solana↔Robinhood routes staying unavailable. `ledger::admission::tests` pins the AND and the ranking as pure functions; `ledger::schema::tests` pins the v25 seed, its re-run safety and its CHECK; `api::tests` pins `/chains` and `/status` agreeing.

## Confirmed-liquidity admission safety buffer (Solana->Goldcoin) (added 2026-09-02)

### Why this exists

`protected_minimum` is a cliff, not a cushion. Until now the last SolToGlc obligation admitted before headroom ran out could take the reserve from "comfortable" to "sitting exactly on the hard floor" in one step, with the next arrival parking as `insufficient_capacity_at_fold` and an operator finding out only from the ManualReview backlog. The reserve was never insolvent at any point — the accounting was correct throughout — but there was no margin left to absorb a payout fee, a reorg, or a batch of deposits arriving inside one tick.

The safety buffer adds that margin. It closes admission for NEW obligations *while the reserve is still healthy*, keeping a deliberate reserve of confirmed liquidity above `protected_minimum` that new demand may not consume, and it reopens only after a genuine recovery — never on a single reading that happens to tick back over the line.

### The policy

- **Buffer (close threshold): 250 000 GLC.** Admission closes as soon as confirmed unreserved Goldcoin headroom drops **below** 250 000 GLC.
- **Reopen threshold: 350 000 GLC.** Admission reopens **only** once confirmed unreserved headroom reaches 350 000 GLC or more.
- Between the two the gate **holds whatever state it is in**. That 100 000 GLC band is the anti-flapping mechanism: a headroom oscillating anywhere inside it produces no state change at all, so the gate cannot toggle on deposit/payout churn. A single-threshold design would flip on every crossing of one number.

Both thresholds are configuration (`goldcoin.admission_safety_buffer_atomic`, `goldcoin.admission_reopen_headroom_atomic`, in 8-decimal Goldcoin atomic units), default to exactly the values above, and are validated `reopen >= buffer` at load time. Setting the buffer to `0` disables the mechanism entirely — the same "0 means disabled" shape `utxo_pool_min_available_count` uses.

### The admission calculation

A new SolToGlc obligation is admitted only when

```
total_reserve_balance >= protected_minimum
                       + reserved_liquidity
                       + <this obligation's net_destination_atomic>
                       + admission_safety_buffer
```

on top of every pre-existing gate (`paused`, operator `admission_closed`, the `utxo_pool_min_available_count` floor, both rolling-24h rate limits, and the plain capacity check). This is TWO checks, and both matter:

1. **Per-request** — the formula above. A single obligation large enough to eat into the buffer is held back *even while headroom is comfortably above the close threshold*, and smaller obligations keep flowing normally.
2. **Direction-wide** — the hysteresis gate on headroom alone (`total_reserve_balance - protected_minimum - reserved_liquidity`), which is what actually closes and reopens admission for everything.

### Confirmed means confirmed

Headroom is computed from `total_reserve_balance`, which is a **mature-only** figure by construction: `sync_vault_utxos` and `Orchestrator::tick_goldcoin_reconciliation` both filter by `vault_min_confirmations` before it is computed, and `Ledger::immature_vault_utxo_total`/`own_unconfirmed_change_atomic` are observational figures that are never added to it.

So **immature payout change buys no admission room** — including this service's own broadcast-but-not-yet-mature change, which is known, accounted for, and provably not missing. Value that cannot be spent yet must not read as room to take on new demand. (Reconciliation's hard solvency invariant *does* add `own_unconfirmed_change_atomic`, deliberately and separately: "is anything actually missing" is a different question from "may we take on more", and the two must not share an answer.)

### What it does, and does not, change

- **The hard invariant and `protected_minimum` are untouched.** The buffer sits on top of them and is never a term inside them. A reserve can be entirely solvent — `invariant_holds=true` — while the buffer has closed admission; that is the normal, intended state.
- **Already-accepted obligations keep processing.** Anything already `SourceFinalized` or later is completely unaffected: payout building, signing, broadcast, confirmation tracking and settlement all continue exactly as before, on liquidity that is real and confirmed. The gate governs admission only.
- **Nothing is cancelled.** A closed gate never touches an existing request. A newly observed deposit is still folded (the Solana-side tokens are already locked and irreversible) and parks in `ManualReview` with `manual_review_note = liquidity_buffer_low_at_fold` — resumable and refundable like every other fold-time park.
- **The operator flag is separate.** `admission_closed` (`glc-admin close-admission`/`open-admission`) remains operator-only: nothing automatic sets or clears it, exactly as before. The automatic gate is its own column and its own line in `glc-admin status`, so "I closed this" is always distinguishable from "liquidity closed this". Either one being closed is enough to park a new fold; neither can clear the other.
- **`open-admission` refuses while the automatic gate is closed**, alongside its existing invariant and UTXO-count refusals — otherwise clearing the operator flag would appear to succeed while every new fold kept parking.
- **Auto-resume, gated on the gate (revised 2026-09-04).** `liquidity_buffer_low_at_fold` IS in `Orchestrator::tick_auto_resume_utxo_liquidity_backlog`'s filter, but only while the direction-wide confirmed-liquidity gate is OPEN. It was originally excluded outright, on the reasoning that retrying the instant headroom crept over the line would re-admit exactly the demand the buffer holds back and defeat the hysteresis. That reasoning was right about the trigger and wrong about the conclusion: the correct trigger is not "headroom is over the close threshold" but "the gate has reopened", which by construction happens only on a genuine recovery to `admission_reopen_atomic` and never on a single reading back over the close line. Consulting the gate therefore USES the hysteresis rather than bypassing it. Nothing is weakened: `resume_manual_review_sol_to_glc` still re-checks the per-request buffer arithmetic, the UTXO floor, both rate-limit windows and the reserve invariant on every individual attempt, and a request too large to fit above the buffer is skipped and stays parked. The cost of the old posture was that a deposit parked by the buffer stayed in `ManualReview` until a human noticed, even after the reserve had fully recovered — the one park with a self-clearing cause and no automatic exit. `insufficient_capacity_at_fold` keeps the original posture: the accounting reserve being genuinely exhausted is not a condition that clears on its own.

### What an operator sees

- `glc-admin status --db PATH` prints an `Admission liquidity:` line for GoldcoinReserve with `confirmed_headroom`, `buffer`, `reopen_at` and `liquidity_admission_closed` (omitted entirely when the buffer is disabled).
- `/metrics`: `glc_goldcoin_admission_liquidity_closed`, `glc_goldcoin_confirmed_admission_headroom_atomic`, `glc_goldcoin_admission_buffer_atomic`, `glc_goldcoin_admission_reopen_atomic`. All gauges, never invariants — a closed gate is the mechanism working on a healthy reserve and must never flip `/health` to 503. **Alert on it staying closed, not on it closing.**
- Admin API: `liquidity_admission_closed` on the direction status view, plus the headroom and both thresholds on the reserve-health view.
- Public `/status`: `sol_to_glc_admission_open` is `false` when EITHER axis is closed. The two causes are deliberately not distinguished there — the user-facing answer ("not accepting new transfers right now") is identical.
- The daemon logs a `WARN` on every gate transition (and only on a transition, not on each evaluation).

### Public availability is evaluated at a normal transfer size (added 2026-09-12)

**The gap this closes.** The buffer rule has two readings. Direction-wide, the hysteresis gate closes when `headroom < buffer`. Per deposit, a fold admits only while `headroom - net >= buffer`. Between those two lines — headroom above the buffer but by less than one normal deposit — the gate is OPEN and every normal deposit is REFUSED. On 2026-09-12 production sat in exactly that band: 280 252 GLC of confirmed headroom against the 250 000 GLC buffer, with 50 000 GLC (47 000 net) deposits arriving every 30–60 s. `GET /status` and `GET /chains` reported `SolToGlc` `available: true` — the public verdict was computed by asking the shared evaluator about ONE atomic unit (`headroom > buffer`, true) — while 38 consecutive deposits folded straight into `ManualReview` with `liquidity_buffer_low_at_fold`, each auto-resuming only as a settlement freed exactly one deposit's worth of headroom. The route was, for every practical purpose, closed, and nothing public said so; the operator had to close `SolToGlc`'s route admission by hand to stop advertising it.

**What changed (service only; no gate, threshold or resume policy moved).**

- `SolToGlc`'s `available` — on `GET /chains`, `GET /status` (`sol_to_glc_available`) and `GET /stats` — is now the shared evaluator's verdict for a deposit of the Solana program's **`per_transfer_limit`** (the largest deposit the program accepts), widened to canonical units and netted through the route's configured fee (`InboundAdmissionGates::route_blocker_at`, `api::AdmissionProbe`). `true` therefore means "the largest permitted deposit would be admitted", so any smaller one would too. It costs the listing one extra chain read (the reserve mint's decimals). If the probe cannot be built (mint or `bridge_config` unreadable, route unpriced) the route fails CLOSED with `availability_reason = probe_unavailable` — it never falls back to the one-unit form.
- The other five routes are evaluated exactly as before (`route_blocker`, one atomic unit) — including `RhnToGlc`, which draws on the same reserve but has no stated normal size yet. Giving it one is a follow-up, not a side effect of this change.
- **`availability_reason`** (machine-readable, `null` when available) on every `GET /chains` route, and `sol_to_glc_availability_reason` on `/status` and `/stats`: the highest-ranked gate that refused, in the evaluator's own names — `liquidity_buffer_low`, `route_admission_closed`, `reserve_admission_closed`, `reserve_paused`, `utxo_liquidity_low`, `insufficient_capacity` — plus `route_disabled`, `not_implemented`, `onchain_paused`, `reserve_unavailable`, `probe_unavailable`, and on `/status` only `quota_exhausted`. `unavailable_reason` keeps its cause-agnostic end-user copy unchanged.
- **`capacity`** on `GET /chains`'s `SolToGlc` entry and `sol_to_glc_capacity` on `/status` (`RouteCapacityView`, canonical units): `confirmed_headroom_atomic`, `liquidity_buffer_atomic`, `liquidity_admission_closed` (the persisted hysteresis state), `probe_gross_atomic` (the size the verdict answers for) and `max_admissible_gross_atomic` — the reserve's own bound `headroom - buffer` grossed up through the fee, deliberately NOT capped at the program limit so the two are comparable: the route is available exactly when `max_admissible_gross_atomic >= probe_gross_atomic` and no other gate refuses. In the incident's figures this reads 32 183 GLC against a 50 000 GLC probe.

**What did not change.** The hysteresis gate, both thresholds, the per-fold buffer arithmetic, `open-admission`'s refusals, and the auto-resume policy are untouched; this changes what is REPORTED, never what is admitted. `sol_to_glc_admission_open` keeps its meaning (the two admission axes only) and can be `true` while `sol_to_glc_available` is `false` for `liquidity_buffer_low` — that combination IS the band described above, and is now visible.

**Reading it during an incident.** `availability_reason = liquidity_buffer_low` with `liquidity_admission_closed = false` and `sol_to_glc_admission_open = true` is the 2026-09-12 shape: the reserve is solvent and no gate is closed, but headroom is within one deposit of the buffer. Follow "Exact operator procedure when the gate closes" above — the remedy (more confirmed Goldcoin headroom, usually via settlements draining or a rebalance) is the same; the difference is only that the gate has not yet closed. Consider `glc-admin route-admission-close --route SolToGlc` if depositors must be stopped rather than merely told.

Regression coverage: `ledger::admission::tests` (`route_blocker_at_is_the_real_decision_at_that_size`, `a_tiny_probe_can_say_open_while_a_normal_transfer_is_refused`, `max_admissible_net_is_the_exact_boundary_of_the_decision`), `amount_conversion::tests::max_gross_for_net_is_the_exact_inverse_of_the_fee`, and `api::tests` from `sol_to_glc_is_unavailable_when_headroom_cannot_admit_the_max_transfer` through `capacity_and_reason_fields_are_additive_on_the_wire` (buffer binds => `false` with the exact reason and figures; sufficient headroom => `true`; the one-unit evaluator saying "open" for the same ledger no longer makes the route available; the other five routes untouched; each gate's exact reason on `/chains` and `/status`; an unbuildable probe failing closed; wire additivity).

### Exact operator procedure when the gate closes

1. Confirm it is the buffer and not something else: `glc-admin status --db PATH`. `liquidity_admission_closed=true` with `paused=false`, `admission_closed=false` and `invariant_holds=true` is the buffer doing its job on a healthy reserve.
2. **Do nothing to already-accepted obligations.** They are still settling; interfering is the only way to turn this into an incident.
3. Look at where the headroom went. The common case is that mature liquidity is temporarily sitting in immature payout change — `glc-admin status`'s `UTXO liquidity:` line shows `temporarily_immature_internal_change`. That recovers on its own as the change matures, and the gate reopens automatically at 350 000 GLC.
4. If it is not recovering, the reserve genuinely needs more Goldcoin: run the rebalance procedure above — `glc-admin rebalance-propose`, then `glc-admin rebalance-approve` once per approving identity, then execute the real transfer through the relevant custody tooling **outside this system** and record its evidence with `glc-admin rebalance-record-executed`, and finally `glc-admin rebalance-confirm` once the transfer is independently observed. `rebalance-confirm` is the step that updates the cached `total_reserve_balance`, and therefore the step that actually moves confirmed headroom. The gate then reopens on the next tick after confirmed headroom reaches the reopen threshold — no command is needed, and no command can force it early.
5. Deposits that parked meanwhile: **normally nothing to do.** Since 2026-09-04 the daemon's auto-resume pass drains `liquidity_buffer_low_at_fold` parks on its own, oldest first, once the gate reopens — see "Auto-resume, gated on the gate" above. Check with `glc-admin manual-review-settle-list --config PATH`, which lists every candidate with the reason it is (or is not) settleable right now. Intervene only for a request that stays parked: `glc-admin resume-manual-review --db PATH --request-id N --note TEXT` (it re-checks the buffer itself and refuses safely until headroom allows — a request too large to fit above the buffer is exactly the case auto-resume skips), or `glc-admin refund-manual-review` for one that will genuinely never be paid out.

## Freezing a ManualReview snapshot: per-request auto-resume hold (added 2026-09-12, schema v29)

**When.** A set of parked requests must not be touched by the daemon's automatic recovery pass for a fixed period and will then be refunded — the 2026-09-12 case: 72 `liquidity_buffer_low_at_fold` / wallet-window / pause-family parks (3.45M GLC gross) that would otherwise have auto-resumed one at a time as headroom recovered, while the bridge itself had to reopen for new deposits. Neither existing lever fits: `max_auto_resumes_per_tick = 0` is global (freezes future parks too, needs a restart), and rewriting `manual_review_note` disqualifies the row from `refund-manual-review`'s allowlist — the one thing it is being kept for.

**What it is.** Two nullable columns on `bridge_requests` (`auto_resume_hold_note`, `auto_resume_hold_until`), set and cleared ONLY by explicit operator command on explicit ids. While `auto_resume_hold_note` is set: `Orchestrator::tick_auto_resume_utxo_liquidity_backlog` does not consider the row at all (it is not attempted and does not consume the per-tick budget), and every resume entry point — `resume-manual-review`, `manual-review-settle` and its dry run — refuses it with `held by operator (auto_resume_hold …)`. Refund commands ignore the hold. `auto_resume_hold_until` is informational (the moment the operator intends to act); **the hold never expires on its own** — release or refund is always an explicit act. Each hold/release writes a `ManualReview -> ManualReview` state-log row (`auto_resume_hold` / `auto_resume_hold_released`) plus an admin-audit row, so the Explorer and audit log show it.

**Why future rows are unaffected.** Folds never read or write the two columns; a row created after a hold was placed has both `NULL` and is handled exactly as before (pinned by `orchestrator::tests::a_held_request_is_skipped_by_auto_resume_and_new_folds_are_unaffected` and `ledger::tests::a_hold_blocks_resume_until_released_and_never_reaches_other_rows`). The migration adds columns only — no backfill, every existing row starts unheld.

**Procedure.**
1. Snapshot immediately before applying: `curl -s 'http://127.0.0.1:9101/transfers?state=ManualReview&limit=500'` and take the ids (the command re-validates each: not ManualReview, a destination txid or a payout row ⇒ that id is REFUSED and reported, the rest are held).
2. `glc-admin manual-review-hold --db /var/lib/glc-bridge/ledger.db --request-ids <ids> --hold-hours 72 --note "<why>"` — prints one verdict per id and `hold_until`.
3. Reopen the bridge as normal (`route-admission-open`, etc.). New parks auto-resume/refund exactly as today.
4. Verify: `glc-admin manual-review-hold-list --db PATH`; the daemon log shows no `auto-resume: attempting` for held ids.
5. At `hold_until`: refund each held id through the official tooling — `glc-admin refund-manual-review --config PATH --request-id N --note TEXT` (dry run), then with `--execute --keypair …`; `robinhood-refund` for Robinhood-sourced rows. The refund path re-checks state, payout absence and the refund lifecycle itself (no double refund). The hold marker stays on the refunded row as audit trail.
6. To un-freeze a row instead: `glc-admin manual-review-hold-release --db PATH --request-id N --note TEXT`, after which it is eligible for auto-resume on the next tick exactly as an unheld park.

## Choosing between recovery and refund (added 2026-09-01)

A `SolToGlc` request parked in `ManualReview` has exactly two operator
exits, and they are mutually exclusive and both one-way:

| | **Recovery** (`manual-review-settle`) | **Refund** (`refund-manual-review`) |
|---|---|---|
| What the user gets | the GLC they asked for, on Goldcoin L1 | their original Solana deposit back |
| Bridge state needed | none — runs 24/7 | **global on-chain pause** for the duration |
| Ends as | `Settled` | `Refunded` |

**Recovery is the default. Reach for a refund only when the request
genuinely can never settle.**

The reasoning is simply what the user asked for: they initiated a bridge
transfer, and completing it is the outcome they wanted. A refund is a
compensating action for a promise the bridge cannot keep — not an
equally-good alternative. A refund also costs more operationally (it
requires pausing the whole bridge) and returns the user to square one,
having paid Solana fees for nothing.

### Choose RECOVERY when

- the park reason has cleared or can be cleared: admission was closed and
  is now open, the reserve was paused and is now healthy, capacity or
  mature UTXOs were short and have recovered, or a rate-limit window has
  elapsed;
- the Goldcoin destination address in the request is still valid and
  payable;
- the reserve can cover the payout now (the dry run tells you).

In short: if `manual-review-settle` dry-runs as ELIGIBLE, that is almost
always the right action.

### Choose REFUND when

- the request can never be paid out — for example the destination
  Goldcoin address is unpayable, or the user has asked for their deposit
  back and support has agreed;
- the park reason will not clear on any reasonable timescale and the user
  should not be left waiting indefinitely;
- an incident makes completing the transfer the wrong call, and returning
  the deposit is the agreed remedy.

A refund is a decision with a support/product dimension, not purely a
technical one. If the only reason a request is parked is that the bridge
was temporarily unable to pay, recover it — do not refund it.

### If you are unsure

Dry-run both. Neither dry run mutates anything, contacts a signer, or
moves funds, so running both is free and tells you exactly what each
would do against current live state:

```
glc-admin manual-review-settle --config PATH --request-id N --note "assessing"
glc-admin refund-manual-review  --config PATH --request-id N --note "assessing"
```

Then pick, and remember both are one-way: once a refund lifecycle starts
the request can never be recovered, and once recovered it can never be
refunded. The code enforces this in both directions — but the code cannot
tell you which the user actually wanted.

## ManualReview -> L1 settlement recovery (added 2026-09-01)

The opposite decision to a refund: **complete** the user's original
bridge request onto Goldcoin L1 rather than returning their deposit.
Prefer this whenever the request can still legitimately settle — a refund
is for requests that never will.

**The bridge keeps running throughout.** No pause of any kind is required
or taken, and nothing about normal settlement changes.

### What it actually does

It re-admits the parked request into the **existing** Goldcoin payout
pipeline, transitioning `ManualReview -> SourceFinalized` and reserving
its capacity exactly as a successful fold would have.
`Orchestrator::tick_goldcoin_payouts` then carries it through the same
build/sign/broadcast/confirm path as every other SolToGlc request. There
is deliberately **no second payout implementation**, and this command
signs nothing and moves no funds itself.

### Eligibility

Refused (no override) unless ALL of: the request is `SolToGlc` and
currently `ManualReview`; its reason is one of the seven recoverable
fold-time reasons (`admission_closed_at_fold`, `reserve_paused_at_fold`,
`insufficient_capacity_at_fold`, `utxo_liquidity_low_at_fold`,
`liquidity_buffer_low_at_fold`, `recipient_rate_limited`,
`source_wallet_rate_limited`); it has **not**
entered a refund lifecycle; it has no Goldcoin payout row and no
destination transaction; neither the recipient nor the source wallet is
inside its 24-hour window; the mature UTXO count is above the floor;
the confirmed-liquidity admission safety buffer's gate is open; and
re-admitting would not breach the GoldcoinReserve invariant.

That list is never maintained by hand: the dry run *trials* the real
`resume_manual_review_sol_to_glc` and rolls it back, so any gate added to
the resume path applies to recovery automatically. The admission safety
buffer (added 2026-09-02, above) arrived exactly that way. Because a
closed buffer gate is the one refusal the capacity numbers cannot
explain — headroom can look ample while admission stays shut, since it
reopens only on a genuine recovery to the reopen threshold — the dry run
prints the buffer, its reopen threshold, and whether the gate is closed.

Additionally, and unlike `resume-manual-review`, the original deposit is
**re-proven on chain**: the `WithdrawalObligation` is re-read at
`finalized` and must exist, still be `Pending`, and carry the same
requester, amount **and Goldcoin destination** the ledger recorded. The
destination check matters most of the three: `bridge_requests.recipient`
is a copy the indexer made of the obligation's own `glc_address` at fold
time, and it is the field that decides who receives the coins. If the two
ever disagree, recovery refuses — paying the stored address would send
real Goldcoin somewhere the depositor never named, unrecallably, and
`record_goldcoin_completion` would then refuse to record the settlement
because it binds a hash of the on-chain address. An empty on-chain
destination is refused for the same reason. An unreachable RPC is a
refusal, never an assumption.

One assumption this proof cannot close, stated so it is not mistaken for
one it does: since the 2026-09-02 reserve-withdrawal hardening,
`status == Pending` no longer means "untouched". `refund_withdraw` returns
a depositor's funds without taking the obligation as `mut`, so a refunded
deposit stays `Pending` on chain forever. What prevents paying a refunded
deposit twice is the database — the `solana_refunds` row check, which
refuses on a row in ANY state and is written *before* the refund
transaction is broadcast, so a crash mid-refund still leaves the blocking
row. A refund executed wholly out of band, leaving no row, would defeat
it; that takes the same attestation quorum that could move reserve funds
directly.

**The destination Goldcoin address and the amount cannot be supplied or
changed by the operator** — both are columns on the existing request row,
and the command takes only a request id and a note.

### Dry run and execute

```
glc-admin manual-review-settle --config PATH --request-id N --note "why"
glc-admin manual-review-settle --config PATH --request-id N --note "why" --execute
```

The dry run is strictly read-only. It works by **running the real
re-admission and rolling it back**, so its verdict is exactly what an
execute would do — the dry run and the enforced gate are the same code
and cannot drift. It briefly takes SQLite's write lock; nothing persists.

Execute refuses outright if the dry run in the same invocation did not
clear. Re-running on an already-recovered request is a safe no-op.

### Relationship to refunds

The two are mutually exclusive, enforced in both directions: a request in
any refund lifecycle can never be recovered, and a recovered request can
never be refunded. Once settled, the request is terminal and
irreversible.

### There is no un-recover

Re-admission is one committed transaction. If a request is re-admitted in
error and has not yet paid out, handle it through the existing
payout-failure paths — **never** by editing state. This is the one step
an operator may expect to be reversible and is not.

### Finding candidates, and why the listing is trustworthy

`glc-admin manual-review-settle-list` applies only the STRUCTURAL
membership test — SolToGlc, currently `ManualReview`, a reason on the
recoverable list, no refund lifecycle — and then reports, for each
candidate, the verdict of the same rolled-back trial the single-request
dry run uses. It applies no rate-limit, liquidity or capacity filter of
its own, so a candidate blocked today by a window that ages out or by
headroom that recovers is LISTED, with the reason, rather than hidden.

That matters because the two rolling-24h accessors answer the
*admission-time* question ("may a brand new deposit for these bytes be
admitted?"), which counts the candidate's own row and any row that
arrived after it. Recovery asks a deliberately different question — may
this ALREADY ACCEPTED deposit proceed, blocked only by a strict
predecessor — so a parked request routinely reads as "rate limited" to
those accessors while being genuinely recoverable. The figures the dry
run prints from them are informational and gate nothing.

**2026-09-04 defect, fixed:** the listing used to filter on a
hand-maintained reason list that never received `liquidity_buffer_low_at_fold`
when the admission safety buffer added it to the resume path on
2026-09-02. `manual-review-settle-list` reported "no ManualReview requests
are currently recoverable" while `manual-review-settle --request-id N`
answered WOULD RE-ADMIT for three parked production requests. Both
surfaces now read one list through one predicate
(`Ledger::is_recoverable_manual_review_reason`), and
`ledger::tests::resume_acceptance_matches_the_recoverable_reason_list`
pins the list's contents against the fold-time reasons and against
`REFUNDABLE_MANUAL_REVIEW_REASONS` in both directions.

### Automatic recovery

`Orchestrator::tick_auto_resume_utxo_liquidity_backlog` auto-resumes the
self-clearing reasons — `utxo_liquidity_low_at_fold`,
`recipient_rate_limited`, `source_wallet_rate_limited`, and (only while
the confirmed-liquidity gate is open) `liquidity_buffer_low_at_fold` — and
never touches `admission_closed_at_fold`, `reserve_paused_at_fold` or
`insufficient_capacity_at_fold`. This command is the operator path,
chiefly for those three.

## ManualReview refunds (Solana->Goldcoin) (added 2026-09-01)

Returns a fold-parked SolToGlc deposit to the **original Solana
depositor** and closes the request permanently. This is the compensating
action docs/04-state-machines.md's "open design item: late deposits after
expiry" and docs/12-management-decisions.md item 8 left unresolved, for
the specific case where the bridge is holding a real, finalized, unsettled
deposit it is not going to pay out.

**Use this only when the request will genuinely never be paid out.** The
normal answer to a fold-time park is `resume-manual-review` (or automatic
recovery) once the underlying condition clears. A refund is one-way: once
begun, the request can never be resumed and never receive a Goldcoin
payout.

### Eligibility (all required, no override, fail-closed)

Refused unless ALL of:

- direction is `SolToGlc`, and the request is currently `ManualReview`
  (or already inside its own refund lifecycle — see "Re-running" below);
- `manual_review_note` is one of the seven **fold-time** reasons:
  `admission_closed_at_fold`, `reserve_paused_at_fold`,
  `insufficient_capacity_at_fold`, `utxo_liquidity_low_at_fold`,
  `liquidity_buffer_low_at_fold`, `recipient_rate_limited`,
  `source_wallet_rate_limited`. Every one of
  these is a park that happened *instead of* reserving Goldcoin capacity,
  on an already-finalized deposit — the two premises a safe refund needs.
  Any other `ManualReview` cause (the GlcToSol-only reasons
  `late_deposit_no_capacity` / `deposit_amount_mismatch: ...` /
  `deposit_spent_before_finalized`, a `NULL` note, or any future/unknown
  string) is refused: an ambiguous reason is excluded, never broadened;
- the source deposit is finalized (`source_finalized_at` set) and its
  on-chain `WithdrawalObligation` still reads `Pending` at `finalized`
  commitment;
- the stored `source_obligation_index`, `requester`, and gross amount all
  match the on-chain obligation **exactly** (any disagreement between
  database and chain is a hard refusal, never a "pick one side");
- no `goldcoin_payouts` row, no `destination_txid`, no `settled_at`;
- the request never advanced to `SourceFinalized` or beyond at any point
  in `bridge_request_state_log` — the per-request *proof* that no
  Goldcoin-side `reserved_liquidity`/`pending_obligations` increment was
  ever applied, so the refund has nothing to release (it never subtracts
  blindly; a request that ever held a reservation is refused outright);
- no existing refund lifecycle other than this request's own;
- the reserve mint and token program match the live on-chain
  `BridgeConfig`;
- SolanaReserve capacity holds: `balance - protected_minimum -
  reserved_liquidity - other open refunds >= refund amount` (stricter
  than the on-chain floor, which only knows `protected_minimum`);
- **the bridge is already globally paused on-chain** (execute only; a dry
  run reports the pause state but does not require it).

### Amount and destination — both derived, never entered

The refund is the **exact gross deposited amount**, in the reserve mint's
own atomic units, taken from the on-chain `WithdrawalObligation.amount`.
No fee is deducted: the 3% SolToGlc bridge fee accrues only inside
`mark_goldcoin_completion_confirmed` (docs/20-bridge-fee.md), which a
refunded request never reaches, so there is no accrued fee to net off and
none is invented.

The destination is the canonical **Token-2022** ATA of
`(WithdrawalObligation.requester, reserve mint, reserve token program)` —
derived from on-chain data the bridge itself verified, which is by
construction the same account the deposit came from (the on-chain
`deposit_to_reserve` instruction constrains the source to exactly that
ATA and records `requester` from the deposit's own `Signer`). **There is
deliberately no `--destination` flag**; an operator cannot direct a refund
anywhere else. If that ATA no longer exists, the refund transaction
creates it idempotently, submitter-paid, in the same atomic transaction
(the identical pattern normal releases already use).

### Authorization and fund movement

Reuses the existing operator-withdrawal rail with nothing weakened:
`rebalance_withdraw` (see RESERVE_EMERGENCY_WITHDRAWAL_RUNBOOK.md) — the
admin's signature **and** a threshold (2-of-3 pilot) ed25519 attestation
over the canonical claim, the on-chain global-pause precondition, the live
`protected_minimum` check, `transfer_checked` via the reserve-authority
PDA, and a per-nonce `rebalance_withdrawal` PDA replay guard. Attestation
signatures come from the configured signer endpoints exactly as the
daemon's own settlement path collects them — in production no attestation
key ever exists on the machine running this command. **No on-chain program
change was needed or made.**

The refund nonce is `(1 << 63) | request_id` — a dedicated refund domain
that can never collide with ordinary rebalance nonces (small counters or
timestamps). One request maps to exactly one nonce forever, so its PDA is
a per-request, on-chain replay guard that holds even against a database
restored from an old backup.

### Dry run (always do this first)

```
glc-admin refund-manual-review --config PATH --request-id N --note "why this is being refunded"
```

Prints: request id and state, the manual-review reason, the original
deposit (obligation index + PDA — **the bridge stores no deposit
transaction signature anywhere; the finalized obligation account *is* the
verified deposit record**), the original sender wallet, the source token
account, the derived refund destination and whether it exists, mint, token
program, the exact refund amount and the fee interpretation, whether a
Goldcoin payout exists, whether a prior refund exists, reserve balance
before/after, the protected minimum, the pause state, the attestation
threshold, and every safety check individually as PASS/FAIL with an
overall verdict.

**Verify the destination independently** before executing: derive
`ATA(requester, reserve mint, Token-2022)` yourself — e.g.
`spl-token address --owner <REQUESTER> --token <RESERVE_MINT> --program-2022`
— and confirm it equals the printed destination, and that the printed
requester matches the depositor you expect from the original on-chain
deposit transaction.

### Execute

```
glc-admin onchain-pause --rpc-url URL --keypair ADMIN_KEY --scope global --note "manual review refunds"
glc-admin refund-manual-review --config PATH --request-id N --note TEXT --keypair ADMIN_KEY --execute
# ... repeat per request; each is individually checked and idempotent ...
glc-admin onchain-unpause --rpc-url URL --keypair ADMIN_KEY --scope global --note "refunds complete"
```

The pause is **never** engaged or lifted by the refund command itself —
that stays an explicit, separately audited operator action, so the
security boundary is visible in the audit log rather than implied.

Execution order, each database step atomic with its own audit row:
re-check everything against fresh state -> record `RefundPending` ->
collect attestations -> **re-check global pause, protected minimum, and
nonce immediately before simulating** -> simulate (a failed simulation
blocks the broadcast unconditionally, `--execute` or not) -> record the
signature and blockhash **before** sending -> broadcast -> confirm at
`finalized` -> `Refunded` + debit the cached SolanaReserve balance.

### Re-running, crash recovery, and rollback expectations

Safe to re-run at any point; it never resolves uncertainty by building a
second transfer:

- **Already `Refunded`** — reports the existing transaction and exits 0.
- **`RefundBroadcast`** — reads the on-chain state back. If the refund's
  nonce PDA exists (and matches this refund's amount/destination), the
  transfer happened: it finalizes the bookkeeping. If not, and the
  recorded blockhash is still landable, it waits for a definite outcome.
  Only once the recorded transaction is *positively* dead (blockhash can
  no longer land **and** no nonce PDA, or it landed and failed) does it
  rebuild — under the **same** nonce.
- **`RefundPending`** — resumes from attestation collection.
- A crash between recording the broadcast and the actual send is the same
  case: the recorded intent plus the nonce PDA make the outcome
  determinable.

There is no "undo": once the transfer confirms, the funds are with the
depositor and the request is terminal. If a refund is broadcast in error,
the compensating action is a new, ordinary deposit by that party — not a
database edit. A refund that has *not* yet broadcast can simply be left
alone (the request stays `RefundPending` and inert; nothing else will ever
act on it).

### Verifying the request is permanently closed

- `glc-admin refund-list --db PATH` shows the row as `Confirmed` with its
  transaction signature; `glc-admin status --db PATH` no longer counts the
  request in the `ManualReview` backlog.
- `glc-admin resume-manual-review --db PATH --request-id N --note TEXT`
  refuses with a refund-lifecycle error — from **any** surface (CLI, admin
  API, or the daemon's automatic recovery), since all three call the same
  ledger function. The refusal keys on the `solana_refunds` row itself, so
  it holds even if `bridge_requests.state` were edited out of band.
- No Goldcoin payout can be created for the request: the guard sits in
  `Ledger::record_goldcoin_payout_built`, the single point every payout row
  is born, not only in the CLI.
- The refunded amount is debited from the cached SolanaReserve balance in
  the same transaction that marks it `Refunded`, and a
  broadcast-but-unconfirmed refund is an explicit in-flight explanation
  term in reconciliation — so a refund never trips the unexplained-drop
  auto-pause, and never hides a real one.

### Accounting

A fold-time park never reserved Goldcoin liquidity, so a refund releases
nothing there — and this is *proved* per request (the state-log check
above) rather than assumed; a request that ever held a reservation is
refused instead of blindly subtracted from. `reserved_liquidity`,
`pending_obligations`, `settled_liquidity_total`, and `accrued_fees_atomic`
are all untouched by the refund path: a refund is not a settlement.

### Batch refunds

Not implemented, deliberately. Drain a backlog with `refund-list` +
per-request dry run + per-request `--execute`; each request is checked and
made idempotent on its own. There is no `refund-all`.

### NEVER do this instead

**Do not** send a manual SPL/Token-2022 transfer from the reserve and then
edit the database to match. A hand-made transfer bypasses every guard
above — the attestation threshold, the protected-minimum check, the
eligibility whitelist, the replay guard, the audit trail — and a
hand-edited row will not release/close the request correctly, will not be
recognized by reconciliation (it will surface as an unexplained balance
drop and auto-pause the reserve), and destroys the request/deposit/refund
linkage an auditor needs. If this command refuses, the refusal is the
answer: fix the named cause, or escalate — never route around it.

### Schema rollback (v17) — read before rolling back a release

The refund feature adds schema **v17** (`solana_refunds`). Migration is
automatic on first daemon start, additive only (`CREATE TABLE IF NOT
EXISTS` — no table rebuild, no column rewrite, no data movement), and
touches no existing table, so upgrading is safe and re-runnable.

**Rolling BACK to a pre-v17 binary is not supported and must not be done
casually**, for two reasons established by inspection, not assumption:

1. A pre-v17 binary does not know the `RefundPending`/`RefundBroadcast`/
   `Refunded` state strings. Any read that parses a refunded request's
   row fails with `InvalidColumnType` — specifically `Ledger::get_request`
   and `Ledger::transfers_page`, i.e. the public `GET /transfers/{id}` and
   `GET /transfers?address=...` endpoints and the admin API's
   request-detail reads, for the affected requests only. **Settlement is
   unaffected**: every daemon loop selects by an explicit state
   (`requests_by_state`) or off `goldcoin_payouts`, and reserve accounting
   uses aggregate SQL, so none of them ever parse a refund state. Verified
   empirically, not inferred.
2. A pre-v17 binary's migration ladder has no forward-compatibility guard,
   so it would silently `UPDATE schema_version SET version = 16` on a v17
   database — relabelling it as older while it still physically carries
   `solana_refunds` and its rows. No data is lost (rolling forward
   re-applies v17 idempotently), but the version marker would be wrong in
   the meantime.

From v17 onward this is prevented: `schema::open_and_migrate` refuses to
open any database whose `schema_version` exceeds the running binary's
`CURRENT_SCHEMA_VERSION` (`LedgerError::SchemaTooNew`) rather than
stamping an older version over it. That guard protects every future
rollback; it cannot retroactively protect a rollback to a binary that
predates the guard itself.

**If a rollback past v17 is genuinely required**: stop the daemon, and
restore a pre-upgrade backup with `scripts/restore-ledger.sh` rather than
pointing the old binary at the current database. Any refund executed after
the upgrade is a real, irreversible on-chain transfer — a restored older
database will not contain its record, so reconcile those refunds manually
(they are visible on-chain as `rebalance_withdrawal` PDAs under the refund
nonce domain, and in `admin_audit_log` in the un-restored database) before
resuming operation.

### Regression coverage

`ledger::tests` (eligibility, whitelist, cross-checks, capacity,
lifecycle, restart, concurrent-begin, resume/payout guards),
`solana::refund::tests` (dry-run purity, exactly-one-transaction,
Token-2022 ATA derivation, idempotent rerun, crash recovery in all three
shapes, simulation-blocks-broadcast, pause re-check, wrong mint/program,
on-chain settlement evidence, insufficient reserve, nonce-without-row),
`orchestrator::tests` (auto-resume never revives a refunded request and is
not stalled by one), and `ledger::schema::tests` (v17 migration, its
constraints, and the forward-compatibility guard refusing a newer-than-
supported database instead of downgrading it).

## Wallet uniqueness: one rolling 24-hour window per wallet, on every route (generalized 2026-09-12)

### The rule

On every one of the six routes, BOTH the source wallet and the destination wallet may be used at most once inside a rolling 24-hour window (`Ledger::WALLET_WINDOW_SECS`, 86,400 seconds — the same constant the two sections below have always used). A new bridge attempt whose source wallet, OR whose destination wallet, already backs a request created inside the window is never admitted into the normal payout flow:

- if the source deposit is already on-chain (every fold; a Goldcoin deposit being observed), it is recorded with its full evidence and parked in `ManualReview` under `manual_review_note = "wallet_source_24h_limit"` or `"wallet_destination_24h_limit"` (source first when both apply), refundable, never auto-paid;
- if nothing is on-chain yet (`POST /transfers` on `GlcToSol`/`GlcToRhn`), the request is refused with `429` before any capacity is reserved — no row, no reserved liquidity, no derived address.

Once 24 hours have passed since the prior qualifying request's `created_at`, the wallet is eligible again. This is a cooldown, not permanent uniqueness. "Qualifying" is the exclude-list the two older sections document (`Ledger::RATE_LIMIT_EXCLUDED_STATES_SQL_IN`): `AwaitingDeposit`, `Confirming`, `SourceFinalized`, every payout/settlement-in-progress state, `Settled`, `ManualReview` (any reason) and the whole refund lifecycle all consume the window; only the terminal never-paid states (`Failed`, `DestinationSubmissionFailed`, `InsufficientReserveAtSettlement`, `Cancelled`, `Expired`, `Reorged`) do not.

### One mechanism (`service/src/ledger/wallet_window.rs`)

The Goldcoin-bound rule described in the next two sections was three near-identical queries reading three spellings of "the wallet" (`recipient`; `requester`; a join to `robinhood_deposit_observations.depositor`). It is now ONE query, `Ledger::wallet_window_blocker_created_at`, parameterized by the wallet's chain and its role, keyed on two columns:

| role | column | rows that consume the window |
|---|---|---|
| source | `bridge_requests.source_wallet` (schema v28) | every direction whose SOURCE chain is the wallet's chain |
| destination | `bridge_requests.recipient` | every direction whose DESTINATION chain is the wallet's chain |

`source_wallet` is written by every fold in the same statement as the row (Solana: the on-chain `requester`; Robinhood: the contract's recorded `depositor`; Goldcoin: the funding address traced from the deposit's own inputs, see below) and backfilled by v28 for every pre-existing Solana/Robinhood row from where each route used to keep it. Every enforcing path — the four folds, `create_request_from`, `record_glc_deposit_observed_from`, and both resume bodies — runs the check inside the same `BEGIN IMMEDIATE` transaction as the row it inserts or transitions, so two attempts sharing a wallet can never both be admitted: SQLite's write lock serializes them and the second always sees the first's committed row. The read-only views (`Ledger::route_wallet_eligibility`, the API below) run the identical query and are purely advisory.

**Scope: per wallet on its chain, across the routes sharing that chain.** A Goldcoin address that just received a `SolToGlc` payout is busy for `RhnToGlc` (unchanged); a Solana wallet that just funded `SolToGlc` is busy for `SolToRhn`; a Solana pubkey that just received a `GlcToSol` release is busy for `RhnToSol`; an EVM address paid by `GlcToRhn` is busy for `SolToRhn`; an EVM depositor on `RhnToGlc` is busy for `RhnToSol`. This is the destination rule's long-standing "property of the address, not of the counterpart chain" decision applied to every chain and both roles — a strict superset of a per-route check. Windows are never pooled ACROSS chains (a 20-byte EVM address is only ever compared against Robinhood-scoped rows). The six direction sets are literals in `Ledger::wallet_window_directions_sql_in`, pinned to `Direction::source_chain`/`destination_chain` by `ledger::wallet_window::tests::the_direction_scope_literals_match_the_chain_predicates`.

### Route-by-route enforcement

| route | source key | destination key | enforced at | on a duplicate |
|---|---|---|---|---|
| `GlcToSol` | Goldcoin address funding the deposit (traced from the deposit tx's inputs; a caller may DECLARE one up front) | Solana pubkey | `POST /transfers` → `Ledger::create_request_from` (destination always; source if declared); then `goldcoin::indexer` → `Ledger::record_glc_deposit_observed_from` (every traced input wallet + destination, against every other request) | `429` at create; `ManualReview` at observation (refundable via `glc-admin refund-glc-manual-review`, reason on `REFUNDABLE_GLC_MANUAL_REVIEW_REASONS`) |
| `GlcToRhn` | same as above | EVM address | same as above | same as above |
| `SolToGlc` | on-chain `WithdrawalObligation.requester` | Goldcoin address | `Ledger::fold_sol_deposit` | `ManualReview`, resumable (auto-resume) / refundable |
| `SolToRhn` | on-chain `requester` | EVM address | `Ledger::fold_sol_deposit_to_robinhood` | `ManualReview`, resumable (`resume_manual_review_cross_route`) / refundable |
| `RhnToGlc` | contract-recorded `depositor` | Goldcoin address | `Ledger::fold_robinhood_deposit` | `ManualReview`, resumable (auto-resume) / refundable |
| `RhnToSol` | contract-recorded `depositor` | Solana pubkey | `Ledger::fold_robinhood_deposit` | `ManualReview`, resumable (`resume_manual_review_cross_route`) / refundable |

**Goldcoin-sourced routes, specifically.** A `GlcToSol`/`GlcToRhn` request exists before its deposit does, so the source is enforced twice. `POST /transfers` accepts an optional `source_address` (P2PKH or P2SH on this network): when given it is checked and stored so the window is consumed from admission. It is a claim, not evidence — when the deposit lands, `goldcoin::indexer::Indexer::trace_funding_wallets` fetches every input's prevout script (one `getrawtransaction` per input; an unservable prevout fails the tick rather than recording an untraced deposit; a coinbase input has no wallet), spells each as the address it pays (raw script bytes for a non-standard script), and the ledger checks EVERY input wallet against every other request in the window and records input 0's as the row's `source_wallet`. The destination is re-checked at observation too, excluding the request's own row, so a late deposit to a destination that was reused while the request sat `Expired` parks rather than pays. A Goldcoin-sourced park keeps its reservation until the refund releases it, like every other Goldcoin park; there is no resume path for one.

### Resume, auto-resume, refund

Both resume bodies (`resume_manual_review_inbound` for the Goldcoin-bound routes, `resume_manual_review_cross_route` for `SolToRhn`/`RhnToSol`) re-check BOTH windows unconditionally through `Ledger::resume_wallet_windows`, strict-predecessor-only, refusing with `LedgerError::WalletWindowActive { role, chain, wallet, retry_after }` (one variant for every chain and role; it replaces `RecipientRateLimited`/`SourceWalletRateLimited`/`RobinhoodSourceWalletRateLimited`). A resume of a row with no `source_wallet` recorded fails closed. The daemon's auto-resume pass (Goldcoin-bound routes, unchanged in scope) treats both reasons as self-clearing and skips a still-blocked candidate without stalling the batch. Both reasons are on `RECOVERABLE_MANUAL_REVIEW_REASONS`, `REFUNDABLE_MANUAL_REVIEW_REASONS` and `REFUNDABLE_GLC_MANUAL_REVIEW_REASONS`.

**Legacy spellings.** Rows parked before this change carry `recipient_rate_limited` / `source_wallet_rate_limited`. Those strings are never written any more but are RECOGNIZED by every list and filter above (`Ledger::is_wallet_window_manual_review_reason`), so an existing park keeps every exit — nothing on a production row is rewritten by this change.

### Pre-transaction eligibility read, every route

`GET /routes/{route}/eligibility?source=<address>&destination=<address>` (`route` in `Route::as_str` spelling, e.g. `RhnToSol`; at least one of the two query parameters; each validated as ITS chain's address type — a malformed one is `400`) answers, read-only, whether a new request on that route from `source` to `destination` would be admitted right now: `{route, source: {address, eligible, reason, retry_after, retry_after_seconds} | null, destination: {…} | null, eligible, blocked_reason, blocked_reasons, retry_after, retry_after_seconds, window_seconds, as_of}`, with `reason`/`blocked_reason` ∈ `wallet_source_24h_limit` | `wallet_destination_24h_limit` (source first when both). A UI must call it before asking the user to sign anything on the source chain, and re-check immediately before submission; the backend enforces regardless. `POST /transfers`'s `429` body carries the same `blocked_reasons` and `retry_after`. The two older endpoints (`GET /recipients/{sol,rhn}-to-glc/eligibility`) are unchanged in shape and vocabulary and now read the same query.

### Tests

`service/src/ledger/wallet_window/tests.rs` runs every scenario for every one of the six routes through the route's real admission path: same source twice inside 24h blocked, same destination twice blocked, different source and destination admitted, same wallets admitted again at exactly the 86,400th second (and blocked again after that — a rolling window, not a bucket), two simultaneous attempts from two connections admitting exactly one, an already-on-chain duplicate parked with the explicit reason and no payout capacity (Goldcoin routes: parked at observation with refundable evidence), the late-deposit destination case, every traced Goldcoin input checked, an untraceable deposit recorded without a source, event replay idempotent on every route, the read-only views agreeing with admission at every boundary, resume refusing until both windows clear on every fold route, oldest-first draining on a cross route, legacy-spelled parks keeping their exits, and the direction-scope literals pinned. `service/src/goldcoin/indexer/tests.rs` covers the prevout tracing end to end (recorded wallet, second deposit from one wallet parked, multi-input deposits, an unservable prevout failing the tick, a coinbase-funded deposit). `service/src/api/tests.rs` covers the new endpoint on every route (fresh, each blocked leg with its reopen time, per-chain validation, read-only, HTTP routing) and `POST /transfers` (declared source recorded, `429` with body for a busy destination or source, nothing reserved). `service/src/ledger/schema.rs` `v28_tests` cover the column, the index, the backfill and its idempotence.

## Goldcoin destination rate limit (added 2026-08-27; made route-global 2026-09-10)

> Since 2026-09-12 this is the destination half of the route-generic wallet uniqueness rule above, served by the same one query; the park reason is now `wallet_destination_24h_limit` (the `recipient_rate_limited` spelling on older rows is still recognized everywhere). Everything below about scope, the exclude-list, refunds and oldest-first draining still holds verbatim.

### The rule

A Goldcoin L1 recipient address may receive at most one accepted bridge payout in a rolling 24-hour window (`Ledger::RECIPIENT_RATE_LIMIT_WINDOW_SECS`, 86,400 seconds), **from any inbound route**. The window starts at the first accepted request's `created_at`. Any new inbound obligation to the same recipient inside that window is parked in `ManualReview` with `manual_review_note = "recipient_rate_limited"` instead of proceeding to payout — checked by `Ledger::fold_sol_deposit` and `Ledger::fold_robinhood_deposit` before any reservation is made, so a rate-limited fold never consumes reserve capacity. Different recipient addresses are completely independent. `GlcToSol`/`GlcToRhn` are unaffected: their `recipient` is a Solana pubkey or an EVM address, and the direction predicate excludes them explicitly regardless.

**The window is GLOBAL across `SolToGlc` and `RhnToGlc` (2026-09-10).** It was originally scoped to `SolToGlc` alone, because that was the only inbound route that existed. When `RhnToGlc` shipped, a per-route reading would have meant one address could collect one payout *per inbound chain* per day — two payouts, not one — which is not the rule. The rule is a property of the destination ADDRESS, not of the funding chain, so:

- a recent `SolToGlc` payout to address X blocks `RhnToGlc` to X until the window expires, **and**
- a recent `RhnToGlc` payout to address X blocks `SolToGlc` to X until the window expires.

The direction predicate is `Direction::DESTINATION_IS_GOLDCOIN_SQL_IN`, which lives beside `Direction::destination_is_goldcoin` and is pinned to it by `ledger::tests::destination_is_goldcoin_sql_in_matches_the_rust_predicate` — a fifth inbound-to-Goldcoin route added later cannot silently get a window of its own.

"Accepted" is an exclude-list, not an include-list: every inbound row created inside the window counts (`SourceFinalized`, `ManualReview` for any reason, `SettlementAuthorized`, `DestinationSubmitted`, `DestinationConfirmed`, `Settled`, and the whole refund lifecycle) EXCEPT the terminal states that mean no payout resulted or ever will (`Failed`, `DestinationSubmissionFailed`, `InsufficientReserveAtSettlement`, `Cancelled`, `Expired`, `Reorged`) — a request that never created a real obligation, or that failed/was cancelled before one completed, does not count against the recipient. A request already sitting in `ManualReview` for some other reason (e.g. `utxo_liquidity_low_at_fold`) still counts, since it remains a live obligation that can still result in a payout.

**A REFUNDED request still consumes the full window**, on both routes. `RefundPending`/`RefundBroadcast`/`Refunded` are deliberately absent from the exclude-list: a refund means the service declined to complete the transfer, not that the deposit never happened, and letting a refund reset the window would hand an abuser a free retry on demand. This has always been the `SolToGlc` behaviour and `RhnToGlc` inherits it exactly. The exclude-list is a single interpolated literal, `Ledger::RATE_LIMIT_EXCLUDED_STATES_SQL_IN`, shared by all six window queries and pinned by `ledger::tests::the_rate_limit_exclude_list_names_exactly_the_terminal_no_payout_states`.

### Manual resume cannot bypass the window

`Ledger::resume_manual_review_sol_to_glc` and `Ledger::resume_manual_review_rhn_to_glc` — two thin wrappers over ONE shared body, `Ledger::resume_manual_review_inbound`, so no check can ever apply to one route and not the other — re-check the SAME rate limit unconditionally on every resume attempt — regardless of the request's own `manual_review_note`. An operator cannot resume a request early just because it happened to be parked for a different reason; if the recipient is still inside its window (because of some OTHER request to the same address), the resume is refused with `LedgerError::RecipientRateLimited { retry_after, .. }`, leaving the request untouched. Retrying the identical command once `retry_after` passes succeeds normally — a transient, self-clearing refusal, exactly like `LedgerError::UtxoLiquidityLow`.

**Only a strict predecessor can ever block a candidate.** The blocker search is restricted to rows ordered `(created_at, id)` strictly BEFORE the candidate's own — never a later-arriving sibling, and never itself. This is what makes "oldest first" actually true for a recipient with several queued rows: candidate `C`'s eligibility can only ever depend on the request immediately before it in creation order, never on anything that showed up after `C` did. Without this restriction, a later-arriving sibling (necessarily still parked, since it too was rate-limited on arrival) could shadow-block an earlier, rightfully-next-in-line candidate — inverting the drain order, and under a steady trickle of new same-recipient arrivals, potentially starving the oldest parked request indefinitely. (This was a real HIGH-severity finding, fixed before merge — see `service/src/ledger/mod.rs`'s `resume_manual_review_sol_to_glc` doc comment.)

### Automatic resume

Once the blocking predecessor ages out of the 24-hour window, a `recipient_rate_limited` request resumes automatically, oldest first, subject to every normal safety check — see "Automatic recovery, without an operator" above. Because eligibility only ever depends on a candidate's immediate predecessor, a backlog of several queued rows to the same recipient drains strictly in creation order: the oldest becomes eligible first (once its predecessor's window clears), the next becomes eligible only once *that* one's own window clears in turn, and so on — never out of order, and never blocked by anything newer. No operator action is required in the common case; `glc-admin resume-manual-review` remains available for the same request and will simply refuse (not error out destructively) if its predecessor's window has not actually cleared yet.

### Pre-transaction eligibility read (added 2026-08-27, extended 2026-08-28)

`GET /recipients/sol-to-glc/eligibility?address=<Goldcoin p2pkh address>&wallet=<base58 Solana pubkey, optional>` answers, read-only, whether a NEW SolToGlc obligation naming that recipient — and, when `wallet` is given, deposited from that Solana wallet — would currently be admitted or parked by EITHER rate limit: `{direction, address, wallet, eligible, blocked_reason, blocked_reasons, retry_after, retry_after_seconds, source_wallet_retry_after, recipient_retry_after, window_seconds}`, where `blocked_reason` is `"source_wallet_rate_limited"` or `"recipient_rate_limited"` (`null` when eligible, checked wallet-first when both would block — see the next section), `retry_after` is the absolute unix second the blocking window reopens (`null` when eligible), and `retry_after_seconds` the same instant as remaining seconds. `wallet` is optional — omitting it means only the recipient leg is checked, same as before this dual limit existed. It is served by `Ledger::goldcoin_recipient_rate_limited_until` and `Ledger::sol_to_glc_source_wallet_rate_limited_until`, each running the SAME shared window query its respective admission check uses, so the answer can never disagree with what admission would actually do.

`GET /recipients/rhn-to-glc/eligibility?address=<Goldcoin p2pkh address>&wallet=<0x EVM address, optional>` is the exact twin for the Robinhood route, added 2026-09-10. Same response shape (one `RecipientEligibility` type, assembled by one `RecipientEligibility::from_windows`, so the precedence and the retry arithmetic exist in exactly one place), same `blocked_reason` values, same `window_seconds`, same optional `wallet` leg, same trimming, same `decode_p2pkh` validation of the address. It differs only in `direction` (`"RhnToGlc"`), in reading the wallet as a `0x` EVM address through `EvmAddress`'s strict `FromStr` (exactly 40 hex digits, `0X` refused, EIP-55 checksum verified whenever the digits mix case — a malformed wallet is a 400, never a padded or truncated blob that would then be asked about someone else's window), and in consulting `Ledger::rhn_to_glc_source_wallet_rate_limited_until` for the wallet leg. The RECIPIENT leg is the SAME call on both endpoints, so a Goldcoin address blocked by a recent `SolToGlc` payout reports as blocked on the Robinhood endpoint too, and vice versa.

**Three response fields were added, additively, for the both-blocked case**: `blocked_reasons` (every applicable reason, source wallet first — `[]` when eligible) and `source_wallet_retry_after`/`recipient_retry_after` (per-leg reopen instants, `null` where that leg is not blocking or was not evaluated). `blocked_reason` and `retry_after` keep their exact original single-reason, wallet-first meaning on BOTH endpoints, so an existing client reading only those is unaffected. `blocked_reason` names the reason a real fold would have recorded as its `manual_review_note`; `blocked_reasons` is for a UI that wants to tell the user everything they must wait for.

**Both endpoints answer about RATE LIMITS ONLY.** Neither says anything about whether the route is open, the reserve is funded, or the chain adapter is operational — `GET /chains` and `GET /robinhood/reserve` own those questions, and a deposit can still be parked for one of those reasons after eligibility said "eligible". Neither endpoint mutates anything, reserves capacity, or consumes the cooldown it reports on: `api::tests::rhn_eligibility_is_read_only_and_consumes_no_cooldown` snapshots `bridge_requests`, `reserve_ledger`, `robinhood_deposit_observations` and `bridge_request_state_log` around repeated calls across every branch of the handler and asserts they are byte-for-byte unchanged. The bridge UI calls it as soon as a wallet is connected AND a valid Goldcoin destination address is entered, AND again immediately before invoking the wallet, so a user is warned before signing a Solana transaction whose deposit would only be parked. Purely advisory by construction: admission re-checks both rules at fold time inside the write transaction, so a stale or bypassed answer here can never weaken either limit, and the endpoint discloses nothing beyond the boolean, which limit is blocking, and the reopen time (no blocking request id, amount, or state).

### Race-safety

The rate-limit query, the row insert (`fold_sol_deposit`) or state update (`resume_manual_review_sol_to_glc`), and the reservation increment all run inside the SAME `BEGIN IMMEDIATE` SQLite transaction — SQLite's write lock serializes every mutating ledger call DB-wide, so two concurrent obligations to the same recipient can never both observe "no blocking row yet" and both proceed; the second one always sees the first's already-committed row. The source-wallet limit below shares this same guarantee, keyed on `requester` instead of `recipient`.

### Regression coverage

`service/src/ledger/tests.rs`: same recipient inside 24h (parked), same recipient after 24h (accepted), different recipients (independent), restart/idempotency, an in-flight `ManualReview`/`DestinationSubmitted`/`Settled` obligation all counting against the recipient, a cancelled/failed obligation never counting, manual resume refusing while still inside the window, GlcToSol completely unaffected — plus, for the predecessor-only ordering fix specifically: three queued requests to the same recipient resuming strictly oldest-first, the newest of the three remaining blocked until the middle one's own window elapses, a flood of newer same-recipient arrivals never starving the oldest parked request, and that ordering surviving a simulated restart. `service/src/orchestrator/tests.rs`: automatic resume draining a `recipient_rate_limited` entry once its window clears, and a still-rate-limited candidate being skipped (not a batch stop) in a mixed-reason batch. See "SolToGlc source-wallet rate limit" below for the dual-key regression coverage.

## Source-wallet rate limits, per source network (Solana added 2026-08-28; Robinhood added 2026-09-10)

> Since 2026-09-12 this is the source half of the route-generic wallet uniqueness rule above: keyed on `bridge_requests.source_wallet` (v28) on every route, spanning both routes each source chain feeds (`SolToGlc`+`SolToRhn`, `RhnToGlc`+`RhnToSol`, and — new — the Goldcoin-sourced `GlcToSol`+`GlcToRhn`); the park reason is now `wallet_source_24h_limit` (`source_wallet_rate_limited` on older rows is still recognized everywhere).

### The rule

A single Solana source wallet could bypass the recipient-only limit above by spreading deposits across many different Goldcoin recipients — the recipient limit alone never noticed, since each individual recipient was still fresh. This adds a SECOND, INDEPENDENT rolling-24-hour limit, keyed by the Solana wallet instead of the Goldcoin recipient, enforced ALONGSIDE the recipient limit (never replacing it): a Solana source wallet may make at most one qualifying SolToGlc deposit in a rolling 24-hour window (`Ledger::RECIPIENT_RATE_LIMIT_WINDOW_SECS`, the SAME 86,400-second constant, shared by both limits so they cannot drift apart on the window itself). Any new SolToGlc obligation from the same wallet inside that window is parked in `ManualReview` with `manual_review_note = "source_wallet_rate_limited"` — checked by `Ledger::fold_sol_deposit` before any reservation is made, using the identical exclude-list and matching semantics as the recipient rule (`Ledger::source_wallet_rate_limit_blocker_created_at`, the structural mirror of `recipient_rate_limit_blocker_created_at`). GlcToSol is unaffected, for the same reason as the recipient limit — this only ever runs in `fold_sol_deposit`.

**Identity, not a client-supplied string.** The wallet key is `requester`, decoded straight from the on-chain `WithdrawalObligation` account (`solana::accounts::decode_withdrawal_obligation`, offset 16, immediately after `index`/`amount`) — which the `glc-reserve-bridge` Anchor program itself sets to `ctx.accounts.user.key()`, the `Signer` that authorized the `deposit_to_reserve` instruction (`programs/glc-reserve-bridge/src/instructions/deposit_to_reserve.rs`). There is no code path by which a caller can set this to anything other than the wallet that actually signed the deposit — it is threaded verbatim from that on-chain account through `solana::indexer::tick`'s `snap.requester.to_bytes()` into `fold_sol_deposit`'s `requester: [u8; 32]` parameter, never taken from request headers, form input, or any other client-controlled source.

### The Robinhood/EVM twin (added 2026-09-10)

`RhnToGlc` gets the same limit, keyed by the Robinhood source wallet: **a Robinhood/EVM source wallet may make at most one qualifying `RhnToGlc` deposit in a rolling 24-hour window** — same `Ledger::RECIPIENT_RATE_LIMIT_WINDOW_SECS` constant, same shared state exclude-list, same strict-predecessor rule, same `manual_review_note = "source_wallet_rate_limited"`, checked by `Ledger::fold_robinhood_deposit` before any reservation is made.

**The two source-wallet windows are INDEPENDENT and are never pooled.** A Solana pubkey and a 20-byte EVM address are different kinds of identity, held by key material on different chains, and this service has no way to know whether two of them belong to the same person. So a Solana wallet's window is never charged to an EVM wallet and vice versa — `Ledger::source_wallet_rate_limit_blocker_created_at` (scoped `SolToGlc`, matching `bridge_requests.requester`) and `Ledger::rhn_source_wallet_rate_limit_blocker_created_at` (scoped `RhnToGlc`) match different columns in different tables under different direction predicates. This is the deliberate opposite of the DESTINATION rule, which is global precisely because a Goldcoin address is one identity however it is reached.

**Identity, not a client-supplied string — the Robinhood side.** The wallet key is `robinhood_deposit_observations.depositor`, decoded by `robinhood::indexer` from the custody contract's own finalized `DepositCreated` log. A Robinhood fold deliberately leaves `bridge_requests.requester` NULL (that column is a fixed 32-byte Solana pubkey, and a 20-byte EVM address is not one), so the depositor is read back through the observation's `folded_request_id` link — the same path `Ledger::transfers_page`'s `RhnToGlc` leg already uses, including its `finality <> 'Reorged'` exclusion. That link is written in the same transaction as the insert it names, immediately after it, so a fold's own admission check (which runs before its insert) can never match the row it is about to create. **No schema migration was required**: every column already existed.

### Manual resume cannot bypass either window

`Ledger::resume_manual_review_sol_to_glc` and `Ledger::resume_manual_review_rhn_to_glc` re-check BOTH rate limits unconditionally on every resume attempt, independently — an operator cannot resume a request early because it happens to be clear of one limit while the other still applies. If the source wallet is still inside its window, the resume is refused with `LedgerError::SourceWalletRateLimited { retry_after, .. }` on `SolToGlc` or `LedgerError::RobinhoodSourceWalletRateLimited { retry_after, .. }` on `RhnToGlc` (checked first; two variants so the message can never name the wrong chain's wallet); if only the recipient is still inside its window, `LedgerError::RecipientRateLimited` (unchanged from before, on either route).

Both entry points are thin wrappers over ONE body, `Ledger::resume_manual_review_inbound`. `expected_direction` is the only policy input; every other check — recoverable-reason list, state, finalized source, no existing payout, refund lifecycle, UTXO floor, reserve invariant, admission safety buffer, and the two rate-limit re-checks — is the same code for both routes, so none of them can drift onto one route only. The three genuinely per-route parts are the wrong-direction error, where the durable refund marker lives (`solana_refunds` vs. a `Refund`-kind row in `robinhood_transactions`), and which source-wallet limiter applies. `glc-admin resume-manual-review` reads the route from the request itself, so one command covers both. Same strict-predecessor-only blocking rule as the recipient limit (a resume candidate can only ever be blocked by an earlier row from the SAME wallet, ordered `(created_at, id)`), for the identical oldest-first-draining reason documented above.

### Automatic resume

Covered by the same `Orchestrator::tick_auto_resume_utxo_liquidity_backlog` pass as the recipient limit (see above) — a `source_wallet_rate_limited` candidate is drained automatically once its wallet's window clears, and skipped (never a batch stop) while it doesn't, exactly like `recipient_rate_limited`.

**The pass sweeps BOTH inbound routes, in one global order (2026-09-10).** Because the destination window is global across `SolToGlc` and `RhnToGlc`, a backlog to one Goldcoin address is ONE queue spanning both routes, not two independent ones. The pass therefore collects candidates from both directions and sorts them by `(created_at, id)` before draining, which is what makes the strict-predecessor rule produce true oldest-first ordering across routes as well as within one. Two separate per-direction passes would have drained them out of order. This is also what makes the Robinhood limits semantically EQUAL to the Solana ones rather than merely stricter: a rate-limited `RhnToGlc` park is a self-clearing 24-hour hold, not a hold until a human notices — without it, the only exit from such a park would have been `glc-admin robinhood-refund`.

### UI enforcement (added 2026-08-28)

The bridge UI's `GET /recipients/sol-to-glc/eligibility?wallet=` leg (see "Pre-transaction eligibility read" above) surfaces this to the user BEFORE Phantom is ever invoked: "This Solana wallet has already used the bridge in the last 24 hours." — shown alone (never alongside the recipient message) when the connected wallet itself is blocked, disabling the Deposit button and re-checked fresh immediately before submission so a wallet that becomes rate-limited between form-fill and click still never reaches the wallet-open step. The backend remains the sole enforcing authority regardless: a direct `deposit_to_reserve` observation that bypasses the UI/API preflight entirely is still independently caught by `fold_sol_deposit` and safely parked in `ManualReview`, never silently accepted.

### Regression coverage — Robinhood (added 2026-09-10)

`service/src/robinhood/fold/tests.rs` owns the cross-route contract, because it is the one place with a harness for both folds: a second `RhnToGlc` deposit to the same Goldcoin address inside 24h (parked), different destinations independent, the exact 86,400-second boundary on both windows, a second deposit from the same EVM wallet to a DIFFERENT address (parked — the bypass the wallet limit closes), a different EVM wallet unaffected, wallet-reason outranking recipient-reason, **a `SolToGlc` payout blocking `RhnToGlc` to the same address and vice versa**, a cross-route block never reaching a different destination, source-wallet windows never pooled across networks (in both directions), a deliberately byte-confusable 20-byte EVM address never colliding with a 32-byte Solana requester, all six terminal states never counting, a **refunded** deposit still consuming both windows (and the window still expiring), a rate-limited park holding no reserve capacity, a replay never being reinterpreted as a rate-limit hit, a park being itself a blocker, resume-after-expiry, resume reserving exactly once and being idempotent, resume refusing a live source-wallet window, the resume re-check firing even when the park's own reason was something else, a refund lifecycle being permanently unresumable, each resume entry point refusing the other route, a mixed-route backlog to one address draining strictly oldest-first, and no new executable route.

`service/src/orchestrator/tests.rs`: automatic resume draining an `RhnToGlc` `recipient_rate_limited` entry and a `source_wallet_rate_limited` entry once their windows clear, and a mixed `SolToGlc`+`RhnToGlc` backlog draining in one global oldest-first order.

`service/src/ledger/tests.rs`: the SQL/Rust pinning tests for `Direction::DESTINATION_IS_GOLDCOIN_SQL_IN` and for the shared exclude-list's exact membership (including that the refund lifecycle is NOT excluded).

### Regression coverage — Solana (unchanged)

`service/src/ledger/tests.rs`: same wallet inside 24h to a DIFFERENT recipient (parked — the exact bypass this closes), a different wallet to the SAME already-paid recipient (still blocked, by the pre-existing recipient rule, proving the new limit never replaced it), a different wallet AND different recipient (unaffected), same wallet after the window ages out (accepted), manual resume refusing/self-excluding for the wallet leg, both limits refusing independently in the same ledger, a direct replay of the same obligation index never being reinterpreted as a rate-limit hit, a cancelled/failed obligation never counting against its wallet, GlcToSol unaffected, plus the read-only `sol_to_glc_source_wallet_rate_limited_until` view agreeing with `fold_sol_deposit` at every boundary. `service/src/orchestrator/tests.rs`: automatic resume draining a `source_wallet_rate_limited` entry once its window clears. `service/src/api/tests.rs`: the eligibility endpoint checking the wallet leg, reporting the correct `blocked_reason` when only one limit (or both) apply, and routing `?wallet=` end to end over real HTTP. UI: `tests/unit/bridge-card-source-wallet-rate-limit.test.tsx` (glc-solana-reserve-bridge-ui) covers the same-wallet/different-recipient block, the different-wallet/same-recipient block via the unchanged recipient rule, the pre-submit race re-check, and the auto-unblock poll — mirroring `bridge-card-recipient-rate-limit.test.tsx`, whose own tests are unchanged and still pass.

## Auto-pause triggers (directional, unless noted global)

| Trigger | Scope | Rationale |
|---|---|---|
| `balance < critical_reserve` | Directional | Sizing/liquidity protection |
| Reconciliation `BREACH` classification (unexplained delta beyond itemized in-flight tolerance) | Directional (or global if the discrepancy implicates shared infrastructure) | Unexpected mismatch must fail safe, never continue silently — see [05](05-reserve-accounting.md), [10-threat-model.md](10-threat-model.md) |
| Rolling volume limit exceeded | Directional | Anomaly/attack containment — enforced by `crate::quota::enforce_rolling_volume_quota`, run every orchestrator tick for both directions (see "Rolling-24h-volume quota exhaustion — full operator workflow" below for the exact mapping, commands, and states) |
| Repeated `DestinationSubmissionFailed` beyond retry budget, same direction | Directional | Likely systemic (RPC outage, fee-market issue) rather than one-off |
| Attestation/vault signer quorum unreachable for a configured duration | Directional | Liveness failure in the authorization layer shouldn't silently degrade to fewer required signers |
| Any `ManualReview` classified as a security incident (see [10-threat-model.md](10-threat-model.md)) | Global | Default to maximum caution until scoped |
| Operator-invoked emergency stop | Global | Always available (`glc-admin onchain-pause --scope global --note ...` and/or `glc-admin pause --direction <goldcoin\|solana>` for the local admission gate), highest priority gate |

**Un-pausing** is always operator-controlled and requires a note, regardless of what triggered the pause — no automatic un-pause path exists for any trigger, to avoid a flapping balance or a transient reconciliation blip silently resuming settlement. This is a deliberate asymmetry (fast/automatic to pause, slow/manual to resume), consistent with the old bridge's asymmetric pause-authority pattern (ADR-0014 §7) applied at the operational layer instead of the governance layer.

## Rolling-24h-volume quota exhaustion — full operator workflow (added 2026-08-22)

The rolling-24h-volume cap (100,000 GLC, GLOBAL and PER DIRECTION — see the reserve-sizing update above and P0-6) is enforced twice, at two different layers, and an operator dealing with an exhausted direction needs to know which is which:

- **On-chain, always, for real**: `programs/glc-reserve-bridge/src/limits.rs::enforce_and_record_rolling_volume`, checked inside `release_from_reserve`/`deposit_to_reserve` on every actual attempt. This is the protocol-level enforcement nothing can bypass — not a pause, a per-transaction quota check against a fixed-bucket window that resets entirely, on its own, once `rolling_window_seconds` (86,400s = 24h) has elapsed since the bucket started. **This reset is real and automatic — but it is a quota reset, never an un-pause**, and never claims to be a "midnight reset" (it resets 24h after the bucket started, not at a fixed wall-clock time).
- **Off-chain, as a consequence, this service's own admission gate**: `crate::quota::enforce_rolling_volume_quota`, run every orchestrator tick for both directions. When it observes a direction's on-chain window exhausted (`remaining < min_transfer_amount`), it engages this service's own LOCAL pause for that direction (`Ledger::set_paused`) — the exact same local gate `reconciliation::reconcile` already uses for a balance breach. **This local pause never lifts itself, even after the on-chain window resets** — only an explicit operator `unpause` clears it, exactly like every other auto-pause trigger in the table above.

### 1. Exact direction <-> pause-scope mapping

| Settlement direction | On-chain `PauseScope` (real, protocol-level) | Local ledger `ReserveDirection` (this service's own admission gate) |
|---|---|---|
| Goldcoin L1 -> Solana (`GlcToSol`) | `Release` (`instructions::admin::PauseScope::Release`) — direction byte `0` | `SolanaReserve` (`GlcToSol`'s destination reserve) |
| Solana -> Goldcoin L1 (`SolToGlc`) | `Deposit` (`instructions::admin::PauseScope::Deposit`) — direction byte `1` | `GoldcoinReserve` (`SolToGlc`'s destination reserve) |

(`PauseScope::Global` pauses both directions at once; there is no `PauseScope` covering both individually in one call.)

### 2. Exact operator commands

```
# Pause only Goldcoin -> Solana (on-chain, protocol-level — blocks release_from_reserve for everyone)
glc-admin onchain-pause   --rpc-url URL --keypair ADMIN_KEY --scope release --note "TEXT"

# Unpause only Goldcoin -> Solana
glc-admin onchain-unpause --rpc-url URL --keypair ADMIN_KEY --scope release --note "TEXT"

# Pause only Solana -> Goldcoin (on-chain, protocol-level — blocks deposit_to_reserve for everyone)
glc-admin onchain-pause   --rpc-url URL --keypair ADMIN_KEY --scope deposit --note "TEXT"

# Unpause only Solana -> Goldcoin
glc-admin onchain-unpause --rpc-url URL --keypair ADMIN_KEY --scope deposit --note "TEXT"
```

These are the real, enforceable, protocol-level circuit breakers — the ones to use if a direction genuinely must stop accepting new settlement, for anyone, immediately. This service's own local admission gate (`glc-admin pause/unpause --db PATH --direction <goldcoin|solana> --note TEXT`, see the LOCAL LEDGER PAUSE section above) only gates what THIS service's own API/orchestrator will do — it does not, and cannot, stop a third party from calling the on-chain program directly. `crate::quota`'s auto-pause (previous section) always engages the LOCAL gate, never the on-chain one — an operator who wants the real, protocol-level circuit breaker engaged too must run `onchain-pause` explicitly.

### 3. Confirmed behavior

- **Quota exhaustion blocks only the affected direction.** Each direction's rolling volume is tracked in its own `RollingVolumeWindow` PDA and checked independently, on-chain and off-chain — confirmed by `quota::tests::auto_pauses_the_affected_direction_only_when_quota_is_exhausted` and `api::tests::status_reports_quota_exhausted_independently_per_direction`.
- **The opposite direction can remain operational.** Same tests as above; there is no shared state between directions that a check on one could accidentally affect on the other.
- **Rolling capacity becoming available does NOT automatically unpause.** The on-chain window's own bucket reset is real and automatic, but `crate::quota` never calls `set_paused(direction, false, ...)` — confirmed by `quota::tests::never_auto_unpauses_across_repeated_ticks_of_continued_exhaustion`. Only an explicit `glc-admin unpause`/`onchain-unpause` clears either pause layer.
- **An operator must explicitly unpause after refill/reconciliation.** Exactly the mechanism above — there is no code path anywhere in this service that calls `set_paused(direction, false, ...)` other than the explicit `glc-admin pause`/`onchain-unpause` commands themselves.
- **Unpausing while the rolling quota is still exhausted does not bypass quota enforcement.** The on-chain pause flag and the on-chain rolling-volume window are two completely independent `require!` checks inside `release_from_reserve`/`deposit_to_reserve` — flipping one never touches the other. Confirmed directly by `pausing_and_unpausing_the_release_leg_does_not_reset_or_bypass_the_rolling_volume_quota` (`programs/glc-reserve-bridge/tests/release_from_reserve.rs`): pause, then unpause, while the window is still genuinely exhausted, and the exact same claim still fails with `ExceedsRollingVolumeLimit`, never succeeds and never fails with a stale pause error.

### 4. API/UI states

`GET /status` and `GET /stats` (`service/src/api.rs`) expose, per direction:

- **active** — `glc_to_sol_available`/`sol_to_glc_available` = `true` (neither paused, quota not exhausted, reserve capacity above zero; and for `sol_to_glc_available`, since 2026-09-12, a deposit of the program's full `per_transfer_limit` would clear the confirmed-liquidity safety buffer — see "Public availability is evaluated at a normal transfer size"). When `false`, `sol_to_glc_availability_reason` names the gate.
- **quota exhausted** — `glc_to_sol_quota_exhausted`/`sol_to_glc_quota_exhausted` = `true`, with the exact remaining headroom in `glc_to_sol_rolling_volume_remaining`/`sol_to_glc_rolling_volume_remaining` (raw atomic units, `0` when fully exhausted).
- **operator paused** — `goldcoin_paused`/`solana_paused` = `true` (this service's own local gate), OR the on-chain `paused`/`release_paused`/`deposit_paused` circuit breaker for that direction's Solana leg is set. The on-chain flags are not exposed as raw fields on this public API (`glc-admin show-config` and the admin API's `/onchain` show them), but since 2026-09-12 they ARE folded into `glc_to_sol_available`/`sol_to_glc_available` and into every `available` on `GET /chains`: `deposit_paused` closes `SolToGlc` and `SolToRhn` (both enter through `deposit_to_reserve`), `release_paused` closes `GlcToSol` and `RhnToSol` (both settle through `release_from_reserve`), `paused` closes all four. Before that date the public API consulted only the local layer, and the 2026-09-12 incident (a `deposit_paused` set on 2026-09-09 and never cleared by launch, which only unpauses the LOCAL Solana row) had both endpoints advertising `SolToGlc` while every deposit reverted with `DepositDirectionPaused`. If `bridge_config` cannot be read, `GET /chains` fails closed for the four Solana-leg routes and keeps serving the two Goldcoin<->Robinhood ones.
- **quota exhausted + operator paused/waiting for refill** — both of the above `true` simultaneously; this is exactly the state `crate::quota`'s auto-pause produces once its tick observes an exhausted window, and it persists (the pause bit) even after the quota itself later clears on its own.
- **reserve/protected-minimum constraint** — a *separate*, pre-existing signal: `available_capacity <= 0` (`GET /reserve`, and `ReserveStats.available_capacity` in `GET /stats`) even with nothing paused and quota not exhausted — this is the `enforce_protected_minimum`-equivalent off-chain check, orthogonal to both pause and quota.

Any one of paused / quota-exhausted / capacity-insufficient alone is enough to make `*_available` report `false` for that direction — a UI wanting the *specific* cause reads these fields directly rather than inferring it from `POST /transfers`' error message (see next section).

### 5. User-facing message

The exact, approved copy for a direction that cannot currently accept a new transfer — for ANY of the causes above — is `service::api::DIRECTION_UNAVAILABLE_MESSAGE`:

> Bridge capacity reached for this direction.
> Transfers are temporarily paused while reserves are replenished.
> Please check the official Telegram for reopening updates.

This is deliberately the ONLY text `POST /transfers` returns for `ApiError::Paused`/`ApiError::QuotaExhausted`/`ApiError::InsufficientLiquidity` — never a technical reason code, never the raw remaining/available numbers, and **never a claim about automatic reopening**: no midnight reset, no automatic unpause, is stated or implied anywhere in this copy. Pinned directly by `api::tests::create_transfer_reports_quota_exhausted_with_the_exact_message_never_creates_a_row`, which additionally asserts the string contains neither "midnight" nor "automatic".

### 6. Test coverage

- `programs/glc-reserve-bridge/tests/release_from_reserve.rs::pausing_and_unpausing_the_release_leg_does_not_reset_or_bypass_the_rolling_volume_quota` — on-chain, item 3's core invariant.
- `service/src/quota.rs`'s own `tests` module — `does_not_pause_while_headroom_remains`, `auto_pauses_the_affected_direction_only_when_quota_is_exhausted`, `a_fresh_bucket_reset_reports_no_exhaustion_even_with_a_high_prior_total`, `never_auto_unpauses_across_repeated_ticks_of_continued_exhaustion`.
- `service/src/solana/accounts.rs`'s `rolling_volume_remaining_*` tests — the pure remaining-capacity projection, including the exact fixed-bucket boundary condition and saturating-subtraction safety.
- `service/src/api/tests.rs` — `status_reports_quota_exhausted_independently_per_direction`, `status_does_not_report_quota_exhausted_while_headroom_remains`, `create_transfer_reports_quota_exhausted_with_the_exact_message_never_creates_a_row`, `create_transfer_succeeds_when_amount_fits_within_remaining_quota`.

### 7. On-chain program (.so) impact

**None.** Every piece of this workflow is either (a) the on-chain quota/pause enforcement that already existed before this update (`limits.rs`, `instructions::admin::set_paused`, both unmodified — `programs/glc-reserve-bridge/src/` has zero diff for this change), or (b) new off-chain code reading that existing on-chain state (`service/src/solana/accounts.rs`'s new `RollingVolumeWindow` decoder and `rolling_volume_remaining` projection, `service/src/quota.rs`'s new local auto-pause consequence, and new `service/src/api.rs` fields/messages). The one on-chain change in this update is a NEW TEST (`release_from_reserve.rs`), not new program source — the deployed `.so` is unaffected.

### 8. Manually discarding the remaining wait (`reset-rolling-window`, added 2026-08-29)

Item 1 above describes the window's own automatic reset — real, but only once the FULL `rolling_window_seconds` (24h) has elapsed since the bucket started. An operator who has already refilled/rebalanced the reserve and verified accounting has no reason to wait out the remainder of that 24h — `glc-admin reset-rolling-window` is the administrative override for exactly this: it manually reopens ONE direction's on-chain rolling-volume window immediately, without editing SQLite and without fabricating a timestamp by hand.

**This is a deliberate override of the anti-drain protection the rolling-volume window exists to provide.** Use it only after independently verifying reserve/accounting state (steps A-C below) — never as a routine substitute for letting the window age out on its own, and never before confirming the exhaustion was actually caused by legitimate volume rather than something that still needs investigating.

#### What it does, and does not, touch

Resets exactly one `RollingVolumeWindow` PDA — `window_start` becomes the current on-chain clock's `unix_timestamp` (a fresh window starting now, from the real trusted clock, never operator-supplied), `window_total` becomes `0`. Nothing else changes: reserve balances, obligations, `protected_minimum`, `per_transfer_limit`, `rolling_volume_limit`, `min_transfer_amount`, and the OTHER direction's window are all left exactly as they were (`programs/glc-reserve-bridge/src/instructions/admin.rs::reset_rolling_volume_window`'s account list contains nothing else to touch). Emits `RollingVolumeWindowReset` (admin pubkey, direction, previous and new `window_start`/`window_total`, unix timestamp, slot) for the audit trail.

#### Authorization and preconditions

Same `BridgeConfig.admin`-gated authorization as `onchain-pause`/`set-limit` (`instructions::admin::AdminConfig`'s pattern) — any other signer is rejected with `UnauthorizedAdmin`. Additionally requires `BridgeConfig.paused == true` (global pause already engaged) — refused with `BridgeNotPaused` otherwise; this is a conscious precondition the operator must satisfy first, same discipline as `rebalance_withdraw`. Does **not** require the individual direction's own `release_paused`/`deposit_paused` flag to also be set.

#### Exact operator commands

```
# Reset the GLC L1 -> Solana (release) rolling-volume window
glc-admin reset-rolling-window --rpc-url URL --keypair ADMIN_KEY --direction glc-to-sol --note "TEXT"

# Reset the Solana -> GLC L1 (deposit) rolling-volume window
glc-admin reset-rolling-window --rpc-url URL --keypair ADMIN_KEY --direction sol-to-glc --note "TEXT"
```

`glc-to-sol` maps to the RELEASE window (`Direction::GoldcoinToSolana`, on-chain direction byte `0`); `sol-to-glc` maps to the DEPOSIT window (`Direction::SolanaToGoldcoin`, byte `1`) — the same mapping table in section 1 above. `--note` is required (mandatory audit trail, same as every other on-chain `glc-admin` command) and is recorded in this command's own printed output and the transaction history itself; it is not written to a separate local audit-log file, since none exists for this class of command today.

#### Full maintenance sequence

A quota-driven maintenance window that includes a deliberate window reset should follow this order, not an ad hoc one:

```
A. glc-admin onchain-pause --scope global --keypair ADMIN_KEY --rpc-url URL --note "maintenance: starting"
B. Refill/rebalance reserves as necessary (glc-rebalance-withdraw / operator-side funding, per the reserve-sizing runbook)
C. Verify reserve invariants and Goldcoin UTXO maturity (glc-admin status, ops::reserve_health, the pre-admission reconciliation report)
D. If, and only if, the operator intentionally wants to discard the remaining rolling-window wait:
     glc-admin reset-rolling-window --direction <glc-to-sol|sol-to-glc> --keypair ADMIN_KEY --rpc-url URL --note "TEXT"
     (repeat for the other direction if both need it — each call touches exactly one window)
E. glc-admin show-config   # verify rolling remaining/quota state before reopening anything
F. glc-admin unpause --db PATH --direction <goldcoin|solana> --note "TEXT"   # this service's own local gate, per direction, as needed
G. glc-admin onchain-unpause --scope global --keypair ADMIN_KEY --rpc-url URL --note "maintenance: complete"   # LAST
H. Verify GET /status and glc-admin show-config both reflect the fully-reopened, reconciled state
```

Global on-chain unpause is deliberately step G, not earlier — every step before it runs with the strongest available circuit breaker still engaged, and reopening settlement is the one irreversible-in-effect action in this sequence (a transfer can be admitted the instant it clears), so it comes only after every verification step, never before.

#### API/status after reset

`GET /status` naturally reports the reset correctly with **no service-side change** — `glc_to_sol_rolling_volume_remaining`/`sol_to_glc_rolling_volume_remaining` and `*_quota_exhausted` are derived live from the same on-chain `RollingVolumeWindow` account `service/src/solana/accounts.rs::rolling_volume_remaining` always reads, so once the reset transaction lands, the very next `/status` poll shows `remaining` equal to the full configured `rolling_volume_limit` and `quota_exhausted = false` for that direction — never a value this API fabricates or caches independently of on-chain state.

#### Test coverage

`programs/glc-reserve-bridge/tests/reset_rolling_volume_window.rs` — valid admin reset of each direction, non-admin rejection, rejection while global pause is `false`, no requirement that the individual direction also be paused, resetting one direction leaving the other completely untouched, `rolling_volume_limit`/reserve accounting unchanged, remaining capacity returning to the full configured limit with `quota_exhausted` becoming false, subsequent volume counted normally from the fresh state (not appended to the discarded total), and a repeated reset while still paused behaving deterministically. `service/src/solana/instructions.rs` — the off-chain instruction encoder's discriminator/account-ordering/direction-to-PDA mapping.

## Key compromise response (draft, depends on ratified trust model)

Structure reused from old bridge's rehearsed compromise runbook, repointed at internal custody domains rather than federation members:

1. **Detect**: anomalous signing activity, reconciliation breach, or external report.
2. **Contain**: global emergency pause immediately — `glc-admin onchain-pause --scope global --keypair ADMIN_KEY --rpc-url URL --note "compromise response: containing"`.
3. **Assess**: determine which custody domain(s) are implicated; do not assume "one domain compromised" without checking whether threshold-worth of domains are affected.
4. **Rotate**: stage the transition off-chain first — `glc-admin custody-propose --db PATH --kind attestation-rotation --old-identities CSV --new-identities CSV --by IDENTITY --required-approvals N --note "compromise response"` (or `--kind vault-sweep --new-threshold N` for the Goldcoin leg), then `glc-admin custody-verify-identity` once the clean replacement key/vault in a fresh custody domain is independently checked, then collect `custody-approve`s. Once `Approved`: for the Solana leg, execute attestation-key rotation via the timelocked governance instruction (already built on-chain — `propose/execute/cancel_attestation_key_rotation`, Phase 2); for the Goldcoin leg, execute the sweep-to-fresh-vault transfer through real custody tooling (the old bridge's `sweep.rs` and its independent-commitment-re-derivation discipline, docs/01-reuse-inventory.md, is the intended shape for that transfer itself — not yet ported). Either way, `glc-admin custody-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT` requires the relevant reserve(s) already paused (enforced, not just documented) and only ever records evidence — this service never generates the new keys/vault or performs the rotation/sweep itself.
5. **Verify**: independent confirmation (a domain not implicated in the compromise) that the new keys/vault are correctly configured before un-pausing.
6. **Resume**: operator-controlled, with note, one direction at a time — `glc-admin onchain-unpause --scope <release|deposit> --keypair ADMIN_KEY --rpc-url URL --note "compromise response: resuming <direction>"`.
7. **Post-mortem**: written, includes whether the reconciliation/monitoring layer detected the compromise before or after external report — a gap here is itself a finding.

## Accrued bridge fees (no withdrawal procedure yet)

The 3% bridge fee (docs/20-bridge-fee.md) accrues on the SOURCE reserve's
row (`reserve_ledger.accrued_fees_atomic`, canonical units, visible via
`glc-admin status` and the `/metrics` endpoint) and stays there — this
phase has **no treasury wallet/address and no fee-withdrawal path**, by
design. Accrued fees are never automatically moved anywhere and are never
counted toward `available_capacity`/the reserve invariant; they are purely
an audit-visible running total. Standing up a withdrawal procedure (who
authorizes it, where funds go, how it's distinguished from a rebalance in
the ledger) is future work, not yet scoped here.

## Admin API & admin UI (added 2026-08-29)

Full reference: [27-admin-control-plane.md](27-admin-control-plane.md).
Operational summary:

- The daemon serves an authenticated admin API when
  `service.admin_bind_addr` is configured (bind privately;
  `config.pilot-template.toml` shows the shape). Operators are listed as
  `{ name, token_env }` — the bearer token lives only in the named env
  var, never in the config file. One token per person; the token's
  operator name is the `actor` on every audit row.
- **UI-executable** (through the admin UI / API, mandatory note,
  audited): local pause/unpause per direction, admission close, admission
  open (same invariant + UTXO-liquidity gates as `glc-admin
  open-admission` — one shared implementation), resume-manual-review
  (same unconditional safety and rate-limit checks as `glc-admin
  resume-manual-review`), and the full rebalance request workflow.
  **Local pause stops new admissions/starts, NOT in-flight
  settlements** — requests already past `SourceFinalized` still settle
  on subsequent ticks (pre-existing semantics, unchanged). The full
  money-movement stop is the ON-CHAIN global pause below; see
  docs/27-admin-control-plane.md "What local pause does and does not
  stop".
- **CLI approval required** (the admin keypair never leaves the
  operator's machine): `glc-admin onchain-pause`, `glc-admin
  onchain-unpause`, `glc-admin set-limit`, `glc-admin
  reset-rolling-window` — the UI shows current on-chain state read-only
  and generates the exact command (atomic units converted server-side)
  for the operator to review and run over SSH, exactly as documented in
  the "Executable commands" section above.
- Never UI-reachable at all: `glc-admin retry-goldcoin-payout`,
  `glc-admin split-vault-utxo` (they sign and broadcast), every
  custody-transition workflow (the CLI's custody subcommands), and the
  bridge fee (a compile-time constant — docs/20-bridge-fee.md's staged
  fee-change process).
- Audit trail: the `admin_audit_log` table (docs/06-schema.md, schema
  v15) records every mutation attempt, refusals included; query it from
  the UI's Audit Log page or `GET /audit-log`.

## Explicitly deferred to real operational experience

Exact reserve thresholds, rebalance cadence, rolling-volume window size, per-transfer limits: all configuration, none defaulted in this document, per the old bridge's precedent of refusing to assert production security parameters without operational data (`docs/custody.md`'s open items #7/#8 were left open for the same reason — better an explicit open decision than a silently wrong default). See [12-management-decisions.md](12-management-decisions.md).

**Update 2026-08-21: confirmation depths are no longer on this deferred list for the pilot specifically** — see "Confirmation-depth values (pilot, approved 2026-08-21)" above for the actual interim numbers now in effect. The *final*, historical-data-backed values remain deferred, per docs/12 item 4, and are a scale gate rather than a pilot concern.

## Goldcoin-sourced ManualReview refunds (Goldcoin side) (added 2026-09-03)

### What this is for

A Goldcoin deposit whose observed amount does not equal the amount the
request reserved is parked in `ManualReview` with the note
`deposit_amount_mismatch: expected N observed M`. The user's Goldcoin is
real and sitting in the vault, but the request can never settle:
settling it would deliver the wrong amount. Before this command the only
exits were to leave the deposit parked indefinitely or to hand-build a
transaction against the vault.

This is the opposite decision to `manual-review-settle`, and it is the
Goldcoin twin of `refund-manual-review` (which returns a *Solana* deposit
for a parked `SolToGlc` request). Both are one-way and mutually exclusive.

### Which requests this covers (updated 2026-09-06)

Both Goldcoin-SOURCED routes: `GlcToSol` and `GlcToRhn`. The Goldcoin
half is identical for the two — same derived deposit address, same
principal, same prevout-derived destination, same one-refund-per-request
rule. What differs is the proof that no settlement has already begun,
because the two routes settle on different chains and leave different
traces:

| Route | "no settlement has begun" is proved by |
|---|---|
| `GlcToSol` | `destination_txid` and `settlement_claim_hash` are NULL, **and** the on-chain Solana `DepositClaim` PDA does not exist |
| `GlcToRhn` | no `robinhood_transactions` row of any kind names the request, and no Robinhood deposit observation folded into it |

Each route additionally requires the OTHER's evidence to be absent. That
is not belt-and-braces for its own sake: a Solana settlement column set on
a `GlcToRhn` row, or a Robinhood payout row naming a `GlcToSol` request,
means the ledger disagrees with itself, and a refund is not the moment to
discover that.

`SolToGlc` and `RhnToGlc` are NOT refunded here — their principal is on
their own source chain. Use `refund-manual-review` for `SolToGlc`.

### Reading the ROBINHOOD PAYOUT WITNESS block

The dry run prints this block for every request, and an empty one is the
affirmative statement that nothing was found:

```
  ROBINHOOD PAYOUT WITNESS (durable ledger state)
    no payout operation and no folded deposit observation names this request
    — no Robinhood payout has begun
```

When it refuses, it names every blocker with a stable code:

```
  ROBINHOOD PAYOUT WITNESS (durable ledger state)
    REFUSING — 1 blocker(s). A Goldcoin refund would return the deposit a
    Robinhood payout is drawn against.
      [payout_broadcast] a Robinhood payout operation (robinhood_transactions
      id 12) exists for this request in state Broadcast: 2 authorization
      signature(s) persisted; a submitter nonce is allocated; signed
      transaction bytes are persisted; a transaction hash is persisted;
      1 broadcast attempt(s)
    Inspect the operation with: glc-admin robinhood-tx-show --config PATH
    --request-id 12
```

| Code | Meaning | Refund |
|---|---|---|
| `payout_authorizing` | the operation row exists; the payload is fixed, no signature yet | refused |
| `payout_authorized` | a 2-of-3 quorum is stored | refused |
| `payout_signed` | a nonce is allocated and the bytes are persisted | refused |
| `payout_broadcast` | handed to a node at least once; **its fate may be unknown** | refused |
| `payout_included` | receipt read back, `status = 1` | refused |
| `payout_finalized` | included and past the confirmation depth | refused |
| `payout_reverted` | receipt read back, `status = 0` | refused |
| `payout_manual_review` | stopped for a human | refused |
| `unexpected_robinhood_operation` | a `Settlement`/`Refund` row names this Goldcoin-sourced request — corrupted linkage | refused |
| `unexpected_deposit_fold` | an inbound Robinhood observation folded into it — corrupted linkage | refused |

**`payout_reverted` is not an all-clear.** The transaction consumed its
nonce and its gas; whether the contract moved value is a question to
settle with `glc-admin robinhood-tx-show` and the chain in front of you,
not by refunding on the assumption that a revert means nothing happened.
`payout_broadcast` is the same: that state says nothing about whether a
node received the bytes, which is exactly why it must not be read as
"did not happen".

There is no override. A refusal here is resolved by investigating the
payout, not by re-running the command.

**The witness is durable ledger state, so a daemon restart changes
nothing.** An in-flight payout looks exactly as disqualifying after a
crash as before one. It is also re-checked inside the writing transaction
at `--execute` time, so a payout that starts between your dry run and your
execute is caught rather than raced.

**No tick ever refunds on its own.** A parked request stays parked
whether the route is open or closed; refund initiation is this command
and nothing else.

### The request-binding proof (corrected 2026-09-04)

A `GlcToSol` deposit does **not** pay the root vault script. Each request
gets its own 2-of-3 P2SH deposit address, derived deterministically from
the request id:

```
tweak = SHA256("glc-bridge-deposit" || request_id.to_le_bytes()) mod n
P_j'  = P_j + tweak·G          for each of the three root pubkeys
```

`goldcoin::derivation::derive_request_vault` is the ONE canonical
implementation, shared by request creation (`api.rs`), payout recovery
(`payout_recovery.rs`) and refund verification. There is no refund-only
copy to drift.

The refund verifier **re-derives** that script from the request id plus the
configured root pubkeys, threshold and network — all immutable and public,
no database value involved — and requires the RPC output's scriptPubKey to
equal it byte for byte. `bridge_requests.deposit_address` is **never** an
authority; `deposit_script_pubkey_hex`, where present, must agree with the
derivation, so a tampered column is a refusal rather than a redirection.

An earlier version of this path required the deposit to pay the ROOT vault
script and to appear in `vault_utxos`. Both were wrong for every
per-request deposit: `vault_utxos` is `listunspent`-derived spendable
inventory for addresses the NODE's wallet owns, and nothing imports a
per-request derived P2SH into the node, so such a deposit can never appear
there. That requirement was unsatisfiable by construction and has been
removed — never by inserting deposits into `vault_utxos` to make
verification pass.

### AMOUNT WITNESS MODE

The dry run and the daemon both report which witness backs the principal.
The two are **not** equivalent and are never presented as if they were.

**`durable chain-vs-ledger`** — `bridge_requests.observed_amount_atomic`
(schema v20) exists. The indexer wrote it at park time, in the same
transaction as the source outpoint, from its own decoded output. A fresh
RPC read must equal it exactly, with no tolerance in either direction: two
independent observations of one fact, taken at different times by
different code.

**`legacy RPC-only — request predates observed_amount_atomic witness`** —
the request was parked before v20, so no historic second observation
exists. The principal rests on the independently verified RPC read alone.

Legacy rows are **not** backfilled. The only historic record of the amount
is the free text of `manual_review_note`, and parsing a number back out of
an operator-readable message is exactly what the durable witness exists to
prevent; reconstructing it from chain history is out of scope for a
migration. In legacy mode every OTHER binding still applies in full —
outpoint, derived deposit script, stored-column agreement, confirmations,
single input, prevout trace, no release, no prior refund, pause,
2-of-3 signing. Only the amount has one witness instead of two, and the
dry run says so in as many words.

### Where the money's two decisions come from

Two facts decide where value goes — how much, and to whom — and **neither
is taken from the database on faith**:

- **How much.** The principal is the deposit output's value read from
  Goldcoin RPC now, required to equal the durable
  `bridge_requests.observed_amount_atomic` witness the indexer wrote at
  park time (schema v20) — two independent observations of one fact; if
  they disagree the refund refuses rather than preferring one. A request
  parked before that column existed falls to the explicitly reported
  legacy mode above. It is never `bridge_requests.amount_atomic` (the amount the request
  *expected* — for a mismatch that number is wrong by definition), and it
  is **never parsed out of `manual_review_note`**. Only the note's reason
  prefix (before the first `:`) is read, purely to decide eligibility; the
  observed figure in the note's free text is ignored entirely, so a
  malformed or tampered note cannot influence the amount by a single unit.

- **To whom.** The destination is traced from what the depositor actually
  spent: fetch the deposit transaction, require **exactly one input**,
  fetch that input's previous output, and recover the P2PKH hash160 from
  its scriptPubKey. There is no `--destination` flag and no database column
  that can redirect it.

### Everything ambiguous is a refusal

The command fails closed on: more than one input (with two or more, the
transaction may combine outputs from different owners and there is no
principled way to pick a sender); a coinbase or unreported prevout; any
script that is not canonical P2PKH (P2SH, multisig, segwit, OP_RETURN); a
sender output that pays the vault itself; an unreachable Goldcoin or
Solana RPC; a chain/index disagreement; insufficient confirmations; a
mempool-only deposit; a deposit output that does not pay the vault; an
existing refund; or any sign a Solana release has begun.

### The independent Solana no-release check

A `GlcToSol` request settles via `release_from_reserve`, which creates a
`DepositClaim` PDA seeded by the deposit's own `txid‖vout`. That PDA
existing is the on-chain, **database-independent** witness that a release
already happened. This command reads it directly, in addition to the
database checks, and refuses if it exists — or if the read cannot be
completed at all, because a chain you could not read proves nothing. It is
re-checked immediately before signing, not only at dry-run time.

### Confirmation requirement

The deposit must have at least `goldcoin.confirmation_depth`
confirmations — the same finality depth the indexer uses to finalize a
deposit. Deliberately not `vault_min_confirmations`, which governs whether
an input is *spendable*; the question here is whether the deposit being
returned is *final*.

### Fee policy: the vault absorbs it

The user receives the **full observed deposit**. The Goldcoin miner fee is
additional vault expenditure, recorded separately as `fee_atomic`. A
deposit the bridge could not settle is not the user's fault, so the user is
made whole; the fee is the bridge's cost of returning it. The schema
enforces `refund_amount_atomic = observed_amount_atomic` as a CHECK, so
this holds even if the application logic regressed.

### Procedure

1. **Dry run first** (read-only; needs no pause, contacts no signer,
   writes nothing):

   ```
   glc-admin refund-glc-manual-review --config /etc/glc-bridge/config.toml \
     --request-id 2477 --note "incident-2477: deposit 29050 vs expected 29100"
   ```

   Read every check. The verdict line is only `would refund` when all of
   them pass.

2. **Pause the GoldcoinReserve.** Execution refuses without it, and this
   command never pauses or unpauses on its own — pausing is an operator
   decision with its own consequences.

   ```
   glc-admin pause --db PATH --direction goldcoin --note "refunding #2477"
   ```

3. **Re-run the dry run** against the now-paused state and confirm the
   verdict reports BOTH that verification passed and that the pause
   prerequisite is satisfied.

4. **Execute.** This does NOT sign locally: `glc-admin` sends the request
   id and your note to the daemon's authenticated admin endpoint, and the
   daemon — which owns the vault signers — re-runs every check against
   fresh state and signs. Set your operator identity first; the token is
   read from the env var your config names, never passed as an argument:

   ```
   export GLC_ADMIN_OPERATOR=alice
   export GLC_ADMIN_TOKEN_ALICE=...        # whatever token_env names

   glc-admin refund-glc-manual-review --config /etc/glc-bridge/config.toml \
     --request-id 2477 --note "incident-2477: deposit 29050 vs expected 29100" --execute
   ```

   You must be on the refund-execution allow-list
   (`may_execute_glc_refunds = true`). An ordinary admin token is
   deliberately not enough.

5. **Unpause explicitly** once the refund is broadcast:

   ```
   glc-admin unpause --db PATH --direction goldcoin --note "refund #2477 broadcast"
   ```

6. Track it: `glc-admin glc-refund-list --db PATH --open-only`. It clears
   itself — the daemon marks the request `Refunded` once the transaction
   reaches `required_goldcoin_confirmations` (see "Confirmation
   reconciliation, without an operator" below). **Do not re-run
   `refund-glc-manual-review` for a request that already shows a txid**:
   that transaction is authoritative, and the money it moved is already
   gone from the vault.

### Execution runs in the daemon, not in the CLI

`glc-admin` never holds a vault key or a signer token. The running
`glc-bridge-daemon` already owns the `vault_remote_signers` and their
env-resolved tokens, and that trust boundary is unchanged: `--execute` is
a *request* to the daemon.

**Transport**: the existing authenticated admin control plane
(docs/27-admin-control-plane.md) — `service.admin_bind_addr`, bound
privately, per-operator bearer tokens, no cookies, no CORS, every mutation
audited. One new route:

```
POST /refunds/glc/{request_id}/execute
Authorization: Bearer <operator token>
{"note": "<mandatory audit note>"}
```

That is the entire input surface. There is no destination, amount, fee,
transaction, signer or override field, so nothing a caller sends can
influence where the money goes.

**This is the one fund-moving exception** to that API's previous read-only
posture, and it is fenced four ways, each failing closed:

1. The route exists only when the daemon injected a refund executor. Every
   other `AdminApi` construction cannot move funds at all.
2. The operator's token must carry `may_execute_glc_refunds`. **An
   ordinary admin token is not sufficient** — a leaked read-only token
   cannot spend from the vault.
3. If *no* operator holds the capability, the route refuses outright, so a
   deployment that never opted in is not exposed.
4. The local `GoldcoinReserve` pause is required, and never engaged or
   cleared by this command.

**Config** — grant it per person:

```toml
[[service.admin_operators]]
name = "alice"
token_env = "GLC_ADMIN_TOKEN_ALICE"
may_execute_glc_refunds = true      # defaults to false

[[service.admin_operators]]
name = "bob"
token_env = "GLC_ADMIN_TOKEN_BOB"   # bob cannot execute refunds
```

The daemon re-runs the FULL verification server-side immediately before
signing — nothing `glc-admin` checked earlier is trusted. A dry run that
passed minutes ago proves nothing about now: the request may have moved
on, a Solana release may have appeared, the chain may disagree, or the
pause may have been lifted.

**Signing** uses the daemon's existing 2-of-3 vault signers. Every partial
is verified locally before assembly, a refusal or timeout aborts the whole
refund, and the threshold is never reduced. The response carries the
lifecycle state, outpoint, amounts, destination, fee, txid and every
server-side check — and never a token, signer identity or signed
transaction.

**Two different confirmation depths, both configured, easily confused:**

- `goldcoin.vault_min_confirmations` — how deep the DEPOSIT being returned
  must be before a refund may be built at all. This is the
  `required_confirmations` the dry run and the executor check.
- `required_goldcoin_confirmations` (the payout confirmation depth) — how
  deep the REFUND's own transaction must be before the request is marked
  `Refunded`. It is the same depth an ordinary Goldcoin payout must reach
  to be considered settled, for the same reason: it is the same vault
  spending to the same chain.

**Nothing automatic ever builds, signs or sends a refund.** That remains
an explicit operator request, and `goldcoin::refund::tests::
no_orchestrator_tick_builds_signs_or_broadcasts_a_goldcoin_refund` asserts
it structurally — the orchestrator's tick surface contains no reference to
`execute_refund`, `begin_goldcoin_refund`,
`record_goldcoin_refund_signed` or `record_goldcoin_refund_broadcast`.
A `Built` or `Signed` row is never swept up by a tick.

**Confirmation reconciliation IS automatic** (added 2026-09-04) — see the
section below. It only ever observes a transaction that already exists.

### Lifecycle and crash recovery

The request walks `ManualReview -> RefundPending -> RefundBroadcast ->
Refunded`; the `goldcoin_refunds` row records which artifact exists
(`Built -> Signed -> Broadcast -> Refunded`). State is always persisted
*before* the irreversible step, which is what makes each state resumable:

| State | What exists | Resume does |
|---|---|---|
| no row | nothing reserved or signed | re-verify everything, build fresh |
| `Built` | inputs reserved, unsigned tx stored; nothing signed | re-verify, sign the SAME reserved inputs |
| `Signed` | signed bytes stored, may already be in a mempool | **re-broadcast the same bytes** — never build a replacement |
| `Broadcast` | **a transaction paying real vault funds already exists**; txid recorded | only advance confirmations — the daemon does this itself |
| `Refunded` | terminal | no-op |

Because a resume re-broadcasts identical bytes, the txid is identical, so a
node that already has it answers `AlreadyInMempool`/`AlreadyInChain` and
that is treated as success. A `missing-inputs` rejection is **not**
retried blindly — it means an input was spent elsewhere, and the vault must
be reconciled first.

### Confirmation reconciliation, without an operator (added 2026-09-04)

Before this, `Broadcast` was where a refund stopped. The terminal
transition — `Ledger::record_goldcoin_refund_confirmed`, and the only
release of the request's stranded SolanaReserve reservation — **had no
production caller at all**: not the orchestrator, not the CLI, not the
admin API. A refund whose transaction confirmed on Goldcoin months ago
stayed `RefundBroadcast` indefinitely, kept holding reserved capacity the
chain had already discharged, and kept appearing in `glc-refund-list
--open-only` as though it had never been sent.

The stale row was not the danger. What it invited was: an operator reading
a long-"open" refund can reasonably conclude the refund never went out and
send another one — of money that has already left the vault, unrecallably.

`Orchestrator::tick_glc_refund_confirmations` now runs every tick,
alongside the payout-confirmation phase and before the tick's
reconciliation passes (confirming a refund releases a reservation those
passes compare against). For every `goldcoin_refunds` row in `Broadcast`
it reads the recorded txid, asks the Goldcoin node about that transaction,
and — only if the answer verifies — records the depth and, at or above
`required_goldcoin_confirmations`, commits `Broadcast -> Refunded`.

**It cannot send anything, by type rather than by promise.** The
reconciliation module takes an RPC trait with exactly one method,
`get_raw_transaction`. It is deliberately not the refund path's own
`RefundRpc`, which also carries `send_raw_transaction`. A broadcast from
this path does not compile. It builds no transaction, selects no UTXO,
contacts no signer, signs nothing, and never writes `txid`,
`signed_tx_hex` or any other evidence column — **the stored txid is read
and never replaced.**

**Depth alone is not enough.** Before any transition the returned
transaction is checked against the evidence recorded when it was
broadcast: the node's reported txid must equal the stored txid (a node
answering about a *different* transaction is caught here, not counted as
this refund's depth); output 0 must pay the stored destination for exactly
the stored amount, the same "output 0 is the refund" invariant enforced
before the bytes were signed; and the inputs must be exactly the stored
reserved outpoints, in order.

**Every unknown fails closed and writes nothing:** an unreachable node, a
transaction the node has never heard of (a pruned or resyncing node says
this about perfectly good transactions), a malformed answer, or an absent
`confirmations` field — which for this node means mempool-only, a real
zero, never "unknown so assume fine".

**A mismatch is never repaired automatically.** It is reported with an
explicit instruction not to send another refund, the row is left exactly
as it was, and a human decides. A disagreement between the chain and our
own record of what we broadcast is not something an unattended pass should
resolve.

The pass is idempotent, crash-safe and safe to run forever: the state
decides the outcome, the depth comparison and the terminal write happen
inside one transaction under the write lock, an already-`Refunded` row
only has its depth refreshed, and the `reservation_released` flag plus the
schema CHECK mean capacity can never be freed twice — across restarts
included.

Watch it in `TickReport::glc_refund_reconciliation`
(`checked`/`confirmed`/`pending`/`unavailable`/`mismatched`). **A non-zero
`mismatched` needs a human.** The daemon logs a `WARN` for it and an
`INFO` for each refund that settles.

### Reserve accounting: one release, at the end

A `GlcToSol` request reserves capacity on the **SolanaReserve** when it is
created. The amount-mismatch park moved it to `ManualReview` *without*
releasing that reservation, so the capacity stayed held — for #2477 it
still is. The refund releases it exactly once, at the terminal
`Refunded` transition, using the same accounting move `cancel_request`
makes. Deliberately **not** at `Built` or `Signed`: a refund that never
lands leaves the deposit outstanding and the obligation real, and freeing
capacity early would let new demand consume liquidity against an obligation
that has not been discharged. A schema CHECK permits the
`reservation_released` flag only in the terminal state, and re-entry after
it is set is a clean no-op, so a retried confirmation tick can never
double-free.

### Structural protections

Application checks are backed by schema constraints, so a regression in the
former still cannot produce a double refund:

- `goldcoin_refunds.request_id PRIMARY KEY` — at most one refund per
  request, ever.
- `UNIQUE (source_txid, source_vout)` — one deposit outpoint can never be
  refunded through two requests.
- `goldcoin_refund_inputs UNIQUE (txid, vout)` — one vault UTXO can never
  fund two refunds.
- `CHECK (refund_amount_atomic = observed_amount_atomic)` — the principal
  is the observed deposit.

### LIMITATION: signer-side verification is future hardening

**This release does not provide signer-daemon independent derivation, and
must not be described as if it does.** The orchestrator re-derives every
fact independently of the ledger row and re-validates the assembled
transaction before signing and again before broadcast — two independent
derivations (chain and index) that must agree, which is strictly more
verification than the ordinary payout path performs. But those checks run
in the **orchestrator process**.

The remote vault signers still receive only a 32-byte sighash
(`POST /v1/sign` with `{"payload_hex": ...}`), so a signer cannot see, let
alone verify, what it is signing — it is a blind signature oracle, exactly
as it is for every ordinary payout today. A compromised orchestrator could
therefore skip these checks; the signers would not catch it.

Closing that gap needs a verifying signer endpoint —
`sign_refund(claim, input_index)` — carrying the full `RefundClaim`, with
each signer daemon given its own Goldcoin RPC endpoint so it can re-run the
same two-hop trace and refuse on any mismatch. `RefundClaim` and
`IndependentRefundSource` in `service/src/goldcoin/refund.rs` are shaped
deliberately as that payload and that logic, so the work is a protocol and
deployment change rather than a redesign. It is **not** part of this
release.

## Robinhood Network operations (added 2026-09-06, Phase G)

**Every Robinhood route ships DISABLED and none of the commands below
enables one.** `glc-admin robinhood-preflight` READS the contract's four
route flags and reports them; there is deliberately no command in this
binary that sets one. Since Phase H (docs/35-solana-robinhood-routes-phase-h.md)
`SolToRhn` and `RhnToSol` are executable — each is one existing inbound
half joined to one existing outbound half — and they ship closed on every
gate exactly like the Goldcoin pair.

Two of the commands take `--config` because they need the Robinhood RPC
endpoint, the submitter key environment variable, or the authorization
signer endpoints; the read-only ledger views take `--db` or `--config`
interchangeably.

### Daily / triage

```bash
# One-screen picture: halt state, scan cursor, observation counts,
# in-flight and stalled operations, the RhnToGlc ManualReview queue, and
# the Robinhood reserve if one is configured.
glc-admin robinhood-status --db /var/lib/glc-bridge/ledger.sqlite

# Every RhnToGlc request parked in ManualReview, and whether a refund or a
# settlement has already been begun for it. Those are opposite,
# irreversible answers to the same question; at most one can exist.
glc-admin robinhood-manual-review-list --db /var/lib/glc-bridge/ledger.sqlite

# Full state of Robinhood operations: authorization digest, how many of
# the required signatures were collected, submitter, nonce, whether the
# signed bytes are persisted, transaction hash, receipt status,
# confirmations, failure reason.
glc-admin robinhood-tx-show --db /var/lib/glc-bridge/ledger.sqlite
glc-admin robinhood-tx-show --db /var/lib/glc-bridge/ledger.sqlite --request-id 42
glc-admin robinhood-tx-show --db /var/lib/glc-bridge/ledger.sqlite --stalled

# The submitter's nonce picture. READ-ONLY, and deliberately so: nothing
# in this binary sets, resets, skips or reallocates a nonce. The allocator
# is the ledger's own maximum inside the same write transaction that
# stores it, and editing that by hand would reintroduce the
# duplicate-broadcast window the design removes.
glc-admin robinhood-nonce-status --config /etc/glc-bridge/config.toml

# The Robinhood reserve as a THIRD independent reserve: ledger figures in
# canonical 8dp, then the on-chain contract balance, encumbered reserve
# and both rolling-limit buckets in Robinhood-native 18dp. Never netted
# against the Goldcoin or Solana reserve.
glc-admin robinhood-reserve --config /etc/glc-bridge/config.toml
```

### Preflight (before any route is opened)

```bash
glc-admin robinhood-preflight --config /etc/glc-bridge/config.toml
```

Every check reports **PASS**, **FAIL** or **UNVERIFIED**.

**UNVERIFIED is not PASS.** It means either the check could not run (an
earlier one failed and preflight stopped there) or the property is not one
an RPC read can establish at all. Every **token security property** is
permanently UNVERIFIED: mint authority, blocklist/freeze, transfer hooks,
fee-on-transfer, pause and proxy upgradeability are properties of the
token's CODE and its governance, and a successful `decimals()` read says
nothing about any of them. Establishing them is a separate mainnet token
review.

Route flags default to expecting **all four closed**, which is how this
ships. A deployment mid-rollout names the ones it expects open, so that an
UNEXPECTEDLY open route is a FAIL rather than something nobody looked at:

```bash
glc-admin robinhood-preflight --config /etc/glc-bridge/config.toml \
    --expect-route-enabled GlcToRhn,RhnToGlc
```

### The local `RobinhoodReserve.paused` gate (added 2026-09-10)

```bash
glc-admin robinhood-local-pause --db /var/lib/glc-bridge/ledger.db \
    --paused true --note "incident OPS-1300, stopping GlcToRhn"

glc-admin robinhood-local-pause --db /var/lib/glc-bridge/ledger.db \
    --paused false --note "OPS-1300 resolved, reopening GlcToRhn"
```

#### What this flag is

`reserve_ledger.paused` on the **`RobinhoodReserve`** row. It is a term of
the same `InboundAdmissionGates` evaluator every fold and `GET /chains`
use, so it is one term of the `available` verdict published for
`GlcToRhn`. With it set, `GlcToRhn` is unavailable — on its own, with
every other gate wide open.

It is what `glc-admin robinhood-status` and `glc-admin robinhood-reserve`
print as:

```
Robinhood reserve  paused=true
```

Both commands now print a legend under that line naming the flag and this
remedy, because reading it as "the Robinhood leg is paused" is the misread
that made this hard to diagnose.

#### What it is NOT

**`RobinhoodReserve.paused` = the local `GlcToRhn` reserve gate.** It is
separate from, never reflects, and is never changed by:

| Flag | Where it lives | Its own command |
| --- | --- | --- |
| `depositsPaused` / `payoutsPaused` | the `GlcRobinhoodBridge` **contract**, on chain | `glc-admin robinhood-governance-pause` (2-of-3 quorum) |
| `routeEnabled(route)` | the **contract**, on chain | `glc-admin robinhood-governance-route` (2-of-3 quorum) |
| `bridge_routes.enabled` | this **ledger** | `glc-admin robinhood-route-enable` / `-disable` |
| `[routes]` per-route flags | the **config file** | edit `config.toml`, restart the daemon |
| `route_admission.admission_closed` | this **ledger** | `glc-admin route-admission-close` / `-open` |
| `GoldcoinReserve` / `SolanaReserve` `paused` | this **ledger** | `glc-admin pause` / `unpause` |

Every one of those is evaluated independently on every transfer. Clearing
this one opens none of them, and none of them can clear this one.

**It does not gate `RhnToGlc`.** That route settles out of the **Goldcoin**
reserve (`Direction::destination_reserve`), so its local gate is
`GoldcoinReserve`'s `paused`/`admission_closed`, which `glc-admin status`
prints and this command never touches. A `GlcToRhn` stop and a `RhnToGlc`
stop are two different commands on two different reserves, deliberately.

#### Why this command exists (the incident, 2026-09-10)

The flag had always been READ and was never WRITABLE. `glc-admin
pause`/`unpause` and the admin API's `POST /pause` both parse
`goldcoin|solana` and reject anything else, so a `RobinhoodReserve` row
sitting at `paused=1` closed `GlcToRhn` with the contract unpaused, both
`routeEnabled` flags true, a healthy 3-of-3 signer quorum, a holding
reserve invariant and spare capacity — and no supported way to clear it.
`RhnToGlc` was unaffected throughout, exactly as the table above predicts,
which is what made the shape of the problem visible.

#### Pausing is unconditional; unpausing is guarded

`--paused true` is an emergency stop and is **never** refused, however bad
the reserve looks. Refusing to stop taking demand is never the safe answer,
and this matches `close-admission` and `route-admission-close`.

`--paused false` runs, with **no override**:

1. The hard reserve invariant for `RobinhoodReserve`
   (`total_reserve_balance >= protected_minimum + reserved_liquidity`) —
   the same `Ledger::check_invariant` call `open-admission` makes, through
   the same shared guard, so this command can never be the weak way around
   those checks.
2. The mature-UTXO floor and the confirmed-liquidity buffer — the same two
   calls, which short-circuit for any reserve that is not `GoldcoinReserve`
   (a UTXO pool is a Goldcoin concept; Robinhood's reserve is a contract
   balance).
3. **The availability evaluator itself**, re-asked with the pause bit
   cleared: "would `GlcToRhn` admit a minimum-sized transfer once this flag
   goes?" If what would still refuse it is a CAPACITY or liquidity
   condition, the unpause is refused and says which gate and what the
   confirmed headroom is. This is `InboundAdmissionGates::route_blocker` —
   the same function `GET /chains` and `fold_robinhood_deposit` call — not
   a second opinion about capacity, so it cannot drift from what the public
   API publishes.

A remaining **operator** switch (route admission, reserve admission) is
deliberately not a refusal: those are separate, deliberately-set gates with
their own audited commands, and refusing here would make this command's
success depend on state it must not touch.

#### Everything else about it

- **Idempotent.** Setting the value it already has succeeds and changes
  nothing — but is still audited, honestly, as `paused=true -> paused=true`
  rather than as a transition that did not happen.
- **Audited, including refusals.** Every invocation appends an
  `admin_audit_log` row (`actor cli:<user>`, action `pause`/`unpause`,
  target `robinhood`, `old_value`/`new_value` as `paused=<bool>`, the
  mandatory `--note`). A refused unpause is audit-relevant too — someone
  tried — and lands in the same log with its refusal message.
- **`--note` is mandatory**, like every other mutation on this surface.
- **Prints before/after**, the audit id, the affected scope
  (`GlcToRhn local reserve gate only`), that `RhnToGlc` is not controlled
  by this flag, and the list of things it did not touch.
- **No restart needed.** Nothing is cached; the gate is re-read on every
  request.
- Read it back with `glc-admin robinhood-status --db PATH` or
  `glc-admin robinhood-reserve --config PATH`.

#### Regression coverage

`service/tests/robinhood_local_pause.rs` — 19 tests, driving the real
audited path and the real binary: that the flag closes and reopens
`GlcToRhn` and no other route, that `RhnToGlc`/`SolToGlc`/`GlcToSol` are
untouched in both directions, that `bridge_routes` and the other two
reserve rows come out field-for-field identical, that the audit row is
written for successes and refusals alike, that repeated calls are
idempotent, and that unpause refuses a broken invariant and an exhausted
capacity state while pause is always allowed.

### Opening a Robinhood route in the ledger (added 2026-09-10)

The `bridge_routes` table is the LEDGER leg of the route gate. Schema
**v24** creates it and seeds one row per route at the value the gate
already resolved to before the table existed — `GlcToSol`/`SolToGlc`
enabled, all four Robinhood routes disabled — so the migration itself
changes nothing. Upgrading the daemon applies it; there is no separate
migration step and no SQL to run by hand.

Opening a route in ledger state is a deliberate, audited operator write:

```bash
glc-admin robinhood-route-enable --db /var/lib/glc-bridge/ledger.db \
    --route GlcToRhn --note "Robinhood launch, ticket OPS-1234"
glc-admin robinhood-route-enable --db /var/lib/glc-bridge/ledger.db \
    --route RhnToGlc --note "Robinhood launch, ticket OPS-1234"
```

and it is reversible the same way:

```bash
glc-admin robinhood-route-disable --db /var/lib/glc-bridge/ledger.db \
    --route GlcToRhn --note "incident OPS-1300, closing the route"
```

Nothing is cached — the gate re-reads this on every request — so **no
daemon restart is needed** in either direction.

**This is one gate of three, and enabling it opens nothing on its own.**
The service config's per-route flag, both chain adapters' capability, the
contract's own `routeEnabled`/`depositsPaused`/`payoutsPaused`, preflight,
the signer quorum, reserve availability and the local pause each still
decide every transfer independently.

In particular this command and `glc-admin robinhood-governance-route`
(see "Robinhood governance" below) are **two different sides of the same
launch, and both are required**: this one writes THIS SERVICE's ledger
flag over `--db` and contacts no chain; that one submits the on-chain
governance transaction that sets the CONTRACT's flag under 2-of-3 quorum.
Neither substitutes for the other, and either one alone leaves the route
closed.

Read this gate back with:

```bash
glc-admin robinhood-routes --config /etc/glc-bridge/config.toml
```

which reports every route's recorded `bridge_routes` state, its
`updated_at`, any `disabled_reason`, and — because `--config` was given —
that config's own `[routes]` gate beside it. It resolves nothing: a ledger
with no `bridge_routes` table is reported as having none, rather than as a
set of defaults, because "disabled" and "never recorded" have different
remedies. Then run `glc-admin robinhood-preflight --config
/etc/glc-bridge/config.toml --expect-route-enabled GlcToRhn,RhnToGlc` once
the rollout expects them open, for the CONTRACT's own flags.

**There is deliberately no single "is this route open?" command.** The
third gate is chain-adapter capability, and it is evaluated inside the
running daemon's process — `GlcToRhn`/`RhnToGlc` are `Operational` only
where that process's startup preflight verified the deployment. No read of
a file or a database can establish it, so nothing here claims to.
`GET /chains` on the running daemon is the only surface that reports the
resolved AND, because it is the only one evaluating it.

`scripts/bridge-admin.sh` (the interactive console) draws all of this in
one screen, with each gate named separately.

**Which routes it accepts.** The four custody-contract routes:
`GlcToRhn`, `RhnToGlc`, `SolToRhn` and `RhnToSol` (the last two since
Phase H, docs/35-solana-robinhood-routes-phase-h.md). The migration seeds
every one of them at `0`.

- `GlcToSol`/`SolToGlc` are refused. Their controls are the local pause
  and admission control above; a second switch here would be one no
  reserve invariant or liquidity check knows about.

The refusal is audited, like every other mutation on this surface: an
operator who tried and was refused is itself audit-relevant.

If the command reports that the ledger has **no `bridge_routes` row**, the
database has not run v24 — start this version's daemon against it once to
migrate, then retry. The command never creates the row itself.

### Robinhood refunds (RhnToGlc)

Returns a Robinhood depositor's exact principal when their deposit cannot
safely complete to Goldcoin: an undeliverable destination, a route that
will not open, a reserve that cannot cover it, or an operator's explicit
decision after review. Never automatic.

```bash
# STRICT READ-ONLY DRY RUN. Prints every ledger-side check as PASS/FAIL.
# Contacts no signer, reads no chain, writes nothing, broadcasts nothing.
glc-admin robinhood-refund --config /etc/glc-bridge/config.toml \
    --request-id 42 --note "undeliverable destination, ticket OPS-1234"

# The real thing.
glc-admin robinhood-refund --config /etc/glc-bridge/config.toml \
    --request-id 42 --note "undeliverable destination, ticket OPS-1234" --execute
```

The **RECIPIENT** is the obligation's own on-chain `depositor` and the
**AMOUNT** is its own on-chain `amount`, both read back from the contract
immediately before the authorization is built. There is deliberately **no
`--destination` and no `--amount`**: neither is an operator's choice, and
the contract compares both exactly and reverts on any difference. There is
no fee, and there are no partial refunds.

`--execute` runs the startup preflight against the deployed contracts,
re-runs every eligibility check against fresh state, collects the 2-of-3
EIP-712 quorum, broadcasts, then drives the receipt phase and reports the
result. It is **idempotent**: re-running resumes the SAME operation under
the SAME nonce and can never produce a second transfer.

It refuses if a settlement operation already exists, if a Goldcoin payout
transaction exists, if the request is not `RhnToGlc` in `ManualReview`, or
if the obligation is anything but `Pending` on-chain — four independent
checks against four independent sources of truth. **A refund and a
settlement are mutually exclusive**, and whichever lands first makes the
other revert on-chain regardless.

### Exercising a second deployment from an isolated ledger (added 2026-09-11)

A successor `GlcRobinhoodBridge` that is deployed but not yet migrated to
— holding a small test reserve — is exercised from an ISOLATED ledger and
an isolated config, never from the production ones: the production ledger
has ONE `RobinhoodReserve` row, and it belongs to the contract the daemon
serves. A withdrawal driven from it would debit that row for a movement
on a different contract.

The daemon is the only thing that creates a reserve row, and a daemon
must never be started against a fresh ledger (its indexers would re-fold
every historical deposit as a new payout). So the isolated ledger gets
its row from:

```
glc-admin robinhood-reserve-init --config /etc/glc-bridge/config-<name>.toml [--db PATH]
```

which verifies the deployment through the same preflight the daemon uses
(chain id, contract code, protocol family, `token()` = `expected_token`,
decimals, signer set, EIP-712 domain), reads `balanceOf(bridge)` from the
token at the latest block, and creates the row with exactly that balance
(18dp → canonical 8dp, exact or refused), `[reserve.robinhood]`'s
protected minimum and bands, and zero reserved / pending / fees. It
refuses, before any write: a config without `[reserve.robinhood]`; a
deployment that does not verify; an RPC failure; a balance that is not a
whole multiple of 1e10; **a ledger with any Robinhood history** (deposit
observations, outbound operations, Robinhood bridge requests, Robinhood
rebalances); and an existing row that does not already say exactly this.
An identical existing row is a no-op. There is no `--force`.

The isolated config is a copy of the production file with, at minimum:
`[service].db_path` → the isolated file; both `bridge_contract`s → the
second deployment; `[robinhood.indexer].start_block` → its deployment
block; `[reserve.robinhood]` sized for the test reserve; and the
`[[robinhood.settlement.auth_remote_signers]]` endpoints → signer
instances whose `GLC_RHN_SIGNER_VERIFYING_CONTRACT` is the second
deployment. The production instances stay bound to the production
contract; the EIP-712 domain makes the two sets of signatures mutually
useless, which is the point.

### Recovering a deposit made to a retired custody contract (added 2026-09-12)

After a cutover, the scanner filters `eth_getLogs` by the NEW contract —
correctly — so a `deposit()` a user sends to the OLD one (a stale UI, a
bookmarked address) is confirmed on chain, holds their GLC, and is never
observed, never folded, never refundable. Incident 2026-09-12: V1
obligations #29 (300 GLC) and #30 (135 GLC), both `RhnToSol`, made to
`0x1753…f440` after production had moved to `0xbaEd…8DBf`.

The recovery is out-of-band and per transaction, driven from the config
that names the OLD contract (`config-v1.toml`):

```
glc-admin robinhood-recover-deposit --config /etc/glc-bridge/config-v1.toml \
    --tx 0x15e8112dbede74952a93e9365467cded0ec698a4acbd9f8e4c4f5989f2809a97 \
    --tx 0x85dfdd3942cf1eac227e78b5d4129de080bdbe912aac99a390a3d1fd1f2184c9
# dry run: receipt fetched, status checked, the ONE DepositCreated from the
# configured contract decoded with the scanner's decoder, finality (depth)
# and canonical block hash proven, amounts/destination printed. Nothing written.
glc-admin robinhood-recover-deposit --config /etc/glc-bridge/config-v1.toml --tx ... --tx ... --execute
# records each observation as Final under (robinhood, contract, index) and
# folds it with the route CLOSED: one ManualReview request per deposit,
# holding no capacity, refundable through `robinhood-refund` with the OLD
# contract's config and signer instances. Rerunning writes nothing.
```

What it never does: pay out, refund, touch any other request, move the
scanner's cursor or anchors, or accept an amount/destination from the
command line — every figure is the log's. And the thing to do FIRST, so
it does not happen again: pause the old contract on chain
(`robinhood-governance-pause --config config-v1.toml --scope deposits
--paused true --execute`, then `--scope payouts`) and fix the UI's
contract address.

### Robinhood reserve withdrawal to the treasury (added 2026-09-11)

The EVM counterpart of `glc-treasury-withdraw` (Solana): an intentional,
operator-initiated movement of reserve GLC out of `GlcRobinhoodBridge` to
the ONE address it was constructed with — its immutable `TREASURY`. It is
the fourth and last way GLC leaves the contract, beside a user payout, a
depositor refund and a full migration, and it changes none of those.

**What an operator supplies: a rebalance id.** The amount is the approved
request's; the destination is read from the contract; the pause state is
read from the contract. There is deliberately **no `--destination` and no
`--amount`** on the executor — the same posture `glc-treasury-withdraw`
adopted after the 2026-09-02 incident.

```bash
# 1. Propose. --amount is canonical 8dp atomic for EVERY direction; the
#    widening to the contract's 18dp happens once, at execution, inside a
#    typed conversion. --amount-glc takes whole GLC (at most 8 decimals)
#    and converts exactly. The confirmation line echoes both units.
glc-admin rebalance-propose --db /var/lib/glc-bridge/ledger.db \
    --direction robinhood --kind withdraw --amount-glc 25000 \
    --by ops:alice --required-approvals 2 --note "Q3 treasury sweep, ticket OPS-2100"

# 2. Approve, by the required number of DIFFERENT operators.
glc-admin rebalance-approve --db /var/lib/glc-bridge/ledger.db --id 7 --by ops:bob
glc-admin rebalance-approve --db /var/lib/glc-bridge/ledger.db --id 7 --by ops:carol

# 3. Pause BOTH directions on the contract. This is the withdrawal's
#    authoritative precondition, read live by eth_call; a guardian's
#    guardianPause(true, true) satisfies it just as well.
glc-admin robinhood-governance-pause --config /etc/glc-bridge/config.toml \
    --scope deposits --paused true --note "OPS-2100 treasury sweep" --execute
glc-admin robinhood-governance-pause --config /etc/glc-bridge/config.toml \
    --scope payouts --paused true --note "OPS-2100 treasury sweep" --execute

# 4. DRY RUN. Reads the ledger AND the contract; prints every check
#    PASS/FAIL, the amount in GLC / canonical / 18dp, the reserve before
#    and after on both sides, the treasury, the pause flags and the
#    signer quorum. Writes nothing, signs nothing, broadcasts nothing,
#    consumes no nonce. Exits non-zero if any check fails.
glc-admin robinhood-treasury-withdraw --config /etc/glc-bridge/config.toml \
    --rebalance-id 7 --note "OPS-2100"

# 5. Execute. Re-runs every check against fresh state, collects the
#    2-of-3 EIP-712 quorum, signs with the submitter key, broadcasts, and
#    drives the receipt to the configured confirmation depth.
glc-admin robinhood-treasury-withdraw --config /etc/glc-bridge/config.toml \
    --rebalance-id 7 --note "OPS-2100" --execute --json
```

**Exit status means final outcome.** `0` is returned ONLY when the
operation is `Finalized` — mined, `status = 1`, the bridge's event present
in the receipt, the contract's replay guard confirming `(0x0C, requestId)`
executed, and the confirmation depth reached — AND the rebalance request
has moved to `Confirmed` on the strength of that receipt. A reverted
receipt exits `1` (operation `ManualReview`, rebalance `Failed`). A
broadcast still unresolved when `--wait-secs` (default 600) runs out
exits `1` with `state = "Broadcast"`; **re-run the same command** to
resume it. Resuming is idempotent: the same operation row, the same
nonce, the same signed bytes (or a fee-bumped replacement under the same
nonce). A second operation for the same approval cannot be created — the
database refuses it.

`--json` prints one object: `operation_id`, `rebalance_id`, `state`,
`rebalance_state`, `success`, `dry_run`, `tx_hash`, `nonce`,
`amount_atomic` (18dp, decimal string), `amount_canonical_atomic` (8dp),
`amount_glc`, `destination`, `receipt_status`, `receipt_block_number`,
`confirmations`, `required_confirmations`, `failure_reason`, `checks[]`,
`onchain{}`, `ledger_reserve_before{}`, `errors[]`.

**Reserve safety, on both sides.** The ledger refuses unless the
post-withdraw balance keeps `protected_minimum`, `reserved_liquidity` and
`pending_obligations` whole; the contract refuses unless
`balanceOf(bridge) - amount >= encumberedReserve()` (its protected floor
plus every unsettled depositor's principal), through the same
`_requireSpendableReserve` every payout obeys. There is **no per-withdrawal
cap and no rolling limit**, deliberately, exactly as on Solana: fixing
WHERE the reserve can go is the bound; capping HOW MUCH would only
constrain legitimate treasury operations.

**Signers.** Each custody domain opts in separately: the signer's
`GLC_RHN_SIGNER_ALLOWED_ACTIONS` must include `treasury_withdraw` AND
`GLC_RHN_SIGNER_ALLOWED_TREASURIES` must list the treasury, read by that
domain's own operators from the deployed contract. Neither is set by
default; a signer binary that merely understands the protocol signs
nothing. The domain's `GLC_RHN_SIGNER_MAX_AMOUNT_ATOMIC` ceiling does
**NOT** apply to withdrawals — deliberately, and unlike the Solana
signer's `max_withdrawal_amount`. The treasury allowlist is the bound.

**No artificial amount or rate limit, anywhere on the path.** Not on the
contract (no `outboundMin`/`outboundMax`, no rolling window, no
percentage), not in the ledger (`rebalance-propose` accepts any amount),
not in the executor, not in the signer. The ONLY amount constraints are
accounting: the contract's `protectedMinReserve` and unsettled depositor
principal (`encumberedReserve()`), and the ledger's `protected_minimum`,
`reserved_liquidity` and `pending_obligations`. To withdraw the ENTIRE
reserve — a deliberate drain before a migration — first clear every
liability (settle/refund to zero), then lower the two floors to zero
(`setLimits` with `protectedMinReserve = 0` by 2-of-3, and
`[reserve.robinhood].protected_minimum = 0`), and the full `balanceOf`
becomes withdrawable in one operation.

```bash
# Inspect, at any time. Read-only.
glc-admin robinhood-treasury-withdraw-status --db /var/lib/glc-bridge/ledger.db
glc-admin robinhood-treasury-withdraw-status --db /var/lib/glc-bridge/ledger.db --rebalance-id 7
```

The admin API serves the same view read-only at
`GET /robinhood/treasury-withdrawals` and
`GET /robinhood/treasury-withdrawals/{operation_id}`. There is no
execute endpoint: execution stays a command line holding the submitter
key, exactly as the Solana refund's does.

**Rotating the treasury** means migrating to a successor contract
constructed with the new address. `TREASURY` is immutable by design —
stronger than the Solana `RebalancePolicy` allowlist, which a threshold
of keys can change behind a timelock; this cannot be changed by any set
of keys. See `docs/34-robinhood-reserve-withdrawal.md`.

### Clearing a halted Robinhood indexer

A halt means the indexer recorded, or was about to record, something it
could not stand behind. Clearing it is a claim that the underlying
condition is gone — never a way to make the alert stop.

```bash
glc-admin robinhood-clear-halt --config /etc/glc-bridge/config.toml \
    --expect-reason chain_id_mismatch \
    --note "endpoint repointed to the correct network, ticket OPS-1235"
# ...then re-run with --execute.
```

- `--expect-reason` is **required** and must equal the stored halt reason
  (`observation_conflict` | `post_finality_reorg` |
  `reorg_beyond_retained_anchors` | `chain_id_mismatch` |
  `unexpected_contract_route`). Naming a different one is a refusal: a halt
  whose cause has not been diagnosed must not be cleared.
- It refuses while **any** Robinhood operation is in flight — an unresolved
  broadcast is verified against the indexer's view of the chain.
- A **reorg** halt additionally requires `--acknowledge-orphaned-finality`,
  after reviewing the finalized observations a reorg may have invalidated
  (the count is printed).
- A **chain-id / wrong-contract** halt additionally requires `--config`, and
  is cleared only if a live preflight against the configured deployment
  passes right now — re-verified from the chain, never asserted on the
  command line.

Clearing a halt is not a fix for what caused it. If the condition is still
true the indexer halts again on its next tick.

### What none of these can do

No force-complete. No balance movement other than a refund whose recipient
and amount come from the chain. No nonce rewrite. No abandonment — the
on-chain path that closes an obligation while RETAINING a depositor's
principal has no representation in this service and gains none here.

## Robinhood governance (added 2026-09-09)

Changing what the **deployed contract** enforces, as opposed to what this
service *states*. The two are deliberately separate tools:

| | changes | tool |
| --- | --- | --- |
| Backend policy | `[robinhood.policy]` in the config file | `scripts/chain-policy.sh` |
| On-chain enforcement | `GlcRobinhoodBridge` storage, under 2-of-3 quorum | the five `robinhood-governance-` commands below |

The governance commands hold **no figures of their own**. `set-limits` reads
`[robinhood.policy]` from the supplied config and reconciles the contract to
it. Change the policy first, then reconcile.

```
glc-admin robinhood-governance-set-limits --config PATH --note TEXT [--execute]
    [--inbound-min N] [--outbound-min N] [--protected-min N]
glc-admin robinhood-governance-pause --config PATH --scope <deposits|payouts>
    --paused <true|false> --note TEXT [--execute]
glc-admin robinhood-governance-route --config PATH
    --route <GlcToRhn|RhnToGlc|SolToRhn|RhnToSol>
    --enabled <true|false> --note TEXT [--execute]
glc-admin robinhood-governance-commit-migration --config PATH
    --successor 0xADDRESS --note TEXT [--execute]
glc-admin robinhood-governance-finalize-migration --config PATH
    --successor 0xADDRESS --note TEXT [--execute]
```

### Dry run is the default

Without `--execute` each command reads the chain, derives the proposal, prints
the exact before/after and the EIP-712 digest a quorum would have to sign, and
stops. No custody domain is contacted, no signature is gathered, no
transaction is built, and the governance nonce is not consumed.

### Where `setLimits`'s values come from

| field | source |
| --- | --- |
| `inboundMax`, `outboundMax` | `[robinhood.policy].per_transfer_limit` |
| `inboundRollingLimit`, `outboundRollingLimit` | **half** of `[robinhood.policy].rolling_daily_limit` |
| `inboundMin`, `outboundMin`, `protectedMinReserve` | **preserved** from current on-chain state |

The halving is the fixed-bucket rule documented above: the contract's window
resets wholesale, so its reachable worst case is 2x the configured number. An
odd `rolling_daily_limit` is refused outright rather than rounded —
`RobinhoodPolicyBinding` will not build. `setLimits` replaces the WHOLE
struct, which is why the three minimums are carried across rather than
defaulted: a tool that defaulted one would silently rewrite a value nobody
asked about, under a signature that covered it.

Change a minimum only with the explicit flag, in 18-decimal atomic units.

### What `--execute` does, in order

1. **Re-reads `governanceNonce` and `signerEpoch`.** A plan is a photograph;
   any governance action anywhere in between invalidates it. Caught here,
   before a quorum is asked to look at anything.
2. **Gathers exactly 2 signatures from DISTINCT custody domains.** The
   contract requires exactly `SIGNER_THRESHOLD` and refuses `first == second`.
   Three are never gathered and two from one domain is refused locally.
3. **Simulates** with `eth_estimateGas` from the submitter's own address. A
   revert here costs nothing; a revert after broadcast costs the nonce.
4. **Broadcasts** and waits for a receipt with `status == 1`.
5. **Re-reads the contract** and requires it to hold exactly what the proposal
   said. A successful receipt proves a transaction executed, not that it meant
   what was intended.

### What it can never do

- Enable a route as a consequence of a limit or a pause change. Each payload
  carries one action and changes exactly that action's fields; the tests
  assert it.
- Enable `GlcToSol` or `SolToGlc`. The contract does not model them, and
  the CLI, the encoder and every signer refuse them. (`SolToRhn` and
  `RhnToSol` ARE governable since Phase H, under their own contract bytes
  `0x03`/`0x04`; the on-chain flag is one gate of four and opens nothing
  on its own.)
- Rotate signers, rotate guardians, or abandon an obligation. None has a
  representation anywhere in this stack. (Committing and finalizing a
  migration DO, since 2026-09-11 — see "Migrating to a successor
  contract" below — and are the two highest-impact actions here.)
- Sign with a dev signer set. Governance requires
  `operators.mode = "production"` and `[[robinhood.settlement.auth_remote_signers]]`.
- Edit the config file or restart the daemon.

### Signers must be upgraded FIRST

Governance rides a new signer protocol, `POST /v3/sign-evm-governance`.
`/v2/sign-evm-auth` is untouched and a signer serving only v2 answers `404`,
which fails closed.

Every custody domain must additionally **opt in**:

```
GLC_RHN_SIGNER_ALLOWED_GOVERNANCE_ACTIONS=set_limits,set_pause,set_route_enabled
```

**Unset means none, and none refuses everything.** Deploying the new signer
binary does not, on its own, widen what a custody key will sign — that stays a
decision each domain makes through its own change process. The signer logs
which posture is live at every start.

`commit_migration` and `finalize_migration` are granted **by name** and are
never implied by the three above — a domain configured with the line above
keeps refusing both. For the duration of a migration, and only then:

```
GLC_RHN_SIGNER_ALLOWED_GOVERNANCE_ACTIONS=set_limits,set_pause,set_route_enabled,commit_migration,finalize_migration
```

and remove the two again once the migration has finalized.

A signer independently re-derives the governance digest from the request's
structured fields and signs only the digest it derived; `expected_digest` is
carried as a cross-check and is never signed. There is still no endpoint
anywhere that accepts arbitrary bytes or a bare digest.

### Migrating to a successor contract

The contract is not upgradeable; a capability it lacks lives in a NEW
deployment, and the reserve reaches it through `commitMigration` →
`finalizeMigration`. This is the heaviest thing governance can do: the
commit closes every inbound route on the old contract forever, and the
finalize moves its ENTIRE balance. Read
docs/34-robinhood-reserve-withdrawal.md §3.1 and §10 first.

Preconditions, all checked by the dry run and named on refusal:

1. Both directions paused ON CHAIN (`robinhood-governance-pause` twice, or
   one guardian's `guardianPause(true, true)`). The service-side reserve
   pause is not this.
2. The successor deployed, its bytecode verified against this repository's
   build (`forge build`, compare `deployedBytecode` with immutables masked
   and the metadata tail stripped — the same comparison
   docs/34 §10.3 records for V1), and its constructor arguments checked:
   the SAME token, signer set, guardian set, protocol ids and limits.
3. Every obligation on the old contract at `Settled`, `Refunded` or
   `Abandoned`: `outstandingRefundableCount() == 0` and
   `outstandingRefundablePrincipal() == 0`. Settle what settled, refund
   the rest (`robinhood-refund`). Finalize is refused while any remains.
4. Every custody domain has `commit_migration` and `finalize_migration` in
   its `GLC_RHN_SIGNER_ALLOWED_GOVERNANCE_ACTIONS` for the duration.

```
# 1. Commit. Dry run prints the successor checks, the pending liability, and
#    the digest; --execute gathers the quorum at the current nonce.
glc-admin robinhood-governance-commit-migration --config /etc/glc-bridge/config.toml \
    --successor 0xSUCCESSOR --note "OPS-xxxx V2 migration: commit"
glc-admin robinhood-governance-commit-migration --config /etc/glc-bridge/config.toml \
    --successor 0xSUCCESSOR --note "OPS-xxxx V2 migration: commit" --execute

# 2. Wait for migrationFinalizableAt(). On a deployment that carries a
#    MIGRATION_DELAY (the first one does: 48 hours) this is commit + delay,
#    enforced by THAT contract's bytecode; the dry run refuses earlier with
#    the remaining seconds. On a deployment without one it is the commit
#    time — and then the wait is the OPERATIONAL hold docs/34 §10.2 requires
#    so a guardian's veto has a window to land in. Publish the commit tx to
#    every guardian either way.

# 3. Finalize. --successor must equal migrationSuccessor() on chain: the
#    finalize quorum re-approves that address, it does not approve whatever
#    happens to be committed.
glc-admin robinhood-governance-finalize-migration --config /etc/glc-bridge/config.toml \
    --successor 0xSUCCESSOR --note "OPS-xxxx V2 migration: finalize"
glc-admin robinhood-governance-finalize-migration --config /etc/glc-bridge/config.toml \
    --successor 0xSUCCESSOR --note "OPS-xxxx V2 migration: finalize" --execute

# 4. Cut over. The old contract is terminal; nothing about it is served.
#    - [robinhood.indexer].bridge_contract and start_block -> the successor
#      and its deployment block; [robinhood.settlement].bridge_contract too.
#    - Every custody domain: GLC_RHN_SIGNER_VERIFYING_CONTRACT -> the
#      successor (it is in the EIP-712 domain; every old signature is void),
#      and remove commit_migration/finalize_migration from the allow-list.
#    - Restart, then:
glc-admin robinhood-preflight --config /etc/glc-bridge/config.toml
#      must PASS treasury_withdraw_capability and no_pending_migration on the
#      successor, and reconciliation must read balanceOf(successor) as the
#      ledger's Robinhood reserve balance.
#    - Routes on the successor are all DISABLED and both directions PAUSED
#      by construction; re-enable each deliberately with
#      robinhood-governance-route / robinhood-governance-pause.
```

The ledger keys every Robinhood obligation by
`(chain, contract, obligation_index)`, so the successor's counter restarting
at 0 does not collide with the predecessor's rows.

### Golden digest vectors

`contracts/test/fixtures/eip712-golden.json` carries one vector per
governance action — `setLimits`, `setPause`, `setRouteEnabled`,
`commitMigration`, `finalizeMigration` — each pinning
the action byte, the payload hash, the struct hash and the final EIP-712
digest, all bound to `governanceNonce = 5`, `signerEpoch = 7` and
`expiry = 1800000000`.

**Neither side generates the file.** `contracts/test/GoldenDigests.t.sol`
asserts the deployed contract produces every value; `governance::tests`'s
`golden_*` cases assert the Rust transcription produces the same ones. A
drift on either side fails that side against a file it cannot quietly edit
into agreement.

The vectors are chosen to make a mismatch *detectable*, not merely possible:
the seven `governanceLimits` figures are all distinct and the pause pair is
asymmetric `(true, false)`, so transposing any two fields changes the hash.
Both suites additionally assert that transposing fields — including
`signerEpoch` with `nonce`, the dangerous pair, since both are small integers
sitting next to each other — does not reproduce the pinned bytes.

Those limit figures are **test vectors, not policy**. Production limits come
from `[robinhood.policy]` and are derived; nothing reads a limit from this
file.

Regenerate only if the contract's encoding deliberately changes:

```
cd contracts && forge test --match-contract GoldenDigests
cd service   && cargo test --lib robinhood::governance::tests::golden
```

## Per-route fees (added 2026-09-10)

**Every executable route has its own fee.** `GlcToSol`, `SolToGlc`,
`GlcToRhn` and `RhnToGlc` each resolve exactly one `fee_bps`, from the
config's `[fees]` table. There is no global rate and no per-chain default
behind them: a route with no configured rate is a **startup error**, never
another route's number.

```toml
[fees]
GlcToSol = 300
SolToGlc = 300
GlcToRhn = 600
RhnToGlc = 600
```

### Reading them

```bash
glc-admin fees-show --config /etc/glc-bridge/config.toml
glc-admin fees-show --config /etc/glc-bridge/config.toml --route RhnToGlc
```

Reports each route's rate AND where it came from — an explicit `[fees]`
entry, or the migration fallback described below. Read-only; it cannot
modify a file.

### Changing exactly one route

```bash
# Dry run first — this is the default, and it writes nothing.
glc-admin fees-set --config /etc/glc-bridge/config.toml \
    --route RhnToGlc --fee-percent 4 --note "OPS-2400 commercial review"

# Then, having read the before -> after:
glc-admin fees-set --config /etc/glc-bridge/config.toml \
    --route RhnToGlc --fee-percent 4 --note "OPS-2400 commercial review" --execute
```

`--fee-percent` takes what an operator types (`6`, `3`, `1.5`);
`--fee-bps` takes the exact machine value. Pass one, never both.

**It changes exactly one route, and proves it.** The edited file is
reloaded by the real config parser and every OTHER route's resolved rate is
compared against what it was before the edit; if any of them moved, the
edit is refused rather than installed. A timestamped backup is taken first
and the file is replaced with one atomic rename, so comments and unrelated
sections survive byte-for-byte.

The interactive console wraps this route-first:

```
scripts/bridge-admin.sh --config /etc/glc-bridge/config.toml
  -> Goldcoin <-> Robinhood -> "Change one route's fee %"
```

### A restart is required; nothing on chain is

The running daemon keeps pricing at the old rate until an operator restarts
it deliberately. **No governance transaction is involved:** the deployed
`GlcRobinhoodBridge` stores no fee at all — its `Limits` struct carries
minimums, maximums, rolling limits and a protected minimum, and nothing
else — so a fee change is a config change and nothing more.

**In-flight requests are unaffected.** Each request snapshots its rate at
creation/fold time and settles at THAT rate
(`amount_conversion::verify_fee_breakdown`), so a change applies to new
requests only.

### Which rates are allowed

**Any rate from 0 to 9,999 basis points (0% to 99.99%).** There is no list
of previously-charged rates and no rebuild involved in moving between them:
a fee is configuration, and `4%` is a fee like any other.

The bounds are arithmetic, not policy:

| rate | result | allowed |
|---|---|---|
| `0` | `fee = 0`, `net = gross` — the route is free | **yes**, deliberately |
| `1` .. `9_999` | `fee = floor(gross × bps / 10_000)`, `net = gross − fee` | **yes** |
| `10_000` (100%) | `fee = gross`, so `net = 0` on every transfer — the route can never deliver anything | no |
| `> 10_000` | `fee > gross`, so the net entitlement would be negative, which unsigned accounting cannot represent | no |

A config naming an out-of-range rate refuses to boot, before any request is
priced.

### What still fails closed

Removing the rate allowlist did not weaken the fee-bypass protection, which
was never the allowlist: it is
`amount_conversion::verify_fee_breakdown`. Every settlement, attestation
and recovery path recomputes the breakdown from the request's stored gross
and stored rate, requires the stored fee and net to reconcile **exactly**,
and builds the settlement from the freshly recomputed figures rather than
the stored ones. A row whose three amounts disagree is refused whatever
rate it claims.

What is no longer caught is a row rewritten *wholesale and consistently* to
a different rate — gross, rate, fee and net all edited to agree. That
requires write access to the ledger, which is the same access that could
rewrite the destination address; the defence there is the database's own
access control and the audit trail, not a list of numbers compiled into the
binary.

### Migration: what a config with no `[fees]` section does

Every production config file today has no `[fees]` section, and **keeps
loading unchanged**. The rates are resolved once, at load, from the
documented fallback:

| route | before this change | fallback |
|---|---|---|
| `GlcToSol`, `SolToGlc` | compiled-in `BRIDGE_FEE_BPS` | same |
| `GlcToRhn`, `RhnToGlc` | `[robinhood.policy].fee_bps` | same, and `BRIDGE_FEE_BPS` when that section is absent |

So the upgrade changes no economics. The fallback is a load-time
convenience with a deliberate shelf life — it exists so this change breaks
no running deployment, and it is out of the picture the moment a config
states `[fees]`.

**The first `fees-set --execute` creates the section.** Creating it makes
it authoritative, so it is created COMPLETE: seeded with the rates already
in force, plus the one change. The command lists which keys it had to seed.

### `[robinhood.policy].fee_bps` after migration

Once `[fees]` exists it is what prices Robinhood routes;
`[robinhood.policy].fee_bps` still states a number but no longer prices
anything. Both `fees-show` and `chain-policy-show` report the disagreement
rather than silently preferring one. `[robinhood.policy]`'s
`per_transfer_limit` and `rolling_daily_limit` are unaffected and remain
the governance binding for the contract's `setLimits`.

### The two Solana<->Robinhood routes may go unpriced while disabled

`SolToRhn` and `RhnToSol` became executable in Phase H, after every
production `[fees]` table was written, so a table that omits them keeps
loading unchanged — as long as both stay disabled in `[robinhood]`.
Enabling either without pricing it is a startup error, never a rate
borrowed from another route or from the compiled-in constant. An unpriced
cross route folds nothing: a Robinhood-bound Solana deposit then folds as
`SolToGlc` exactly as before, and a finalized `RhnToSol` observation stays
recorded and unfolded. Price one with `fees-set --route SolToRhn` (the one
edit with no "before"); `fees-show` lists an unpriced cross route as
`UNPRICED`. See docs/35-solana-robinhood-routes-phase-h.md.

## Chain policy management (added 2026-09-09)

The fee rate and the transfer ceilings for one bridge network, managed as
configuration. Read-only unless you explicitly pass `--execute`.

### The interactive manager

```
scripts/chain-policy.sh --config /etc/glc-bridge/config.toml
```

It draws the menus and asks the questions; every value it parses, converts
or writes is handled by `glc-admin`, behind the same types and the same
config parser the daemon itself uses. The network list is built from the
route registry, so a future chain appears in the menu with no edit to the
script.

**What it can never do**, because the commands beneath it cannot: enable a
route, read or write a secret, restart the daemon, or sign or send an
on-chain governance transaction.

The session is always: check the config path -> show current values ->
validate -> dry run -> type `APPLY` -> apply. Anything other than `APPLY`
aborts with nothing changed.

Set `GLC_ADMIN` if `glc-admin` is not on `PATH`.

### The path must be the FULL bridge config, and that is checked first

`--config` names the file the daemon itself loads — the one with
`[solana]`, `[goldcoin]`, `[reserve]`, `[operators]` and `[service]` in it,
typically `/etc/glc-bridge/config.toml`.

It is **not** `docs/robinhood/launch-policy.toml.example`. That file states
the approved policy for documentation and holds no config sections, so the
parser refuses it — and used to refuse it with

```
TOML parse error at line 1, column 1
missing field `solana`
```

which is literally true, says nothing about which file was wrong, and
repeated once per menu action.

The manager now runs `chain-policy-check-config` **before it draws a
menu**, and again for every path typed at the re-prompt, so an unusable
path is explained once, up front:

```
$ scripts/chain-policy.sh --config docs/robinhood/launch-policy.toml.example

NOT A CONFIG FILE — this is a POLICY FRAGMENT.
...
Path to full bridge config.toml:
```

For a fragment the check also prints, **read-only**, the policy that
fragment states — in operator units, with the fixed-bucket half — and the
exact `chain-policy-apply` flags that would put it into a real config file.
It never edits the fragment, and never treats it as a config.

The same classification backs `chain-policy-show`, `-validate` and
`-apply`: each names the kind of file it was handed instead of forwarding a
bare parser error.

| Answer | Meaning |
| --- | --- |
| `full-config` | `Config::load` accepts it. The only kind any command acts on. |
| `policy-fragment` | Valid TOML with a `[<chain>.policy]` section and none of the required config sections — a snippet, not a config. |
| `incomplete-config` | Valid TOML, required sections missing, no policy either. |
| `invalid-config` | Every required section present; the parser still refuses it, in its own words. |
| `not-toml` / `missing` / `unreadable` | What it says. |

`chain-policy-check-config` reads one file, writes nothing, contacts
nothing, and exits non-zero for anything but `full-config` — so a script
can branch on the status alone, or on the `kind` field of `--porcelain`.

### The commands underneath

```
glc-admin chain-policy-check-config --config PATH [--porcelain]
glc-admin chain-policy-networks [--json] [--porcelain]
glc-admin chain-policy-show --config PATH --network <solana|robinhood> [--json] [--porcelain] [--no-onchain]
glc-admin chain-policy-validate --config PATH --network NAME <values>
glc-admin chain-policy-apply --config PATH --network NAME <values> --note TEXT [--dry-run] [--execute]
```

Values may be given exactly or in the form an operator types:

| Exact | Human |
| --- | --- |
| `--fee-bps 600` | `--fee-percent 6` (also `3`, `1.5`) |
| `--per-transfer-limit 2000000000000` | `--per-transfer-glc 20000` |
| `--rolling-daily-limit 1000000000000000` | `--rolling-glc 10000000` |

Passing both spellings of one value is refused: two ways of saying one
thing is an ambiguity about money, not a convenience. Amounts are canonical
8-decimal atomic units (1 GLC = 100000000); percentages carry at most two
decimals, because one basis point is 0.01% and a finer rate cannot be
charged.

Refused: negative values, a zero transfer limit, a fee at or above 100%, a
rate outside 0..=9999 basis points, a rolling limit
below the per-transfer limit, overflow, an unsupported network, and
malformed input.

### `chain-policy-apply` is a dry run by default

Without `--execute` it prints the exact before/after values and writes
nothing. With `--execute` it:

1. renders the edit as a TOML **document**, so comments and every
   unrelated key survive — it is not a text substitution and not a
   re-serialisation;
2. writes a candidate file beside the target and **loads it with the real
   config parser**, refusing if it does not load or does not mean what was
   asked;
3. copies the original to `config.toml.bak.<UTC timestamp>`;
4. installs the already-validated candidate with one atomic `rename`.

The config is never partially written: a reader sees the whole old file or
the whole new one. `--note` is mandatory, as it is for every other
state-changing command here.

It does **not** restart the daemon. The running process keeps the previous
policy until an operator restarts it deliberately.

### Per-network differences, which are real

- **Robinhood** — configurable. `[robinhood.policy]` STATES the policy;
  `GlcRobinhoodBridge` ENFORCES it. `chain-policy-show` reads the deployed
  contract's `limits()` when `[robinhood.indexer]` permits and names every
  disagreement. An unavailable read is reported as unavailable, never as a
  pass.
- **Solana** — NOT configurable here, and the tool says so rather than
  offering a menu entry that does nothing. Its fee is the compiled-in
  its own `[fees]` entry (a config edit, no rebuild); its
  limits live in the Solana program's config account and are changed with
  `glc-admin set-limit` under the Solana admin authority. Nothing in this
  section can alter either.

### The rolling window is a fixed bucket — the on-chain number is HALF

`GlcRobinhoodBridge`'s 24-hour window resets wholesale rather than sliding.
A bucket filled at `t0` and refilled at exactly `t0 + 24h` lets **2x** the
configured amount move inside one 86,400-second span, and that worst case
is reachable. So the number configured on chain must be half the strict
policy:

```
Requested strict 24h policy:       10,000,000 GLC
Recommended on-chain bucket limit:  5,000,000 GLC
```

Every `chain-policy-show`, `chain-policy-validate` and `chain-policy-apply`
prints this relationship for Robinhood, in canonical and 18-decimal units.
Installing it is a `setLimits(...)` action under a 2-of-3 signer quorum;
**no command here signs, prepares or sends that transaction.** Run
`glc-admin robinhood-preflight --config PATH` afterwards to check the
backend policy against what the contract actually holds.

### The Robinhood launch session, end to end

```
$ scripts/chain-policy.sh --config /etc/glc-bridge/config.toml
Select network:            2   (Robinhood Network)
Action:                    4   (Change all)
New fee:                   6
New per-transfer limit:    20000
New 24h rolling limit:     10000000
Note:                      Robinhood mainnet launch policy
Confirm:                   APPLY
```

Result: `fee_bps = 600`, `per_transfer_limit = 2000000000000`,
`rolling_daily_limit = 1000000000000000`, a timestamped backup beside the
config, and a printed reminder that the on-chain bucket must be set to
5,000,000 GLC by governance before the policy is real.
