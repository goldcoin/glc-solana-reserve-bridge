//! The **rapid-burst hold** — a deterministic, config-driven anti-abuse
//! rule evaluated at fold time (schema v30, 2026-09-13 policy).
//!
//! # What it is
//!
//! A deposit that has ALREADY landed on its source chain (a fold, or a
//! Goldcoin deposit observation) is compared with the bridge's own
//! recent history inside the same write transaction. If, within a short
//! rolling window, the bridge has already seen more requests than the
//! configured maximum for
//!
//! - the same SOURCE wallet (on the same source chain),
//! - the same DESTINATION wallet (on the same destination chain), or
//! - the same source/destination PAIR (necessarily the same route),
//!
//! the request is written as `ManualReview` with
//! `manual_review_note = rapid_burst_hold`,
//! `manual_review_disposition = rapid_burst_hold`, the hold columns set
//! (`hold_reason = rapid_burst:<rule>`, `held_by = system`,
//! `hold_started_at = now`, `review_after = now + minimum_review_hold_secs`)
//! — and, critically, the v29 `auto_resume_hold_note` marker, so every
//! pre-existing hold predicate treats it as held from the first tick.
//!
//! The funds are custodied exactly as any other park: the source deposit
//! is finalized, nothing is reserved on the destination side, nothing is
//! paid out.
//!
//! # What it is NOT
//!
//! - Not a global deposits-per-second rule. Each rule is scoped to an
//!   identity the bridge itself observed (the on-chain depositor, the
//!   recorded recipient); unrelated traffic is never counted against a
//!   stranger's burst.
//! - Not a refund timer. `review_after` is the EARLIEST moment an
//!   operator may normally decide the row's fate; its passing changes
//!   nothing. The row leaves `ManualReview` only through an explicit
//!   `process` or `refund` decision
//!   ([`Ledger::process_held_manual_review`],
//!   [`Ledger::record_operator_decision`]) — never by the automatic
//!   recovery pass, never by liquidity recovering, never by a route
//!   reopening, never by a daemon restart.
//! - Not a replacement for the rolling-24h wallet uniqueness windows
//!   (`ledger::wallet_window`), which remain in force, separately, on
//!   every route. The burst rule outranks them only in the sense that a
//!   deposit matching BOTH is classified by the burst rule (the stricter,
//!   non-self-clearing one) rather than parked as a self-clearing
//!   wallet-window park that would auto-resume 24 hours later.
//!
//! # Where the thresholds live
//!
//! `[rapid_burst]` in the daemon's config (`config::Config::rapid_burst`),
//! seeded into the single-row `rapid_burst_policy` table at startup
//! ([`Ledger::set_rapid_burst_policy`]) exactly like the liquidity
//! thresholds — so the fold, which runs inside the ledger's own
//! transaction, reads the same numbers `glc-admin rapid-burst-policy-show`
//! prints, and a ledger with no row (or `enabled = false`) applies no
//! rule at all. The rule is OFF by default: a deployment that never
//! configured it behaves exactly as before v30.
//!
//! # Which rows count
//!
//! Every `bridge_requests` row created inside `(now - window_secs, now]`
//! with the same identity, whatever its state — an attempt is an
//! attempt. The row being folded is not yet inserted at evaluation time
//! (folds), or is excluded explicitly (the Goldcoin observation path,
//! [`WalletWindowScope::ExcludingRequest`]), so `observed` below is
//! always "prior rows + this one".

use rusqlite::Connection;

use super::wallet_window::WalletWindowScope;
use super::{Direction, Ledger, LedgerError, ManualReviewDisposition, RequestState, WalletRole};

/// The effective rapid-burst policy — what `[rapid_burst]` configures and
/// what the `rapid_burst_policy` table holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RapidBurstPolicy {
    pub enabled: bool,
    /// The rolling window, in seconds.
    pub window_secs: i64,
    /// The maximum number of requests (INCLUDING the one being folded)
    /// the same source wallet may have inside the window before the
    /// next one is held.
    pub max_per_source_wallet: u32,
    /// Same, keyed on the destination wallet.
    pub max_per_destination_wallet: u32,
    /// Same, keyed on the exact source/destination pair.
    pub max_per_pair: u32,
    /// `review_after = hold_started_at + minimum_review_hold_secs`.
    pub minimum_review_hold_secs: i64,
}

/// Which scoped rule matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RapidBurstRule {
    SameSourceWallet,
    SameDestinationWallet,
    SameSourceDestinationPair,
}

impl RapidBurstRule {
    pub const ALL: [RapidBurstRule; 3] = [
        RapidBurstRule::SameSourceDestinationPair,
        RapidBurstRule::SameSourceWallet,
        RapidBurstRule::SameDestinationWallet,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RapidBurstRule::SameSourceWallet => "same_source_wallet",
            RapidBurstRule::SameDestinationWallet => "same_destination_wallet",
            RapidBurstRule::SameSourceDestinationPair => "same_source_destination_pair",
        }
    }

    /// The `bridge_requests.hold_reason` spelling.
    pub fn hold_reason(self) -> String {
        format!(
            "{}:{}",
            Ledger::RAPID_BURST_HOLD_REASON_PREFIX,
            self.as_str()
        )
    }
}

/// A matched burst: the rule, how many requests the identity now has in
/// the window (prior rows + this one), the limit it exceeded, and the
/// window — everything the audit note and the operator listing show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RapidBurstMatch {
    pub rule: RapidBurstRule,
    pub observed: u32,
    pub limit: u32,
    pub window_secs: i64,
}

impl RapidBurstMatch {
    /// The human-readable hold note written to `auto_resume_hold_note`.
    pub fn note(&self) -> String {
        format!(
            "rapid-burst hold: {} — {} requests within {}s (limit {})",
            self.rule.as_str(),
            self.observed,
            self.window_secs,
            self.limit
        )
    }
}

impl Ledger {
    /// `hold_reason` prefix of every rapid-burst hold.
    pub const RAPID_BURST_HOLD_REASON_PREFIX: &'static str = "rapid_burst";

    /// Seeds (or replaces) the single policy row from the daemon's config.
    /// Called once at startup, before the first tick — the same posture
    /// as `set_admission_liquidity_thresholds`.
    pub fn set_rapid_burst_policy(
        &mut self,
        policy: &RapidBurstPolicy,
        now: i64,
    ) -> Result<(), LedgerError> {
        if policy.window_secs <= 0
            || policy.max_per_source_wallet == 0
            || policy.max_per_destination_wallet == 0
            || policy.max_per_pair == 0
            || policy.minimum_review_hold_secs < 0
        {
            return Err(LedgerError::InvalidRapidBurstPolicy(format!("{policy:?}")));
        }
        self.conn.execute(
            "INSERT INTO rapid_burst_policy
                (id, enabled, window_secs, max_per_source_wallet, max_per_destination_wallet,
                 max_per_pair, minimum_review_hold_secs, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                enabled = excluded.enabled,
                window_secs = excluded.window_secs,
                max_per_source_wallet = excluded.max_per_source_wallet,
                max_per_destination_wallet = excluded.max_per_destination_wallet,
                max_per_pair = excluded.max_per_pair,
                minimum_review_hold_secs = excluded.minimum_review_hold_secs,
                updated_at = excluded.updated_at",
            rusqlite::params![
                policy.enabled as i64,
                policy.window_secs,
                policy.max_per_source_wallet as i64,
                policy.max_per_destination_wallet as i64,
                policy.max_per_pair as i64,
                policy.minimum_review_hold_secs,
                now,
            ],
        )?;
        Ok(())
    }

    /// The policy row, `None` when never seeded (rule off).
    pub fn rapid_burst_policy(&self) -> Result<Option<RapidBurstPolicy>, LedgerError> {
        Self::rapid_burst_policy_in(&self.conn)
    }

    pub(crate) fn rapid_burst_policy_in(
        conn: &Connection,
    ) -> Result<Option<RapidBurstPolicy>, LedgerError> {
        use rusqlite::OptionalExtension;
        let row = conn
            .query_row(
                "SELECT enabled, window_secs, max_per_source_wallet, max_per_destination_wallet,
                        max_per_pair, minimum_review_hold_secs
                   FROM rapid_burst_policy WHERE id = 1",
                [],
                |r| {
                    Ok(RapidBurstPolicy {
                        enabled: r.get::<_, i64>(0)? != 0,
                        window_secs: r.get(1)?,
                        max_per_source_wallet: r.get::<_, i64>(2)? as u32,
                        max_per_destination_wallet: r.get::<_, i64>(3)? as u32,
                        max_per_pair: r.get::<_, i64>(4)? as u32,
                        minimum_review_hold_secs: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// THE burst decision, evaluated inside the fold's own transaction
    /// BEFORE the request row is written (or, on the Goldcoin observation
    /// path, with the already-existing row excluded). `None` = no rule
    /// matched (or the rule is off); `Some` = hold this deposit.
    ///
    /// The pair rule is checked first — it is the most specific — then
    /// the source wallet, then the destination wallet, so a deposit that
    /// trips several is described by the most specific one.
    ///
    /// Pure over the ledger's own rows: it reads nothing but
    /// `bridge_requests` and the policy row, and writes nothing.
    pub(crate) fn rapid_burst_verdict_in(
        tx: &Connection,
        direction: Direction,
        source_wallet: Option<&[u8]>,
        recipient: Option<&[u8]>,
        now: i64,
        scope: WalletWindowScope,
    ) -> Result<Option<RapidBurstMatch>, LedgerError> {
        let Some(policy) = Self::rapid_burst_policy_in(tx)? else {
            return Ok(None);
        };
        if !policy.enabled {
            return Ok(None);
        }
        let source = source_wallet.filter(|w| !w.is_empty());
        let recipient = recipient.filter(|w| !w.is_empty());
        let since = now - policy.window_secs;
        let excluded: i64 = match scope {
            WalletWindowScope::ExcludingRequest(id) => id,
            _ => -1,
        };

        for rule in RapidBurstRule::ALL {
            let (prior, limit): (i64, u32) = match rule {
                RapidBurstRule::SameSourceDestinationPair => {
                    let (Some(s), Some(r)) = (source, recipient) else {
                        continue;
                    };
                    (
                        tx.query_row(
                            "SELECT COUNT(*) FROM bridge_requests
                              WHERE direction = ?1 AND source_wallet = ?2 AND recipient = ?3
                                AND created_at > ?4 AND created_at <= ?5 AND id <> ?6",
                            rusqlite::params![direction, s, r, since, now, excluded],
                            |row| row.get(0),
                        )?,
                        policy.max_per_pair,
                    )
                }
                RapidBurstRule::SameSourceWallet => {
                    let Some(s) = source else {
                        continue;
                    };
                    let directions = Self::wallet_window_directions_sql_in(
                        direction.source_chain(),
                        WalletRole::Source,
                    );
                    (
                        tx.query_row(
                            &format!(
                                "SELECT COUNT(*) FROM bridge_requests
                                  WHERE direction IN {directions} AND source_wallet = ?1
                                    AND created_at > ?2 AND created_at <= ?3 AND id <> ?4"
                            ),
                            rusqlite::params![s, since, now, excluded],
                            |row| row.get(0),
                        )?,
                        policy.max_per_source_wallet,
                    )
                }
                RapidBurstRule::SameDestinationWallet => {
                    let Some(r) = recipient else {
                        continue;
                    };
                    let directions = Self::wallet_window_directions_sql_in(
                        direction.destination_chain(),
                        WalletRole::Destination,
                    );
                    (
                        tx.query_row(
                            &format!(
                                "SELECT COUNT(*) FROM bridge_requests
                                  WHERE direction IN {directions} AND recipient = ?1
                                    AND created_at > ?2 AND created_at <= ?3 AND id <> ?4"
                            ),
                            rusqlite::params![r, since, now, excluded],
                            |row| row.get(0),
                        )?,
                        policy.max_per_destination_wallet,
                    )
                }
            };
            let observed = u32::try_from(prior).unwrap_or(u32::MAX).saturating_add(1);
            if observed > limit {
                return Ok(Some(RapidBurstMatch {
                    rule,
                    observed,
                    limit,
                    window_secs: policy.window_secs,
                }));
            }
        }
        Ok(None)
    }

    /// Marks an already-inserted `ManualReview` row as a rapid-burst
    /// hold, in the same transaction as the fold that inserted it. Sets
    /// every hold column AND the v29 marker, and writes the state-log
    /// row (`rapid_burst_hold`, actor `system`) so the Explorer and the
    /// audit log show the hold the moment it exists.
    pub(crate) fn mark_rapid_burst_hold_in(
        tx: &Connection,
        request_id: i64,
        matched: &RapidBurstMatch,
        now: i64,
    ) -> Result<(), LedgerError> {
        let policy = Self::rapid_burst_policy_in(tx)?.ok_or_else(|| {
            LedgerError::InvalidRapidBurstPolicy("policy row vanished mid-fold".to_string())
        })?;
        let review_after = now + policy.minimum_review_hold_secs;
        tx.execute(
            "UPDATE bridge_requests
                SET manual_review_disposition = ?1, hold_reason = ?2, held_by = 'system',
                    hold_started_at = ?3, review_after = ?4,
                    auto_resume_hold_note = ?5, auto_resume_hold_until = ?4
              WHERE id = ?6 AND state = ?7",
            rusqlite::params![
                ManualReviewDisposition::RapidBurstHold,
                matched.rule.hold_reason(),
                now,
                review_after,
                matched.note(),
                request_id,
                RequestState::ManualReview,
            ],
        )?;
        super::log_transition(
            tx,
            request_id,
            Some(RequestState::ManualReview),
            RequestState::ManualReview,
            now,
            Some(Self::RAPID_BURST_HOLD_TRANSITION_REASON),
            "system",
        )?;
        Ok(())
    }

    /// Every request currently under a rapid-burst hold, oldest first.
    pub fn rapid_burst_held_requests(&self) -> Result<Vec<super::BridgeRequest>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "{} WHERE manual_review_disposition = 'rapid_burst_hold' ORDER BY id",
            super::SELECT_REQUEST_PREFIX
        ))?;
        let rows = stmt
            .query_map([], super::row_to_request)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
