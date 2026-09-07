# Local Development

## Toolchain

- Rust (host, pinned): 1.85.0 — see `rust-toolchain.toml`.
- Anchor CLI: 0.31.1
- Solana CLI / Agave: 2.1.21

Verified locally against this exact pairing (see `Anchor.toml`, `rust-toolchain.toml`).

## Git hooks

Run this once per clone (and once per `git worktree`, if the worktree was created
before you installed):

```
scripts/install-git-hooks.sh
```

It points `core.hooksPath` at the tracked `.githooks/` directory, so you always run
whatever the repo currently ships rather than a stale copy under `.git/hooks`. To
uninstall: `git config --local --unset core.hooksPath`.

### `commit-msg` — no AI authorship attribution

Rejects any commit message carrying AI/Claude authorship attribution:

| Pattern (case-insensitive) | Why |
| --- | --- |
| `Co-authored-by:.*Claude` | GitHub credits co-author trailers as real authors |
| `Co-authored-by:.*Anthropic` | same |
| `noreply@anthropic.com` | GitHub resolves this address to the account `claude` |
| `Claude-Session:` | AI session link, not project metadata |
| `generated-by:.*Claude` | AI attribution trailer |
| `AI-generated` | AI attribution marker |

This matters because GitHub treats `Co-Authored-By:` as first-class authorship and
maps `noreply@anthropic.com` to the GitHub account `claude` — a commit carrying one
puts an AI account in this repository's Contributors panel. Removing it afterwards
requires rewriting published history and force-pushing, which breaks every open PR.
Blocking it at commit time is far cheaper.

The hook is **reject-only**: it never edits your message. Ordinary human
co-authorship is unaffected — `Co-authored-by: Jane Doe <jane@example.com>` passes,
because every pattern above requires Claude/Anthropic specifically.

Two notes on scope:

- The hook only reads the message text that will actually be committed. Comment
  lines and the `git commit -v` diff below the scissors line are excluded, so you
  can freely commit files (this hook, its docs) that *mention* the patterns.
- `AI-generated` is matched anywhere in the message, so prose legitimately
  discussing AI-generated content will trip it. That is deliberate — false
  positives here are cheap and false negatives are expensive. Use
  `git commit --no-verify` for the rare genuine case.

A hook is a convenience, not an enforcement boundary: it is per-clone and
bypassable. Treat it as the thing that catches the accident, not as a guarantee.

## Building the program

```
anchor build
```

Produces `target/deploy/glc_reserve_bridge.so` and a dev-only deploy keypair at
`target/deploy/glc_reserve_bridge-keypair.json` (gitignored — never a production key).

## Running tests

Host-side unit tests (pure logic: `shared/`, `limits.rs`, `validation.rs`, `verification.rs`,
`state.rs` layout tests) and litesvm-based integration tests (full instruction behavior) both
run via `cargo test`, but **require the `nightly` toolchain** in this environment, not the
pinned 1.85.0 stable channel:

```
cargo +nightly test --workspace
```

Why: several of Anchor/Solana's transitive dependencies have since published releases requiring
a newer cargo/rustc (edition2024 manifests, raised MSRV) than 1.85.0 provides. `Cargo.lock`
precisely pins the affected crates to versions that predate those requirements — the same
resolved versions the reference bridge repository's own lockfile uses for this Anchor/Solana
pairing — so the *dependency graph itself* builds fine under 1.85.0; only the causally
unrelated compiler-version floor on the crates.io index forces a newer host rustc to even
resolve/download them. `nightly` was already installed in this environment and is used for
test execution only; it does not change `rust-toolchain.toml`, which still governs the
production SBF build via `anchor build`.

If a future environment ships rustc >= 1.88 as its default, `cargo test` will work without the
`+nightly` override — no code or lockfile change required.

See `IMPLEMENTATION_LOG.md` for the full decision record of this and other implementation-phase
choices.

## The off-chain service (`service/`)

A separate Cargo workspace (its own `service/Cargo.toml`, excluded from the root workspace —
see the root `Cargo.toml`'s `exclude` comment). Build and test from within `service/`:

```
cd service
cargo +nightly test
```

The same `+nightly` requirement applies here (see above) — this workspace's `Cargo.lock`
resolved cleanly without needing the precise-pin treatment the on-chain workspace's litesvm
dev-dependency required, since nothing SBF-adjacent is pulled in.

## What is intentionally not built yet

See `IMPLEMENTATION_LOG.md`'s Phase 2 and Phase 0/1 entries. Not yet implemented: timelocked
governance for limit/pause changes (currently admin-immediate), `rebalance_deposit`/
`rebalance_withdraw` instructions, attestation signing clients, the settlement orchestrator,
Goldcoin vault/payout construction, and operator tooling (CLI/health/metrics endpoints). Do not
assume any of these are bugs by omission — they are sequenced into later phases.
