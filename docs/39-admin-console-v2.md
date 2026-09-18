# Admin console v2 — backend support (schema v38, admin API additions)

Added 2026-09-18. The bridge-side half of the Admin Console v2 rebuild
(the console itself lives in `glc-solana-reserve-bridge-admin-ui`, see its
`ADMIN-CONSOLE-V2-DESIGN.md`). This document is the canonical
cross-reference target ("docs/39-admin-console-v2.md") for everything in
this change.

Nothing here is deployed at the time of writing. Production runs schema
v37; the v38 migration runs the first time a binary built from this
change opens the ledger.

## 1. What the console needed that the daemon could not give it

| Need | Before | After |
|---|---|---|
| Stop ONE Goldcoin-sourced route (`GlcToSol` or `GlcToRhn`) | impossible without pausing its destination reserve, which also stops the route sharing that reserve | route-scoped admission gate on every route (schema v38) |
| Route admission over HTTP | CLI only (`route-admission-close/open`); the console spawned `glc-admin` through `sudo` | `POST /routes/{route}/admission/{close\|open}` on the admin API, same audited implementation |
| Robinhood LOCAL pause over HTTP | CLI only (`robinhood-local-pause`) | `POST /pause` / `POST /unpause` accept `direction: "robinhood"` (guarded unpause) |
| One authoritative per-route gate decomposition | assembled in the browser from `/status`, `/reserve-health`, `/chains`, `/robinhood/reserve` and a preflight run | `GET /routes` |
| Contract `routeEnabled` flags without a sudo spawn per refresh | `robinhood-preflight` only | read-only `RobinhoodAdminReader`, cached 10 s |
| Submitter balances | preflight only | `GET /submitters` |

## 2. Schema v38 — `route_admission` for every route

`route_admission.route_id`'s CHECK is widened from the four observed-deposit
routes (v27) to all six, using the same `widen_check_constraint_labelled`
table rebuild v27 used (DDL read from `sqlite_master`, every column copied
through, row count verified). Two rows are then seeded OPEN with
`INSERT OR IGNORE`:

```sql
INSERT OR IGNORE INTO route_admission (route_id, admission_closed, admission_closed_reason, updated_at)
VALUES ('GlcToSol', 0, NULL, strftime('%s','now')), ('GlcToRhn', 0, NULL, strftime('%s','now'));
```

`Route::is_admission_settable` is now `true` for every route and
`Route::ADMISSION_SETTABLE == Route::ALL`.

### Where the new gate is enforced

- **Observed-deposit routes** (`SolToGlc`, `RhnToGlc`, `SolToRhn`,
  `RhnToSol`): unchanged — the fold parks a newly observed deposit in
  `ManualReview` with `route_admission_closed_at_fold`.
- **Requested-deposit routes** (`GlcToSol`, `GlcToRhn`): their admission
  moment is `Ledger::create_request_from` (behind `POST /transfers`), where
  the destination capacity is reserved. It now reads
  `route_admission_closed_in` inside the same write transaction, after the
  reserve pause check and before the wallet windows, and returns
  `CreateRequestOutcome::RouteAdmissionClosed`. The public API maps that to
  the existing cause-agnostic 503 (`ApiError::Paused` copy). No row, no
  reservation, no deposit address is created. A request created before the
  gate closed keeps settling: already-accepted obligations are never
  affected by any admission flag, on any route. The late-deposit
  (`Expired` → re-reservation) path is untouched; it did not consult the
  pause either.
- **`GET /chains`**: unchanged code. `route_availability` already evaluates
  `InboundAdmissionGates` for every route, and the route gate is ranked
  first, so a closed `GlcToSol` reports `available: false,
  availability_reason: "route_admission_closed"`.

### Guards

Closing is always allowed. Opening runs `guard::open_route_admission_guarded`
— the same three reserve safety checks `open-admission` runs (invariant,
mature-UTXO floor, confirmed-liquidity buffer) against the route's
DESTINATION reserve. For the Solana and Robinhood reserves checks 2 and 3
are `Ok` by construction, as they are for every existing caller.

### Behaviour change: none on migration

Both seeded rows are OPEN and `Ledger::route_admission_closed` resolves an
absent row to OPEN, so a ledger before and after v38 admits exactly the
same requests and `GET /chains` publishes exactly the same verdicts.
Pinned by `upgrading_from_v37_keeps_a_closed_gate_closed_and_seeds_the_two_new_rows_open`
(an operator's closed gate on an existing route survives with its reason
and timestamp) and `a_fresh_database_seeds_route_admission_open_for_every_route`.

### Rollback

A v37 binary refuses a v38 ledger (`LedgerError::SchemaTooNew`) rather than
relabelling it. Rollback is therefore: stop the daemon, restore the ledger
snapshot the deploy step takes immediately before the first v38 start
(`/var/lib/glc-bridge/backups/ledger-pre-v38-<ts>.db`), reinstall the v37
binaries, start. Any route-admission change an operator made on
`GlcToSol`/`GlcToRhn` between the two is lost with the snapshot — those two
gates do not exist at v37 — and every other operator change made in that
window is lost too, exactly as with every earlier schema rollback
(docs/09-runbook.md "Schema rollback").

### Read-only verification after deploy

```
glc-admin route-admission-show --db /var/lib/glc-bridge/ledger.db --json
#   six rows, every admission_closed=false, GlcToSol and GlcToRhn present
curl -s 127.0.0.1:9101/chains | jq '.routes[] | {id, available, availability_reason}'
#   identical to the same command run before the deploy
curl -s -H "Authorization: Bearer $TOKEN" 127.0.0.1:9102/routes | jq '.routes[] | {route, available, blockers}'
```

`route-admission-show` opens the ledger through `Ledger::open`, which
RUNS MIGRATIONS — never point a v38 `glc-admin` at the live ledger before
the v38 daemon has been started (the same rule every earlier migration
carried).

## 3. Admin API additions

All mutations: bearer-authenticated, `note` mandatory, audited through the
existing `audited_*` helpers (success and refusal alike), one
`MutationReceipt` back. All reads: bearer-authenticated, read-only.

| Endpoint | Body | Backing logic |
|---|---|---|
| `POST /routes/{route}/admission/close` | `{note}` | `audited_set_route_admission(route, true)` — action `route_admission_close`, target the route |
| `POST /routes/{route}/admission/open` | `{note}` | `audited_set_route_admission(route, false)` — `guard::open_route_admission_guarded`; refusal is 409 and audited |
| `POST /pause`, `POST /unpause` | `{direction: goldcoin\|solana\|robinhood, note}` | `robinhood` → `audited_set_robinhood_local_pause` (unpause behind `guard::unpause_robinhood_reserve_guarded`); the two existing directions are unchanged |
| `GET /routes` | — | `RoutesAdminView` (below) |
| `GET /submitters` | — | Solana fee-payer pubkey + lamports (`get_account`), Robinhood submitter address + wei + `min_submitter_balance_wei` + `funded` |

`{route}` is parsed with `Route::from_str`; anything else is 404. An
unknown verb is 404. A missing or empty note is 400 with nothing written.

### `GET /routes` — the effective route state

One entry per route, in `Route::ALL` order. The verdict field
`available` is computed by the SAME function `GET /chains` uses
(`api::route_availability`, made `pub(crate)`), from the same
`InboundAdmissionGates` snapshot, the same Solana `BridgeConfig` read
(`SolanaProgramPause`), the same `SolToGlc` probe
(`api::sol_to_glc_probe_from`, extracted so both callers strike it from
the same config) and the same rate verdict (`BridgeRateVerdict::from_book`).
It is then ANDed with the Robinhood contract flags when they were read.
The console never re-derives availability; it renders this.

Per route:

- identity: `route`, `source_chain`, `destination_chain`,
  `destination_reserve`, `source_reserve`, `reserve_siblings` (every other
  route the destination reserve's local pause also stops), `implemented`
- enablement: `enabled`, `disabled_by` (`config`|`ledger`|`adapter`),
  `enablement_settable`
- the route's own gate: `route_admission_closed`, `route_admission_reason`,
  `route_admission_updated_at`
- destination reserve: `reserve_paused`, `reserve_pause_reason`,
  `reserve_admission_closed`, `reserve_admission_reason`,
  `liquidity_admission_closed`, `confirmed_headroom_atomic`,
  `admission_buffer_atomic`, `max_admissible_net_atomic`,
  `reserve_not_configured`
- on-chain layers: `onchain_blocked` (Solana program; `null` = no Solana
  leg), `contract_route_enabled`, `contract_paused` (`depositsPaused` for a
  Robinhood-sourced route, `payoutsPaused` for a Robinhood-bound one;
  `null` = not modelled or unread)
- rate and fee: `bridge_rate` (as `GET /chains`), `fee_bps`
- load: `pending_requests` (active states), `manual_review_count`,
  `last_audit` (newest audit row targeting the route or its destination
  reserve)
- verdict: `available`, `primary_reason` (the `GET /chains` reason, or the
  contract gate when that is what closed it), `blockers` (EVERY closed
  gate, most operator-actionable first: `route_disabled`,
  `route_admission_closed`, `reserve_admission_closed`, `reserve_paused`,
  `reserve_unavailable`, `onchain_paused`, `contract_route_disabled`,
  `contract_deposits_paused`/`contract_payouts_paused`, the rate reasons,
  `utxo_liquidity_low`, `liquidity_buffer_low`, `insufficient_capacity`,
  `probe_unavailable`), `warnings` (`contract_unread`,
  `contract_not_configured` — never a blocker)

Top level: `solana_program` (`paused`, `release_paused`, `deposit_paused`;
`null` when the config read failed — every Solana leg then fails closed
exactly as on `GET /chains`), `robinhood_contract` (`availability`,
`deposits_paused`, `payouts_paused`, `route_enabled[]`, `read_at`), `as_of`.

Invariant, pinned by `routes_view_lists_all_six_routes_with_consistent_verdicts`:
`available == blockers.is_empty()` for every route. Isolation, pinned by
`closing_one_routes_admission_touches_exactly_that_route` and
`pausing_a_reserve_blocks_exactly_its_routes`: closing one route's gate
changes that route's blockers by exactly `route_admission_closed` and no
other route's by anything; pausing a reserve adds `reserve_paused` to
exactly the routes in each other's `reserve_siblings`.

### The Robinhood reader

`admin_api::robinhood_read::RobinhoodAdminReader` — object-safe, read-only:
`contract_flags()` (`depositsPaused`, `payoutsPaused`, four `routeEnabled`;
six `eth_call`s, failing as a unit) and `submitter()` (`eth_getBalance`).
Wired by the daemon only when both `[robinhood.indexer]` and
`[robinhood.settlement]` exist, on its own `EvmRpcClient` (never shared
with a settlement path). The admin API caches the flags for
`CONTRACT_FLAGS_CACHE_SECS` (10 s) and reports `read_at`; a failed re-read
is reported as `unavailable` and adds a `contract_unread` WARNING, never a
blocker. Nothing here holds a key or can broadcast; the admin API's "one
fund-moving route" statement in `admin_api.rs` is unchanged.

## 4. What did NOT change

- The reserve-wide `close-admission`/`open-admission` (Goldcoin only,
  closes `SolToGlc` AND `RhnToGlc`) and its guards.
- Every unpause/open guard; every fold; `ManualReview` handling; the
  auto-resume switch (default FALSE, no new endpoint reads or writes it).
- The Solana on-chain pause (CLI + admin keypair) and Robinhood governance
  (2-of-3 quorum): the admin API still cannot touch either.
- `bridge_routes` enablement: still CLI-only (`robinhood-route-enable`).

## 5. Tests added

`routes::tests` (predicates re-pinned to "every route with a Direction");
`ledger::schema` (v38 fresh seed, v37→v38 upgrade keeping a closed gate,
CHECK literal pin, rewind test updated); `ledger::tests`
(`create_request_from` refused while `GlcToSol` is closed and reserving
nothing, unaffected while only `GlcToRhn` is closed, pause ranked before
the route gate, six rows on a fresh ledger); `api::tests`
(`GET /chains` reason and `POST /transfers` refusal for a closed
`GlcToSol`, five other routes unchanged); `admin_api::tests` (six-route
isolation matrix over admission and pause, refused open audited,
validation of route/verb/note, Robinhood local pause over HTTP with
guarded unpause, contract flags folding and the unread case, submitters
absence, every new GET path in the bearer-required sweep).
