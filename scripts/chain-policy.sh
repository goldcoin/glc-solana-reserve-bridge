#!/usr/bin/env bash
# Goldcoin Bridge — Chain Policy Manager.
#
# A friendly interactive front end for `glc-admin chain-policy-*`. It draws
# menus and asks questions; it does NOT parse config files, do arithmetic on
# a fee rate, or edit anything. Every one of those happens in glc-admin,
# behind the same types and the same config parser the daemon itself uses,
# because a shell script that re-implemented any of them would be a second
# set of rules — and the second set is always the one that is wrong.
#
# What this script can never do, because the commands it calls cannot:
#   - enable a route
#   - read or write a secret
#   - restart the daemon
#   - sign or submit an on-chain governance transaction
#
# The network menu is built from `glc-admin chain-policy-networks`, which
# derives its list from the route registry — so a future chain appears here
# with no edit to this file.
#
# THE CONFIG PATH IS THE FULL BRIDGE CONFIG, and this script checks that
# before it draws a single menu. `--config` must name the file the daemon
# itself loads — the one with [solana], [goldcoin], [reserve],
# [operators] and [service] sections in it, typically
# /etc/glc-bridge/config.toml. It is NOT a policy fragment such as
# docs/robinhood/launch-policy.toml.example: that file states the
# approved policy for documentation and is not, and never becomes, a
# config file. Given one, `glc-admin chain-policy-check-config` says so
# in those words — and prints the policy the fragment states, read-only,
# plus the flags that would put it into a real config — and this script
# asks again instead of entering a menu whose every action would fail
# with the same parse error.
#
# Usage:
#   scripts/chain-policy.sh --config /path/to/config.toml
#   scripts/chain-policy.sh                 # prompts for the config path
#
# Environment:
#   GLC_ADMIN   path to the glc-admin binary (default: search PATH, then
#               service/target/{release,debug}/glc-admin)

set -euo pipefail

# ------------------------------------------------------------------ setup --

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

find_glc_admin() {
    if [ -n "${GLC_ADMIN:-}" ]; then
        printf '%s' "$GLC_ADMIN"
        return 0
    fi
    if command -v glc-admin >/dev/null 2>&1; then
        command -v glc-admin
        return 0
    fi
    local candidate
    for candidate in \
        "$repo_root/service/target/release/glc-admin" \
        "$repo_root/service/target/debug/glc-admin"; do
        if [ -x "$candidate" ]; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    return 1
}

if ! GLC_ADMIN_BIN="$(find_glc_admin)"; then
    cat >&2 <<'EOF'
error: could not find the glc-admin binary.

Build it, or point GLC_ADMIN at it:

    cd service && cargo build --release --bin glc-admin
    GLC_ADMIN=/path/to/glc-admin scripts/chain-policy.sh --config ...
EOF
    exit 2
fi

CONFIG=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --config)
            CONFIG="${2:-}"
            shift 2 || true
            ;;
        -h|--help)
            sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

# ------------------------------------------------------------- utilities --

rule() { printf '%s\n' "----------------------------------------------------------------------"; }

# Reads one line. Returns non-zero on EOF so the script exits cleanly when
# stdin closes rather than looping forever.
# The locals here are deliberately `__ask_`-prefixed: `printf -v "$2"`
# assigns by NAME, so a local in this function sharing the caller's
# variable name would shadow it and the caller would read an empty string
# back. That is a silent wrong answer, which for a fee prompt is the worst
# possible failure.
ask() {
    local __ask_prompt="$1" __ask_var="$2" __ask_line
    printf '%s' "$__ask_prompt" >&2
    IFS= read -r __ask_line || return 1
    printf -v "$__ask_var" '%s' "$__ask_line"
}

# Runs glc-admin, showing the exact command first. Nothing this script does
# is hidden from the operator: the command line is the audit trail.
run_admin() {
    printf '\n$ %s' "$GLC_ADMIN_BIN"
    printf ' %q' "$@"
    printf '\n\n'
    "$GLC_ADMIN_BIN" "$@"
}

# Establishes a config path that the REAL parser accepts, before any menu
# exists.
#
# The check is `glc-admin chain-policy-check-config`, not a test in this
# script: deciding whether a file is a bridge config means loading it the
# way the daemon loads it, and a shell approximation of that — grepping
# for "[solana]", say — would be a second set of rules that disagrees
# with the first the moment a section is renamed.
#
# Every rejected path is explained once, here, and then asked about
# again. The alternative is what this replaced: a menu that draws fine
# and answers every single action with the same parse error.
require_config() {
    while true; do
        if [ -z "$CONFIG" ]; then
            ask "Path to full bridge config.toml: " CONFIG || exit 1
            continue
        fi
        if run_admin chain-policy-check-config --config "$CONFIG"; then
            return 0
        fi
        echo
        echo "That file cannot be used here — see above. Nothing was read from"
        echo "it beyond the check, and nothing anywhere was written."
        echo "The path wanted is the full bridge config the daemon loads,"
        echo "typically /etc/glc-bridge/config.toml."
        CONFIG=""
    done
}

# --------------------------------------------------------- network menu --

# The list comes from the binary, not from this file — requirement: do not
# hardcode a menu independently of the chain registry.
load_networks() {
    mapfile -t NETWORKS < <("$GLC_ADMIN_BIN" chain-policy-networks --porcelain | cut -f1)
    # Field 3 is the operator-facing display name ("Robinhood Network"),
    # field 1 the identifier the commands take ("robinhood"). The menu
    # shows the first and passes the second.
    mapfile -t NETWORK_NAMES < <("$GLC_ADMIN_BIN" chain-policy-networks --porcelain | cut -f3)
    mapfile -t NETWORK_KINDS < <("$GLC_ADMIN_BIN" chain-policy-networks --porcelain | cut -f2)
    if [ "${#NETWORKS[@]}" -eq 0 ]; then
        echo "error: glc-admin chain-policy-networks returned no networks" >&2
        exit 1
    fi
}

choose_network() {
    local i choice
    echo
    rule
    echo "Goldcoin Bridge — Chain Policy Manager"
    rule
    echo
    echo "Select network:"
    for i in "${!NETWORKS[@]}"; do
        if [ "${NETWORK_KINDS[$i]}" = configurable ]; then
            printf '%d. %s\n' "$((i + 1))" "${NETWORK_NAMES[$i]}"
        else
            printf '%d. %s   (policy not changeable here — shown read-only)\n' \
                "$((i + 1))" "${NETWORK_NAMES[$i]}"
        fi
    done
    printf '%d. Exit\n' "$(( ${#NETWORKS[@]} + 1 ))"
    echo
    ask "Choice: " choice || exit 0
    case "$choice" in
        ''|*[!0-9]*)
            echo "not a number: $choice" >&2
            return 1
            ;;
    esac
    if [ "$choice" -ge 1 ] && [ "$choice" -le "${#NETWORKS[@]}" ]; then
        NETWORK="${NETWORKS[$((choice - 1))]}"
        NETWORK_LABEL="${NETWORK_NAMES[$((choice - 1))]}"
        return 0
    fi
    if [ "$choice" -eq $(( ${#NETWORKS[@]} + 1 )) ]; then
        exit 0
    fi
    echo "out of range: $choice" >&2
    return 1
}

# ------------------------------------------------------- policy actions --

show_policy() {
    run_admin chain-policy-show --config "$CONFIG" --network "$NETWORK" || true
}

# Whether the selected network's policy can be changed here at all. Asked of
# the binary rather than assumed, so a chain whose limits are immutable or
# governed elsewhere says so in its own words.
network_is_configurable() {
    "$GLC_ADMIN_BIN" chain-policy-networks --porcelain \
        | awk -F'\t' -v n="$NETWORK" '$1 == n && $2 == "configurable" { found = 1 } END { exit !found }' 
}

# Reads the current value of one field so "change just the fee" can keep the
# other two exactly as they are. Empty when no policy is configured yet.
current_field() {
    "$GLC_ADMIN_BIN" chain-policy-show --config "$CONFIG" --network "$NETWORK" --porcelain \
        | awk -F'\t' -v k="$1" '$1 == k { print $2; exit }' 
}

# Collects the four values, prompting only for the ones being changed and
# carrying the rest across unchanged. The two per-transfer limits are
# separate on purpose (backend docs/40-destination-bound-admission.md):
# INBOUND is the user's deposit ceiling into the contract (inboundMax),
# OUTBOUND is how much one settlement may pay out of it (outboundMax) —
# destination capacity, sized for the elastic payouts a source-side
# maximum can produce, never a user limit.
collect_values() {
    local which="$1"
    FEE_ARGS=() PER_ARGS=() ROLL_ARGS=()
    local cur_fee cur_in cur_out cur_roll answer
    cur_fee="$(current_field fee_bps)"
    cur_in="$(current_field inbound_per_transfer_limit)"
    cur_out="$(current_field outbound_per_transfer_limit)"
    cur_roll="$(current_field rolling_daily_limit)"

    if [ "$which" = fee ] || [ "$which" = all ]; then
        ask "New fee, as a percentage (e.g. 6, 3, 1.5): " answer || return 1
        FEE_ARGS=(--fee-percent "$answer")
    else
        [ -n "$cur_fee" ] || { echo "no fee is configured yet — choose 'Change all'" >&2; return 1; }
        FEE_ARGS=(--fee-bps "$cur_fee")
    fi

    if [ "$which" = inbound ] || [ "$which" = all ]; then
        ask "New INBOUND per-transfer limit (user deposit ceiling, inboundMax), in GLC (e.g. 20000): " answer || return 1
        PER_ARGS+=(--inbound-per-transfer-glc "$answer")
    else
        [ -n "$cur_in" ] || { echo "no inbound per-transfer limit is configured yet — choose 'Change all'" >&2; return 1; }
        PER_ARGS+=(--inbound-per-transfer-limit "$cur_in")
    fi

    if [ "$which" = outbound ] || [ "$which" = all ]; then
        ask "New OUTBOUND per-transfer limit (destination settlement capacity, outboundMax), in GLC (e.g. 2000000): " answer || return 1
        PER_ARGS+=(--outbound-per-transfer-glc "$answer")
    else
        [ -n "$cur_out" ] || { echo "no outbound per-transfer limit is configured yet — choose 'Change all'" >&2; return 1; }
        PER_ARGS+=(--outbound-per-transfer-limit "$cur_out")
    fi

    if [ "$which" = roll ] || [ "$which" = all ]; then
        ask "New STRICT 24h rolling limit, in GLC (e.g. 10000000): " answer || return 1
        ROLL_ARGS=(--rolling-glc "$answer")
    else
        [ -n "$cur_roll" ] || { echo "no rolling limit is configured yet — choose 'Change all'" >&2; return 1; }
        ROLL_ARGS=(--rolling-daily-limit "$cur_roll")
    fi
}

# Validate, preview (dry run), confirm, apply. Each step is a separate
# glc-admin invocation and any failure stops here — nothing is written
# unless the operator types the confirmation word after seeing the diff.
change_policy() {
    local which="$1" note answer
    if ! network_is_configurable; then
        echo
        echo "$NETWORK's policy cannot be changed here. Its own description:"
        show_policy
        return 0
    fi
    collect_values "$which" || return 1

    echo
    rule
    echo "Step 1/3 — validate (writes nothing)"
    rule
    if ! run_admin chain-policy-validate --config "$CONFIG" --network "$NETWORK" \
        "${FEE_ARGS[@]}" "${PER_ARGS[@]}" "${ROLL_ARGS[@]}"; then
        echo
        echo "Rejected. Nothing was changed."
        return 1
    fi

    ask $'\nShort note for the audit trail: ' note || return 1
    if [ -z "$note" ]; then
        echo "a note is required." >&2
        return 1
    fi

    echo
    rule
    echo "Step 2/3 — dry run (shows the exact before/after; writes nothing)"
    rule
    if ! run_admin chain-policy-apply --config "$CONFIG" --network "$NETWORK" \
        "${FEE_ARGS[@]}" "${PER_ARGS[@]}" "${ROLL_ARGS[@]}" --note "$note" --dry-run; then
        echo
        echo "Dry run failed. Nothing was changed."
        return 1
    fi

    echo
    rule
    echo "Step 3/3 — apply"
    rule
    echo "This will back up $CONFIG and rewrite it. It will NOT restart the"
    echo "daemon, enable any route, or send any on-chain transaction."
    ask $'Type APPLY to continue, anything else to abort: ' answer || return 1
    if [ "$answer" != "APPLY" ]; then
        echo "Aborted. Nothing was changed."
        return 0
    fi
    run_admin chain-policy-apply --config "$CONFIG" --network "$NETWORK" \
        "${FEE_ARGS[@]}" "${PER_ARGS[@]}" "${ROLL_ARGS[@]}" --note "$note" --execute
}

action_menu() {
    local choice
    while true; do
        echo
        rule
        echo "Network: $NETWORK_LABEL ($NETWORK)"
        echo "Config:  $CONFIG"
        rule
        show_policy
        echo
        echo "1. Change fee"
        echo "2. Change INBOUND per-transfer limit  (user deposit ceiling — inboundMax)"
        echo "3. Change OUTBOUND per-transfer limit (destination settlement capacity — outboundMax)"
        echo "4. Change 24h rolling limit"
        echo "5. Change all"
        echo "6. Show policy only"
        echo "7. Back to network selection"
        echo "8. Exit"
        echo
        ask "Choice: " choice || exit 0
        case "$choice" in
            1) change_policy fee || true ;;
            2) change_policy inbound || true ;;
            3) change_policy outbound || true ;;
            4) change_policy roll || true ;;
            5) change_policy all || true ;;
            6) show_policy ;;
            7) return 0 ;;
            8) exit 0 ;;
            *) echo "not a valid choice: $choice" >&2 ;;
        esac
    done
}

# ------------------------------------------------------------------ main --

require_config
load_networks
while true; do
    if choose_network; then
        action_menu
    fi
done
