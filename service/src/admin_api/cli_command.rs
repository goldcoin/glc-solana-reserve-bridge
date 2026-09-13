//! Server-side generation of the exact `glc-admin` command an authorized
//! operator should run over SSH for the on-chain admin actions this API
//! deliberately cannot execute (the admin keypair is CLI-only — see the
//! module docs of [`crate::admin_api`]).
//!
//! Why server-side rather than in the admin UI: the 6-decimal
//! mint-atomic / 8-decimal Goldcoin-atomic conversions and the live
//! "current value" reads already exist in this crate, in the exact code
//! the daemon itself trusts (`amount_conversion`, `solana::accounts`) —
//! duplicating that arithmetic in TypeScript is precisely the
//! unit-confusion bug class this bridge's typed-unit discipline exists
//! to prevent, and an old→new preview is only trustworthy if it decodes
//! the same account bytes the daemon decodes.
//!
//! Everything here is a PURE function over already-fetched state: no RPC,
//! no ledger, no execution. The output is a string for a human to review
//! and run; nothing in this crate ever executes it. RPC URL and keypair
//! path are deliberately placeholders — the RPC URL this daemon uses may
//! embed provider credentials, and the keypair path exists only on the
//! operator's own machine.

use serde::{Deserialize, Serialize};

use super::OnchainView;

/// Placeholder the operator substitutes with their own RPC endpoint.
pub const RPC_URL_PLACEHOLDER: &str = "<RPC_URL>";
/// Placeholder for the operator-held admin keypair path — this service
/// never knows a real value for it, by design.
pub const KEYPAIR_PLACEHOLDER: &str = "<ADMIN_KEYPAIR_PATH>";
/// Placeholder for the mandatory operator note.
pub const NOTE_PLACEHOLDER: &str = "<NOTE>";

/// Deployment path of the daemon's own config file, which
/// `glc-admin refund-manual-review` takes instead of `--rpc-url`
/// (it needs the ledger path, RPC endpoint and signer endpoints
/// together). A filesystem PATH is not credential material — no key
/// bytes, no token, and no RPC credential is rendered here; the file it
/// names is readable only on the operator's own host.
pub const REFUND_CONFIG_PATH: &str = "/etc/glc-bridge/config.toml";
/// Deployment path of the operator-held admin keypair. Again a path,
/// never contents: this service neither reads nor transmits the file.
pub const REFUND_ADMIN_KEYPAIR_PATH: &str = "/etc/glc-bridge/keys/deployer.json";

/// Every action here requires the on-chain admin keypair, which stays on
/// the operator's machine — the UI renders this label on each of them.
pub const CLI_APPROVAL_REQUIRED: &str = "CLI approval required";

#[derive(Debug, Deserialize)]
pub struct CliCommandInput {
    /// `onchain-pause` | `onchain-unpause` | `set-limit` |
    /// `reset-rolling-window` — exactly the `glc-admin` subcommand names.
    pub action: String,
    /// For `onchain-pause`/`onchain-unpause`: `global`|`release`|`deposit`.
    pub scope: Option<String>,
    /// For `set-limit`: `min-transfer`|`per-transfer`|`protected-minimum`|
    /// `rolling-volume`.
    pub field: Option<String>,
    /// For `set-limit`: the new value as a decimal GLC string (e.g.
    /// `"20000"` or `"20000.5"`), converted server-side to the reserve
    /// mint's atomic units at its LIVE decimals.
    pub value_glc: Option<String>,
    /// For `reset-rolling-window`: `glc-to-sol`|`sol-to-glc`.
    pub direction: Option<String>,
    /// For `refund-manual-review`: which parked request to refund. The
    /// ONLY refund parameter a caller may supply — the destination and
    /// the amount are derived by the CLI itself from the verified
    /// on-chain deposit and can never be passed in from here.
    pub request_id: Option<i64>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ValueDisplay {
    /// Mint-atomic units (the mint's live decimals) — the unit
    /// `set_limit` takes on-chain.
    pub atomic: u64,
    /// The same value in whole GLC, fixed-point decimal string.
    pub display_glc: String,
}

#[derive(Debug, Serialize)]
pub struct CliCommandView {
    /// The full command line for the operator to review and run. Never
    /// executed by anything in this service.
    pub command: String,
    pub old_value: Option<ValueDisplay>,
    pub new_value: Option<ValueDisplay>,
    /// Human summary when old/new aren't a single number (pauses, window
    /// resets).
    pub summary: String,
    /// A step the operator must complete BEFORE the command can succeed
    /// on-chain, when one applies right now (e.g. reset-rolling-window
    /// requires the global pause first). `None` when the command is
    /// runnable as-is.
    pub precondition: Option<String>,
    pub unit: String,
    pub label: &'static str,
}

/// Formats `atomic` with `decimals` fractional digits as a fixed-point
/// decimal string — pure integer arithmetic, trailing fractional zeros
/// trimmed (but never the integer part).
///
/// Precondition: `decimals <= 19` (the largest u64 power of ten) —
/// `generate()` bounds every chain-fed decimals value before any call
/// here, and the fee display passes a constant 2. Asserted so a future
/// unguarded caller fails loudly in tests, never wraps silently.
pub fn format_atomic_as_decimal_string(atomic: u64, decimals: u8) -> String {
    debug_assert!(decimals <= 19, "callers must bound decimals first");
    let scale = 10u64.pow(u32::from(decimals.min(19)));
    let whole = atomic / scale;
    let frac = atomic % scale;
    if frac == 0 {
        return whole.to_string();
    }
    let frac_str = format!("{frac:0width$}", width = decimals as usize);
    let trimmed = frac_str.trim_end_matches('0');
    format!("{whole}.{trimmed}")
}

/// Parses a decimal GLC string into mint-atomic units at the mint's
/// LIVE `decimals` (never a compile-time assumption — the
/// `amount_conversion` module's own rule) — pure integer/string
/// arithmetic, no floating point (a float parse of `"20000.000001"` is
/// exactly the rounding bug this exists to avoid). Rejects: empty, sign
/// characters, more fractional digits than the mint can represent,
/// non-digits, and values that overflow u64.
pub fn parse_glc_to_mint_atomic(value: &str, decimals: u8) -> Result<u64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("value_glc must not be empty".to_string());
    }
    let mut parts = value.splitn(2, '.');
    let whole_str = parts.next().unwrap_or("");
    let frac_str = parts.next().unwrap_or("");
    if whole_str.is_empty() && frac_str.is_empty() {
        return Err("value_glc must be a decimal number".to_string());
    }
    if !whole_str.chars().all(|c| c.is_ascii_digit())
        || !frac_str.chars().all(|c| c.is_ascii_digit())
    {
        return Err(format!(
            "value_glc must be an unsigned decimal number, got {value:?}"
        ));
    }
    if frac_str.len() > decimals as usize {
        return Err(format!(
            "value_glc has more than {decimals} fractional digits — the Solana mint cannot represent it"
        ));
    }
    // Checked, like amount_conversion's own arithmetic: a decimals value
    // with no u64 power of ten must error, never wrap (overflow-checks
    // are off in release, and the mint byte is chain-fed).
    let scale = 10u64
        .checked_pow(u32::from(decimals))
        .ok_or_else(|| format!("mint decimals {decimals} out of convertible range (max 19)"))?;
    let whole: u64 = if whole_str.is_empty() {
        0
    } else {
        whole_str
            .parse()
            .map_err(|_| format!("value_glc integer part out of range: {whole_str:?}"))?
    };
    let mut frac: u64 = if frac_str.is_empty() {
        0
    } else {
        frac_str
            .parse()
            .map_err(|_| format!("value_glc fractional part out of range: {frac_str:?}"))?
    };
    frac *= 10u64.pow(u32::from(decimals) - frac_str.len() as u32);
    whole
        .checked_mul(scale)
        .and_then(|w| w.checked_add(frac))
        .ok_or_else(|| format!("value_glc out of range: {value:?}"))
}

fn value_display(atomic: u64, decimals: u8) -> ValueDisplay {
    ValueDisplay {
        atomic,
        display_glc: format_atomic_as_decimal_string(atomic, decimals),
    }
}

/// The `glc-admin` subcommand names this module can emit — kept in one
/// place so the drift-guard test below can assert every one of them
/// exists in `glc-admin`'s real dispatch table.
pub const GENERATED_SUBCOMMANDS: [&str; 6] = [
    "onchain-pause",
    "onchain-unpause",
    "set-limit",
    "reset-rolling-window",
    "refund-manual-review",
    "manual-review-refund",
];

pub fn generate(input: &CliCommandInput, onchain: &OnchainView) -> Result<CliCommandView, String> {
    // The mint's LIVE decimals, read from chain by the caller
    // (`AdminApi::fetch_onchain`) — the value-bearing actions refuse to
    // generate anything rather than assume a decimal count.
    let decimals = || {
        let d = onchain.reserve_mint_decimals.ok_or_else(|| {
            "reserve vault is not configured yet — live mint decimals unavailable".to_string()
        })?;
        // The mint byte is chain-fed and unvalidated on-chain (SPL
        // accepts any u8); 10^d must fit in u64 (d <= 19) or every
        // conversion below would wrap in release. Refuse loudly — a mint
        // claiming 20+ decimals is misconfigured, not a unit to convert
        // into.
        if d > 19 {
            return Err(format!(
                "reserve mint claims {d} decimals — out of the convertible range (max 19);                  refusing to generate a value-bearing command against it"
            ));
        }
        Ok(d)
    };
    match input.action.as_str() {
        "onchain-pause" | "onchain-unpause" => {
            let pausing = input.action == "onchain-pause";
            let scope = input
                .scope
                .as_deref()
                .ok_or_else(|| "scope is required for onchain-pause/onchain-unpause".to_string())?;
            let current = match scope {
                "global" => onchain.paused,
                "release" => onchain.release_paused,
                "deposit" => onchain.deposit_paused,
                other => {
                    return Err(format!(
                        "unknown scope {other:?} (expected global|release|deposit)"
                    ))
                }
            };
            Ok(CliCommandView {
                command: format!(
                    "glc-admin {} --rpc-url {RPC_URL_PLACEHOLDER} --keypair {KEYPAIR_PLACEHOLDER} --scope {scope} --note '{NOTE_PLACEHOLDER}'",
                    input.action
                ),
                old_value: None,
                new_value: None,
                summary: format!("{scope} pause: currently {current}, would become {pausing}"),
                precondition: None,
                unit: "flag".to_string(),
                label: CLI_APPROVAL_REQUIRED,
            })
        }
        "set-limit" => {
            let field = input
                .field
                .as_deref()
                .ok_or_else(|| "field is required for set-limit".to_string())?;
            let old_atomic = match field {
                "min-transfer" => onchain.min_transfer_amount,
                "per-transfer" => onchain.per_transfer_limit,
                "protected-minimum" => onchain.protected_minimum,
                "rolling-volume" => onchain.rolling_volume_limit,
                other => {
                    return Err(format!(
                        "unknown field {other:?} (expected min-transfer|per-transfer|protected-minimum|rolling-volume)"
                    ))
                }
            };
            let value_glc = input
                .value_glc
                .as_deref()
                .ok_or_else(|| "value_glc is required for set-limit".to_string())?;
            let mint_decimals = decimals()?;
            let new_atomic = parse_glc_to_mint_atomic(value_glc, mint_decimals)?;
            // Mirror the two on-chain validations (`set_limit` rejects a
            // zero per-transfer or rolling-volume limit) so the operator
            // is told before ever building the transaction.
            if new_atomic == 0 && matches!(field, "per-transfer" | "rolling-volume") {
                return Err(format!("{field} cannot be set to 0 (on-chain ZeroAmount check)"));
            }
            let old = value_display(old_atomic, mint_decimals);
            let new = value_display(new_atomic, mint_decimals);
            let summary = format!(
                "{field}: {} GLC ({} atomic) -> {} GLC ({} atomic)",
                old.display_glc, old.atomic, new.display_glc, new.atomic
            );
            Ok(CliCommandView {
                command: format!(
                    "glc-admin set-limit --rpc-url {RPC_URL_PLACEHOLDER} --keypair {KEYPAIR_PLACEHOLDER} --field {field} --value {new_atomic} --note '{NOTE_PLACEHOLDER}'"
                ),
                old_value: Some(old),
                new_value: Some(new),
                summary,
                precondition: None,
                unit: format!("{mint_decimals}-decimal Solana mint atomic units"),
                label: CLI_APPROVAL_REQUIRED,
            })
        }
        "reset-rolling-window" => {
            let direction = input
                .direction
                .as_deref()
                .ok_or_else(|| "direction is required for reset-rolling-window".to_string())?;
            if !matches!(direction, "glc-to-sol" | "sol-to-glc") {
                return Err(format!(
                    "unknown direction {direction:?} (expected glc-to-sol|sol-to-glc)"
                ));
            }
            let window = onchain
                .rolling_windows
                .iter()
                .find(|w| w.window == direction)
                .ok_or_else(|| "rolling window state unavailable".to_string())?;
            let mint_decimals = decimals()?;
            // The on-chain instruction requires `BridgeConfig.paused ==
            // true` (see `glc-admin reset-rolling-window`'s docs) — when
            // the bridge is NOT currently paused, say so up front rather
            // than handing the operator a command that will fail
            // on-chain after they've reviewed the preview.
            let precondition = if onchain.paused {
                None
            } else {
                Some(concat!(
                    "the on-chain instruction requires BridgeConfig.paused == true, ",
                    "and the bridge is NOT currently paused — run ",
                    "`glc-admin onchain-pause --scope global` first, ",
                    "then this command, then unpause"
                )
                .to_string())
            };
            Ok(CliCommandView {
                command: format!(
                    "glc-admin reset-rolling-window --rpc-url {RPC_URL_PLACEHOLDER} --keypair {KEYPAIR_PLACEHOLDER} --direction {direction} --note '{NOTE_PLACEHOLDER}'"
                ),
                old_value: Some(value_display(window.remaining, mint_decimals)),
                new_value: Some(value_display(onchain.rolling_volume_limit, mint_decimals)),
                summary: format!(
                    "{direction} window: {} GLC remaining of {} GLC -> reset to full capacity",
                    format_atomic_as_decimal_string(window.remaining, mint_decimals),
                    format_atomic_as_decimal_string(onchain.rolling_volume_limit, mint_decimals),
                ),
                precondition,
                unit: format!("{mint_decimals}-decimal Solana mint atomic units"),
                label: CLI_APPROVAL_REQUIRED,
            })
        }
        "refund-manual-review" => {
            let request_id = input
                .request_id
                .ok_or_else(|| "request_id is required for refund-manual-review".to_string())?;
            if request_id <= 0 {
                return Err("request_id must be a positive request id".to_string());
            }
            // The command carries NO destination and NO amount: both are
            // derived by `glc-admin refund-manual-review` itself from the
            // verified on-chain `WithdrawalObligation`, exactly as the
            // dry run showed. There is deliberately no `--destination`
            // flag anywhere in that command's interface.
            let command = format!(
                "glc-admin refund-manual-review --config {REFUND_CONFIG_PATH} \
                 --request-id {request_id} --note '{NOTE_PLACEHOLDER}' \
                 --keypair {REFUND_ADMIN_KEYPAIR_PATH} --execute"
            );
            // The on-chain global pause is a hard precondition of the
            // refund instruction itself. Report whether it is satisfied
            // RIGHT NOW from the same live `BridgeConfig` read every
            // other view uses; the CLI re-checks it again immediately
            // before simulating, and refuses if it has lapsed.
            let precondition = if onchain.paused {
                None
            } else {
                Some(
                    "the bridge is not globally paused on-chain — run `glc-admin onchain-pause \
                     --scope global` first, and unpause explicitly once the refund is confirmed"
                        .to_string(),
                )
            };
            Ok(CliCommandView {
                command,
                old_value: None,
                new_value: None,
                summary: format!(
                    "Refund request #{request_id} to its original depositor. The destination \
                     (the depositor's canonical Token-2022 ATA) and the exact gross amount are \
                     derived by the CLI from the verified on-chain deposit — neither can be \
                     supplied by this console. Re-runs every safety check against fresh state, \
                     simulates first, and confirms at finalized commitment before the request \
                     becomes Refunded."
                ),
                precondition,
                unit: "no value is supplied by this console".to_string(),
                label: CLI_APPROVAL_REQUIRED,
            })
        }
        "manual-review-refund" => {
            let request_id = input
                .request_id
                .ok_or_else(|| "request_id is required for manual-review-refund".to_string())?;
            if request_id <= 0 {
                return Err("request_id must be a positive request id".to_string());
            }
            // The REFUND decision on a HELD request (schema v30) and the
            // refund itself, in one command: it records the audited
            // decision, then runs the request's own route's existing
            // refund command with these same arguments. Same posture as
            // `refund-manual-review` above — no destination, no amount,
            // no override flag is generated here. `--emergency` (the
            // before-review_after exit for a rapid-burst hold) is
            // deliberately NOT emitted: an operator types that word
            // themselves, or not at all.
            let command = format!(
                "glc-admin manual-review-refund --config {REFUND_CONFIG_PATH} \
                 --request-id {request_id} --note '{NOTE_PLACEHOLDER}' \
                 --keypair {REFUND_ADMIN_KEYPAIR_PATH} --execute"
            );
            let precondition = if onchain.paused {
                None
            } else {
                Some(
                    "for a Solana-sourced request the bridge must be globally paused on-chain \
                     first (`glc-admin onchain-pause --scope global`); Robinhood- and \
                     Goldcoin-sourced refunds have their own preconditions, re-checked by the \
                     CLI"
                        .to_string(),
                )
            };
            Ok(CliCommandView {
                command,
                old_value: None,
                new_value: None,
                summary: format!(
                    "Record the operator REFUND decision on held request #{request_id} and \
                     refund it to its original depositor through the route's existing refund \
                     tooling. Nothing is decided by this console: the CLI records the decision \
                     only on --execute, refuses a rapid-burst hold before its review_after \
                     unless the operator adds --emergency by hand, and re-runs every refund \
                     safety check against fresh state."
                ),
                precondition,
                unit: "no value is supplied by this console".to_string(),
                label: CLI_APPROVAL_REQUIRED,
            })
        }
        other => Err(format!(
            "unknown action {other:?} (expected onchain-pause|onchain-unpause|set-limit|reset-rolling-window|refund-manual-review|manual-review-refund)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_api::RollingWindowView;

    fn onchain() -> OnchainView {
        OnchainView {
            paused: false,
            release_paused: true,
            deposit_paused: false,
            min_transfer_amount: 100_000_000,
            per_transfer_limit: 10_000_000_000,
            protected_minimum: 20_000_000_000,
            rolling_volume_limit: 100_000_000_000,
            rolling_window_seconds: 86_400,
            obligation_count: 7,
            reserve_mint_decimals: Some(6),
            rolling_windows: vec![
                RollingWindowView {
                    window: "glc-to-sol".to_string(),
                    window_start: 0,
                    window_total: 25_000_000_000,
                    remaining: 75_000_000_000,
                },
                RollingWindowView {
                    window: "sol-to-glc".to_string(),
                    window_start: 0,
                    window_total: 0,
                    remaining: 100_000_000_000,
                },
            ],
        }
    }

    #[test]
    fn twenty_thousand_glc_converts_to_the_exact_6dp_atomic_value() {
        assert_eq!(parse_glc_to_mint_atomic("20000", 6), Ok(20_000_000_000));
        assert_eq!(
            parse_glc_to_mint_atomic("20000.000000", 6),
            Ok(20_000_000_000)
        );
        assert_eq!(parse_glc_to_mint_atomic("0.000001", 6), Ok(1));
        assert_eq!(parse_glc_to_mint_atomic("100", 6), Ok(100_000_000));
        assert_eq!(parse_glc_to_mint_atomic("20000.5", 6), Ok(20_000_500_000));
    }

    #[test]
    fn conversion_fails_closed_on_malformed_input() {
        assert!(parse_glc_to_mint_atomic("", 6).is_err());
        assert!(parse_glc_to_mint_atomic("-5", 6).is_err());
        assert!(parse_glc_to_mint_atomic("+5", 6).is_err());
        assert!(parse_glc_to_mint_atomic("1e6", 6).is_err());
        assert!(parse_glc_to_mint_atomic("0.0000001", 6).is_err(), "7 dp");
        assert!(parse_glc_to_mint_atomic(".", 6).is_err());
        assert!(
            parse_glc_to_mint_atomic("18446744073709551616", 6).is_err(),
            "overflow"
        );
        assert!(parse_glc_to_mint_atomic("20 000", 6).is_err());
    }

    #[test]
    fn formatting_round_trips_and_never_uses_floats() {
        assert_eq!(format_atomic_as_decimal_string(20_000_000_000, 6), "20000");
        assert_eq!(
            format_atomic_as_decimal_string(20_000_500_000, 6),
            "20000.5"
        );
        assert_eq!(format_atomic_as_decimal_string(1, 6), "0.000001");
        assert_eq!(format_atomic_as_decimal_string(0, 6), "0");
        assert_eq!(format_atomic_as_decimal_string(300, 2), "3");
    }

    #[test]
    fn set_limit_command_carries_the_atomic_value_and_old_new_preview() {
        let view = generate(
            &CliCommandInput {
                action: "set-limit".to_string(),
                scope: None,
                field: Some("per-transfer".to_string()),
                value_glc: Some("20000".to_string()),
                direction: None,
                request_id: None,
            },
            &onchain(),
        )
        .unwrap();
        assert_eq!(
            view.command,
            "glc-admin set-limit --rpc-url <RPC_URL> --keypair <ADMIN_KEYPAIR_PATH> --field per-transfer --value 20000000000 --note '<NOTE>'"
        );
        assert_eq!(
            view.old_value,
            Some(ValueDisplay {
                atomic: 10_000_000_000,
                display_glc: "10000".to_string()
            })
        );
        assert_eq!(
            view.new_value,
            Some(ValueDisplay {
                atomic: 20_000_000_000,
                display_glc: "20000".to_string()
            })
        );
        assert_eq!(view.label, CLI_APPROVAL_REQUIRED);
    }

    #[test]
    fn set_limit_mirrors_the_onchain_zero_checks() {
        for field in ["per-transfer", "rolling-volume"] {
            let err = generate(
                &CliCommandInput {
                    action: "set-limit".to_string(),
                    scope: None,
                    field: Some(field.to_string()),
                    value_glc: Some("0".to_string()),
                    direction: None,
                    request_id: None,
                },
                &onchain(),
            )
            .unwrap_err();
            assert!(err.contains("ZeroAmount"), "{err}");
        }
        // min-transfer and protected-minimum have no such on-chain check.
        assert!(generate(
            &CliCommandInput {
                action: "set-limit".to_string(),
                scope: None,
                field: Some("min-transfer".to_string()),
                value_glc: Some("0".to_string()),
                direction: None,
                request_id: None,
            },
            &onchain(),
        )
        .is_ok());
    }

    #[test]
    fn pause_command_reports_current_and_target_state() {
        let view = generate(
            &CliCommandInput {
                action: "onchain-unpause".to_string(),
                scope: Some("release".to_string()),
                field: None,
                value_glc: None,
                direction: None,
                request_id: None,
            },
            &onchain(),
        )
        .unwrap();
        assert!(view.command.starts_with("glc-admin onchain-unpause "));
        assert!(view.command.contains("--scope release"));
        assert_eq!(
            view.summary,
            "release pause: currently true, would become false"
        );
    }

    #[test]
    fn reset_rolling_window_previews_remaining_vs_full_capacity() {
        let view = generate(
            &CliCommandInput {
                action: "reset-rolling-window".to_string(),
                scope: None,
                field: None,
                value_glc: None,
                direction: Some("glc-to-sol".to_string()),
                request_id: None,
            },
            &onchain(),
        )
        .unwrap();
        assert!(view.command.contains("--direction glc-to-sol"));
        assert_eq!(view.old_value.unwrap().atomic, 75_000_000_000);
        assert_eq!(view.new_value.unwrap().atomic, 100_000_000_000);
    }

    #[test]
    fn unknown_actions_scopes_fields_and_directions_are_rejected() {
        let base = onchain();
        assert!(generate(
            &CliCommandInput {
                action: "set-paused".to_string(),
                scope: None,
                field: None,
                value_glc: None,
                direction: None,
                request_id: None
            },
            &base
        )
        .is_err());
        assert!(generate(
            &CliCommandInput {
                action: "onchain-pause".to_string(),
                scope: Some("everything".to_string()),
                field: None,
                value_glc: None,
                direction: None,
                request_id: None
            },
            &base
        )
        .is_err());
        assert!(generate(
            &CliCommandInput {
                action: "reset-rolling-window".to_string(),
                scope: None,
                field: None,
                value_glc: None,
                direction: Some("both".to_string()),
                request_id: None
            },
            &base
        )
        .is_err());
    }

    /// The conversion follows the mint's LIVE decimals, never a
    /// compile-time 6: at 9 decimals the same "20000" input is a
    /// different atomic number, and the generated command carries it.
    #[test]
    fn conversion_uses_the_live_mint_decimals_not_a_hardcoded_six() {
        assert_eq!(parse_glc_to_mint_atomic("20000", 9), Ok(20_000_000_000_000));
        assert_eq!(parse_glc_to_mint_atomic("0.000000001", 9), Ok(1));
        assert!(parse_glc_to_mint_atomic("0.0000001", 6).is_err());
        assert!(parse_glc_to_mint_atomic("0.0000001", 9).is_ok());

        let mut base = onchain();
        base.reserve_mint_decimals = Some(9);
        let view = generate(
            &CliCommandInput {
                action: "set-limit".to_string(),
                scope: None,
                field: Some("per-transfer".to_string()),
                value_glc: Some("20000".to_string()),
                direction: None,
                request_id: None,
            },
            &base,
        )
        .unwrap();
        assert!(
            view.command.contains("--value 20000000000000"),
            "{}",
            view.command
        );
        assert_eq!(view.unit, "9-decimal Solana mint atomic units");
    }

    #[test]
    fn value_bearing_actions_refuse_when_no_live_decimals_are_available() {
        let mut base = onchain();
        base.reserve_mint_decimals = None;
        for input in [
            CliCommandInput {
                action: "set-limit".to_string(),
                scope: None,
                field: Some("per-transfer".to_string()),
                value_glc: Some("20000".to_string()),
                direction: None,
                request_id: None,
            },
            CliCommandInput {
                action: "reset-rolling-window".to_string(),
                scope: None,
                field: None,
                value_glc: None,
                direction: Some("glc-to-sol".to_string()),
                request_id: None,
            },
        ] {
            let err = generate(&input, &base).unwrap_err();
            assert!(err.contains("live mint decimals unavailable"), "{err}");
        }
    }

    /// The on-chain reset instruction requires `BridgeConfig.paused ==
    /// true` — the preview must surface that precondition whenever the
    /// bridge is not currently paused, and stay quiet when it is.
    #[test]
    fn reset_rolling_window_surfaces_the_pause_precondition() {
        let unpaused = onchain(); // fixture has paused: false
        let view = generate(
            &CliCommandInput {
                action: "reset-rolling-window".to_string(),
                scope: None,
                field: None,
                value_glc: None,
                direction: Some("glc-to-sol".to_string()),
                request_id: None,
            },
            &unpaused,
        )
        .unwrap();
        let precondition = view
            .precondition
            .expect("unpaused bridge must carry a precondition");
        assert!(
            precondition.contains("BridgeConfig.paused == true"),
            "{precondition}"
        );
        assert!(precondition.contains("onchain-pause"), "{precondition}");
        // Rendered verbatim in the admin UI: single spaces only — a
        // reflowed multi-line literal once shipped ~22-space runs here.
        assert!(
            precondition.contains("`glc-admin onchain-pause --scope global`"),
            "{precondition}"
        );
        assert!(!precondition.contains("  "), "{precondition:?}");

        let mut paused = onchain();
        paused.paused = true;
        let view = generate(
            &CliCommandInput {
                action: "reset-rolling-window".to_string(),
                scope: None,
                field: None,
                value_glc: None,
                direction: Some("glc-to-sol".to_string()),
                request_id: None,
            },
            &paused,
        )
        .unwrap();
        assert!(view.precondition.is_none());
    }

    /// `manual-review-refund` (schema v30): the same no-destination,
    /// no-amount posture as `refund-manual-review`, `--execute` always
    /// present (the CLI records the decision only then), and NEVER an
    /// `--emergency` flag — that word is the operator's to type.
    #[test]
    fn manual_review_refund_is_generated_without_any_override_flag() {
        let input = CliCommandInput {
            action: "manual-review-refund".to_string(),
            scope: None,
            field: None,
            value_glc: None,
            direction: None,
            request_id: Some(4205),
        };
        let view = generate(&input, &onchain()).unwrap();
        assert_eq!(
            view.command,
            format!(
                "glc-admin manual-review-refund --config {REFUND_CONFIG_PATH} --request-id 4205 \
                 --note '{NOTE_PLACEHOLDER}' --keypair {REFUND_ADMIN_KEYPAIR_PATH} --execute"
            )
        );
        assert!(!view.command.contains("--emergency"));
        assert!(!view.command.contains("--destination"));
        assert!(!view.command.contains("--amount"));
        assert_eq!(view.label, CLI_APPROVAL_REQUIRED);
        assert!(
            view.precondition.is_some(),
            "the fixture is not globally paused"
        );
        let mut no_id = input;
        no_id.request_id = None;
        assert!(generate(&no_id, &onchain()).is_err());
    }

    /// Drift guard, mirroring `service/tests/runbook_commands.rs`'s
    /// discipline: every subcommand this module can emit must exist as a
    /// literal match arm in `glc-admin`'s dispatch table, so a renamed
    /// CLI command cannot leave this generator emitting a command that no
    /// longer runs.
    #[test]
    fn every_generated_subcommand_exists_in_glc_admins_dispatch_table() {
        let glc_admin_src = include_str!("../bin/glc-admin.rs");
        for subcommand in GENERATED_SUBCOMMANDS {
            let needle = format!("\"{subcommand}\" =>");
            assert!(
                glc_admin_src.contains(&needle),
                "glc-admin.rs has no dispatch arm for {subcommand:?}"
            );
        }
    }
}
