# Admin control plane (admin API + admin UI)

Added 2026-08-29. The authenticated HTTP surface (`service/src/admin_api.rs`)
that lets authorized operators run the LOCAL subset of `glc-admin`'s
operations from an admin UI instead of SSH, plus read-only dashboards.
This document is the reference for its boundary, auth model, and endpoint
set. The separate admin UI application consumes this API through its own
server-side proxy; the public bridge UI never talks to it.

## The trust posture, in one paragraph

The admin API holds **no keys**: not the on-chain admin keypair, not the
deployer keypair, not any signer. It never touches `crate::signing`, has
no shell/command execution path, and cannot submit a transaction on
either chain. Everything it can mutate goes through the same validated
`Ledger` methods `glc-admin` uses — including every internal safety check
(the admission-open invariant + UTXO-liquidity gates via the shared
`admin_api::guard::open_admission_guarded`, and
`Ledger::resume_manual_review_sol_to_glc`'s unconditional recipient/
source-wallet rate-limit re-checks, which no admin surface can bypass).
On-chain admin actions remain CLI-only on the operator's own machine; for
those the API serves read-only state plus a generated `glc-admin` command
line to review and run ("CLI approval required").

## Boundary vs. the public API

`service/src/api.rs`'s public listener still never exposes privileged
operations — that boundary is unchanged. The admin listener is separate:

- Only starts when `service.admin_bind_addr` is configured. Bind it
  privately (localhost / internal interface). No TLS of its own — put the
  operators' reverse proxy in front (which in production also carries the
  external Basic Auth layer on the admin UI's origin; that proxy
  configuration is deployment concern, out of this repository's scope).
- Every request — reads included — requires `Authorization: Bearer` with
  a per-operator token.

## Authentication

- Config lists operators as `{ name, token_env }`
  (`service.admin_operators`); the token lives ONLY in the named
  environment variable — the same "config names the env var, never the
  secret" discipline as the remote signers' `auth_token_env`. The token
  type (`admin_api::auth::AdminAuthToken`) redacts itself from all
  `Debug` output.
- Verification compares SHA-256 digests in constant time; the matched
  operator name becomes the `actor` on every audit row.
- **Bearer-only, structurally CSRF-immune**: the API never sets or reads
  cookies and never answers CORS preflight, so browser-ambient
  credentials can never authorize anything. Requests carrying a `Cookie`
  or `Origin` header are rejected with 403 outright. The admin UI's
  browser session (cookie + Origin allowlist + custom header) lives in
  the admin UI application's own server, which attaches the bearer token
  server-side.

## Audit

Every mutation attempt — success or refusal — appends one row to the
append-only `admin_audit_log` (schema v15, docs/06-schema.md): actor,
action, target, old value, new value, mandatory non-empty note, outcome,
error text on refusal. The schema itself `CHECK`s the note non-empty, so
a noteless mutation is unrepresentable. `GET /audit-log` reads it
newest-first with keyset pagination and actor/action filters.

## Endpoints

Read-only (still authenticated):

| Endpoint | Serves |
|---|---|
| `GET /whoami` | the operator the token resolved to |
| `GET /status` | per-direction local pause/admission flags + reasons, ManualReview counts, post-finality reorg count |
| `GET /reserve-health` | `ops::reserve_health::check` for both directions (balance, protected minimum, reserved liquidity, pending obligations, accrued fees, mature/immature UTXO pool, invariant) |
| `GET /onchain` | decoded live `BridgeConfig` (pause flags, limits) + both rolling-volume windows with remaining capacity, mirroring the on-chain bucket arithmetic |
| `GET /fee` | the fixed `BRIDGE_FEE_BPS` with provenance "Compile-time setting — requires code deployment to change" — deliberately no mutation route exists (docs/20-bridge-fee.md's staged fee-change process) |
| `GET /manual-review` | the ManualReview backlog with reasons and, for SolToGlc, live recipient/source-wallet rate-limit `*_until` context |
| `GET /rebalances`, `GET /rebalances/{id}` | `rebalance::assess` per direction + the request workflow rows |
| `GET /audit-log` | the admin audit trail |
| `GET /routes` | every route's effective state — enablement, its own admission gate, the destination reserve's gates, the Solana program and Robinhood contract flags, rate, counts, blockers (docs/39-admin-console-v2.md) |
| `GET /submitters` | the Solana and Robinhood fee-payers' addresses and live balances |

UI-executable mutations (each: mandatory note, audited, existing Ledger
logic only):

| Endpoint | Backing logic |
|---|---|
| `POST /pause`, `POST /unpause` | `Ledger::set_paused` (local reserve direction: `goldcoin`, `solana`, or — since docs/39 — `robinhood`, whose unpause runs `guard::unpause_robinhood_reserve_guarded`) — see "What local pause does and does not stop" below |
| `POST /routes/{route}/admission/close\|open` | `audited_set_route_admission` — the route's OWN admission gate, every route since schema v38; open runs the same three reserve safety checks as `/admission/open` |
| `POST /admission/close` | `Ledger::set_admission` (goldcoin direction only, as in the CLI) |
| `POST /admission/open` | `guard::open_admission_guarded` — invariant + UTXO-liquidity gates, shared verbatim with `glc-admin open-admission` |
| `POST /manual-review/{id}/resume` | `Ledger::resume_manual_review_sol_to_glc`, called as-is |
| `POST /rebalances` + `/{id}/approve\|reject\|cancel\|record-executed\|confirm\|fail` | the rebalance `Ledger` methods; `record-executed` records an out-of-band tx reference string only |

CLI-approval-required helper:

| Endpoint | Serves |
|---|---|
| `POST /cli-command` | the exact `glc-admin onchain-pause`/`onchain-unpause`/`set-limit`/`reset-rolling-window` command line, with GLC→mint-atomic conversion done server-side at the mint's LIVE decimals and an old→new preview decoded from the live `BridgeConfig`. When a command has an unmet on-chain precondition right now (reset-rolling-window requires `BridgeConfig.paused == true`), the response's `precondition` field says so up front. RPC URL and keypair path are placeholders by design. Nothing ever executes the string. |

## What local pause does and does not stop

The LOCAL pause (`POST /pause`, `glc-admin pause`) stops this service
from ADMITTING and STARTING new work for the direction: new SolToGlc
folds park in ManualReview, auto-resume holds off, and quota enforcement
uses it. It does NOT halt settlements already past `SourceFinalized` —
the orchestrator's payout/release ticks deliberately finish work the
bridge already accepted and reserved capacity for, and that pre-existing
settlement semantic is unchanged by the admin control plane.

**To actually stop money movement** (releases and payouts included), use
the ON-CHAIN global pause — `glc-admin onchain-pause --scope global` (CLI
approval required): `release_from_reserve` and
`record_goldcoin_completion` both refuse while `BridgeConfig.paused` is
set, which halts outflows at the protocol level regardless of what this
service does. The admin console labels the local pause accordingly and
points at the on-chain pause for the full-stop case; treating the local
pause as an emergency stop-everything control is the operator error this
section exists to prevent.

Deliberately absent: `retry-goldcoin-payout` and `split-vault-utxo`
(they sign and broadcast — CLI-only, with `--config`), every `custody-*`
operation (operator-only CLI workflow; a future read-only view is a
reasonable candidate), and any on-chain mutation.

## Operator token issuance / rotation

Generate a long random token per operator (e.g. `openssl rand -hex 32`),
export it in the daemon's environment under the name the config declares,
and hand it to that operator through your secrets channel. Rotate by
changing the env var and restarting the daemon; revoke by removing the
operator entry. Tokens are per-person so the audit log's `actor` stays
meaningful — never share one token between operators.
