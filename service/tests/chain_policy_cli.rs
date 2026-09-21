//! The chain policy manager, end to end: the real `glc-admin` binary and
//! the real `scripts/chain-policy.sh`, against a real config file.
//!
//! # Why this exists alongside the unit tests
//!
//! `chain_policy::{human, edit}`'s own tests prove the conversions and the
//! backup/atomic-write behaviour. They cannot prove the two things an
//! operator actually depends on: that the COMMANDS wire those pieces up
//! the way the runbook says, and that the SCRIPT — which is the thing an
//! operator types — reaches them. A shell wrapper that silently passed the
//! wrong flag would leave every unit test green.
//!
//! Nothing here touches a network, a daemon, a secret or a chain. Every
//! config file is a throwaway in a temp directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use solana_sdk::signature::{Keypair, Signer};

fn write_solana_keypair_file(dir: &Path, name: &str) -> (PathBuf, Keypair) {
    let keypair = Keypair::new();
    let path = dir.join(name);
    std::fs::write(
        &path,
        serde_json::to_string(&keypair.to_bytes().to_vec()).unwrap(),
    )
    .unwrap();
    (path, keypair)
}

fn write_vault_key_file(dir: &Path, name: &str) -> (PathBuf, [u8; 33]) {
    let secret_key = libsecp256k1::SecretKey::random(&mut rand::rngs::OsRng);
    let pubkey = libsecp256k1::PublicKey::from_secret_key(&secret_key).serialize_compressed();
    let path = dir.join(name);
    std::fs::write(
        &path,
        glc_reserve_bridge_service::goldcoin::hex::encode(&secret_key.serialize()),
    )
    .unwrap();
    (path, pubkey)
}

/// A loadable config with a `[robinhood.policy]` section holding
/// deliberately PRE-LAUNCH values, plus an operator comment that every
/// assertion about preservation keys on.
fn config_with_policy(dir: &Path) -> PathBuf {
    let (a1_path, a1) = write_solana_keypair_file(dir, "attest1.json");
    let (a2_path, a2) = write_solana_keypair_file(dir, "attest2.json");
    let (a3_path, a3) = write_solana_keypair_file(dir, "attest3.json");
    let (v1_path, v1) = write_vault_key_file(dir, "vault1.hex");
    let (v2_path, v2) = write_vault_key_file(dir, "vault2.hex");
    let (v3_path, v3) = write_vault_key_file(dir, "vault3.hex");
    let (sub_path, _) = write_solana_keypair_file(dir, "submitter.json");
    let hex = glc_reserve_bridge_service::goldcoin::hex::encode;

    let toml = format!(
        r#"
[solana]
rpc_url = "http://127.0.0.1:8899"
commitment = "finalized"
reserve_token_mint = "{mint}"

[goldcoin]
network = "regtest"
rpc_url = "http://127.0.0.1:18332"
rpc_user = "user"
rpc_password = "pass"
confirmation_depth = 3
max_reorg_depth = 50
required_payout_confirmations = 3
vault_min_confirmations = 1
fee_rate_per_kb = 100000
dust_threshold = 1000
max_inputs = 10

[reserve]
reconciliation_tolerance = 0

[reserve.solana]
protected_minimum = 0
target_reserve = 50000000000
warning_reserve = 20000000000
critical_reserve = 10000000000

[reserve.goldcoin]
protected_minimum = 0
target_reserve = 50000000000
warning_reserve = 20000000000
critical_reserve = 10000000000

[operators]
admin_pubkey = "{admin}"
attestation_threshold = 2
attestation_pubkeys = ["{a1}", "{a2}", "{a3}"]
attestation_key_paths = ["{a1_path}", "{a2_path}", "{a3_path}"]
vault_threshold = 2
vault_pubkeys = ["{v1}", "{v2}", "{v3}"]
vault_key_paths = ["{v1_path}", "{v2_path}", "{v3_path}"]
submitter_key_path = "{sub_path}"

[bridge_rate]
mode = "fixed_unit"

[service]
db_path = "{db}"
tick_interval_ms = 5000
health_bind_addr = "127.0.0.1:9100"
reservation_ttl_secs = 3600

# OPERATOR NOTE: this comment records why these numbers were chosen and
# must survive every edit the policy manager makes.
[robinhood.policy]
fee_bps = 300
per_transfer_limit = 1000000000000
rolling_daily_limit = 4000000000000
"#,
        mint = Keypair::new().pubkey(),
        admin = Keypair::new().pubkey(),
        a1 = a1.pubkey(),
        a2 = a2.pubkey(),
        a3 = a3.pubkey(),
        a1_path = a1_path.display(),
        a2_path = a2_path.display(),
        a3_path = a3_path.display(),
        v1 = hex(&v1),
        v2 = hex(&v2),
        v3 = hex(&v3),
        v1_path = v1_path.display(),
        v2_path = v2_path.display(),
        v3_path = v3_path.display(),
        sub_path = sub_path.display(),
        db = dir.join("ledger.sqlite3").display(),
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, toml).unwrap();
    path
}

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Output {
    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

fn admin(args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_glc-admin"))
        .args(args)
        .output()
        .expect("glc-admin runs");
    Output {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the service crate has a parent directory")
        .to_path_buf()
}

/// Drives `scripts/chain-policy.sh` with a scripted set of menu answers.
fn script(config: &Path, keystrokes: &str) -> Output {
    use std::io::Write;

    let mut child = Command::new("bash")
        .arg(repo_root().join("scripts/chain-policy.sh"))
        .arg("--config")
        .arg(config)
        .env("GLC_ADMIN", env!("CARGO_BIN_EXE_glc-admin"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the policy manager script runs");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(keystrokes.as_bytes())
        .expect("keystrokes are accepted");
    let out = child.wait_with_output().expect("the script exits");
    Output {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn policy_field(config: &Path, field: &str) -> Option<String> {
    let out = admin(&[
        "chain-policy-show",
        "--config",
        config.to_str().unwrap(),
        "--network",
        "robinhood",
        "--porcelain",
    ]);
    assert!(out.ok, "{}", out.all());
    out.stdout.lines().find_map(|line| {
        let (k, v) = line.split_once('\t')?;
        (k == field).then(|| v.to_string())
    })
}

fn backups(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".bak."))
        .collect();
    names.sort();
    names
}

// =====================================================================
// Network selection
// =====================================================================

/// The networks come from the route registry, and both of this bridge's
/// are offered — with Solana marked as one whose policy is NOT
/// changeable here rather than quietly presented as if it were.
#[test]
fn the_network_list_comes_from_the_route_registry() {
    let out = admin(&["chain-policy-networks", "--porcelain"]);
    assert!(out.ok, "{}", out.all());
    let rows: Vec<Vec<&str>> = out
        .stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('\t').collect())
        .collect();

    let names: Vec<&str> = rows.iter().map(|r| r[0]).collect();
    assert_eq!(names, vec!["solana", "robinhood"], "{}", out.stdout);
    // Goldcoin is the home chain, not a network the bridge holds a
    // policy towards, and must never appear as one.
    assert!(!names.contains(&"goldcoin"), "{}", out.stdout);

    assert_eq!(rows[0][1], "fixed", "solana's policy is not configurable");
    assert_eq!(rows[1][1], "configurable");
}

#[test]
fn the_script_builds_its_menu_from_that_list() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    // "3" is Exit, because there are exactly two networks.
    let out = script(&config, "3\n");
    let text = out.all();

    assert!(
        text.contains("Goldcoin Bridge — Chain Policy Manager"),
        "{text}"
    );
    assert!(text.contains("Select network:"), "{text}");
    assert!(text.contains("1. Solana"), "{text}");
    assert!(text.contains("2. Robinhood Network"), "{text}");
    assert!(text.contains("3. Exit"), "{text}");
    assert!(
        text.contains("policy not changeable here"),
        "Solana must be marked read-only in the menu: {text}"
    );
}

/// Selecting a network shows the CURRENT values first, before any menu of
/// changes — the operator sees what is true before being asked what to
/// change.
#[test]
fn selecting_a_network_shows_the_current_policy_first() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let out = script(&config, "2\n8\n");
    let text = out.all();

    let policy_at = text
        .find("Backend configured policy:")
        .expect("current policy shown");
    let menu_at = text.find("1. Change fee").expect("the action menu");
    assert!(
        policy_at < menu_at,
        "current values must come first:\n{text}"
    );

    assert!(text.contains("Fee:                 3%"), "{text}");
    assert!(text.contains("10,000 GLC"), "{text}");
    assert!(text.contains("40,000 GLC"), "{text}");
    for entry in [
        "1. Change fee",
        "2. Change INBOUND per-transfer limit",
        "3. Change OUTBOUND per-transfer limit",
        "4. Change 24h rolling limit",
        "5. Change all",
        "6. Show policy only",
    ] {
        assert!(text.contains(entry), "missing {entry}:\n{text}");
    }
}

#[test]
fn an_unsupported_network_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    for network in ["goldcoin", "ethereum", "", "ROBINHOOD"] {
        let out = admin(&[
            "chain-policy-show",
            "--config",
            config.to_str().unwrap(),
            "--network",
            network,
        ]);
        assert!(!out.ok, "{network:?} must be refused: {}", out.all());
        assert!(
            out.all().contains("unsupported network") || out.all().contains("missing required"),
            "{network:?}: {}",
            out.all()
        );
    }
}

// =====================================================================
// Conversions, through the real command line
// =====================================================================

/// The launch session's numbers, typed the way an operator types them,
/// converted the way the config file stores them.
#[test]
fn human_input_converts_to_the_documented_atomic_values() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let out = admin(&[
        "chain-policy-validate",
        "--config",
        config.to_str().unwrap(),
        "--network",
        "robinhood",
        "--fee-percent",
        "6",
        "--per-transfer-glc",
        "20000",
        "--rolling-glc",
        "10000000",
    ]);
    assert!(out.ok, "{}", out.all());
    let text = out.stdout;

    assert!(text.contains("6%"), "{text}");
    assert!(text.contains("(600 bps)"), "{text}");
    assert!(text.contains("20,000 GLC"), "{text}");
    assert!(text.contains("2000000000000 canonical 8dp"), "{text}");
    assert!(text.contains("10,000,000 GLC"), "{text}");
    assert!(text.contains("1000000000000000 canonical 8dp"), "{text}");
    // Both the human value and the canonical value are shown before any
    // confirmation is asked for.
    assert!(text.contains("Nothing was written"), "{text}");
}

/// The fixed-bucket relationship, displayed. 10,000,000 strict means
/// 5,000,000 on chain, and the command says so in both units.
#[test]
fn a_ten_million_strict_policy_displays_a_five_million_on_chain_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let out = admin(&[
        "chain-policy-validate",
        "--config",
        config.to_str().unwrap(),
        "--network",
        "robinhood",
        "--fee-bps",
        "600",
        "--per-transfer-limit",
        "2000000000000",
        "--rolling-daily-limit",
        "1000000000000000",
    ]);
    assert!(out.ok, "{}", out.all());
    let text = out.stdout;

    assert!(
        text.contains("Requested strict 24h policy:       10,000,000 GLC"),
        "{text}"
    );
    assert!(
        text.contains("Recommended on-chain bucket limit:  5,000,000 GLC"),
        "{text}"
    );
    assert!(
        text.contains("5000000000000000000000000"),
        "the 18-decimal on-chain figure must be shown: {text}"
    );
    // And it must be explicit that nothing sends the governance change.
    assert!(text.contains("setLimits"), "{text}");
    assert!(text.contains("DOES NOT SEND IT"), "{text}");
}

#[test]
fn invalid_values_are_refused_and_named() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let path = config.to_str().unwrap().to_string();

    // (fee, per-transfer, rolling, what makes it invalid)
    let cases: [(&str, &str, &str, &str); 8] = [
        ("-6", "20000", "10000000", "negative fee"),
        ("6", "-20000", "10000000", "negative amount"),
        ("100", "20000", "10000000", "fee at 100%"),
        ("150", "20000", "10000000", "fee above 100%"),
        ("6", "0", "10000000", "zero transfer limit"),
        ("6", "20000", "10000", "rolling below per-transfer"),
        ("6", "abc", "10000000", "malformed amount"),
        ("6", "20000", "99999999999999", "overflow"),
    ];
    for (fee, per, roll, why) in cases {
        let out = admin(&[
            "chain-policy-validate",
            "--config",
            &path,
            "--network",
            "robinhood",
            "--fee-percent",
            fee,
            "--per-transfer-glc",
            per,
            "--rolling-glc",
            roll,
        ]);
        assert!(!out.ok, "{why} must be refused: {}", out.all());
        assert!(
            !out.all().contains("VALID"),
            "{why} must not report VALID: {}",
            out.all()
        );
    }
}

/// The exact-value and human-value flags are two ways to say one thing,
/// and passing both is an ambiguity about money rather than a convenience.
#[test]
fn giving_a_value_twice_in_two_units_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let out = admin(&[
        "chain-policy-validate",
        "--config",
        config.to_str().unwrap(),
        "--network",
        "robinhood",
        "--fee-bps",
        "600",
        "--fee-percent",
        "3",
        "--per-transfer-glc",
        "20000",
        "--rolling-glc",
        "10000000",
    ]);
    assert!(!out.ok, "{}", out.all());
    assert!(out.all().contains("exactly one"), "{}", out.all());
}

// =====================================================================
// Solana isolation
// =====================================================================

/// Selecting Solana explains how Solana is governed and refuses to change
/// it, rather than pretending every chain behaves identically.
#[test]
fn solana_is_shown_read_only_and_never_written() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let before = std::fs::read(&config).unwrap();

    let show = admin(&[
        "chain-policy-show",
        "--config",
        config.to_str().unwrap(),
        "--network",
        "solana",
    ]);
    assert!(show.ok, "{}", show.all());
    assert!(
        show.stdout.contains("NOT CHANGEABLE by this tool"),
        "{}",
        show.stdout
    );
    assert!(
        show.stdout.contains("glc-admin set-limit"),
        "{}",
        show.stdout
    );
    assert!(
        show.stdout.contains("compiled-in"),
        "the fee's real source must be named: {}",
        show.stdout
    );

    for extra in [vec!["--execute"], vec!["--dry-run"], vec![]] {
        let mut args = vec![
            "chain-policy-apply",
            "--config",
            config.to_str().unwrap(),
            "--network",
            "solana",
            "--fee-percent",
            "6",
            "--per-transfer-glc",
            "20000",
            "--rolling-glc",
            "10000000",
            "--note",
            "must be refused",
        ];
        args.extend(extra);
        let out = admin(&args);
        assert!(!out.ok, "Solana apply must fail: {}", out.all());
        assert!(out.all().contains("Nothing was written"), "{}", out.all());
    }

    assert_eq!(std::fs::read(&config).unwrap(), before);
    assert!(backups(dir.path()).is_empty());
    // And the Robinhood policy is exactly as it was.
    assert_eq!(policy_field(&config, "fee_bps").as_deref(), Some("300"));
}

/// A Robinhood change through the whole tool leaves every Solana-facing
/// line in the file untouched.
#[test]
fn a_robinhood_change_does_not_touch_the_solana_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let before: Vec<String> = std::fs::read_to_string(&config)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();

    let out = admin(&[
        "chain-policy-apply",
        "--config",
        config.to_str().unwrap(),
        "--network",
        "robinhood",
        "--fee-percent",
        "6",
        "--per-transfer-glc",
        "20000",
        "--rolling-glc",
        "10000000",
        "--note",
        "launch policy",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());

    let after: Vec<String> = std::fs::read_to_string(&config)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    // Every line that is not one of the three policy keys is unchanged,
    // in the same order.
    let strip = |lines: &Vec<String>| -> Vec<String> {
        lines
            .iter()
            .filter(|l| {
                let t = l.trim_start();
                !(t.starts_with("fee_bps")
                    || t.starts_with("per_transfer_limit")
                    || t.starts_with("inbound_per_transfer_limit")
                    || t.starts_with("outbound_per_transfer_limit")
                    || t.starts_with("rolling_daily_limit"))
            })
            .cloned()
            .collect()
    };
    assert_eq!(strip(&before), strip(&after));
    assert!(after.iter().any(|l| l.contains("[solana]")));
    assert!(after.iter().any(|l| l.contains("[reserve.solana]")));
}

// =====================================================================
// Dry run, backup, atomic write
// =====================================================================

#[test]
fn a_dry_run_prints_the_diff_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let before = std::fs::read(&config).unwrap();
    let entries_before = std::fs::read_dir(dir.path()).unwrap().count();

    for extra in [vec!["--dry-run"], vec![]] {
        let mut args = vec![
            "chain-policy-apply",
            "--config",
            config.to_str().unwrap(),
            "--network",
            "robinhood",
            "--fee-percent",
            "6",
            "--per-transfer-glc",
            "20000",
            "--rolling-glc",
            "10000000",
            "--note",
            "preview only",
        ];
        args.extend(extra.clone());
        let out = admin(&args);
        assert!(out.ok, "{}", out.all());

        // The exact before/after values are printed.
        assert!(out.stdout.contains("BEFORE:"), "{}", out.stdout);
        assert!(out.stdout.contains("AFTER:"), "{}", out.stdout);
        assert!(out.stdout.contains("3%"), "{}", out.stdout);
        assert!(out.stdout.contains("6%"), "{}", out.stdout);
        assert!(
            out.stdout.contains("DRY RUN"),
            "{:?}: {}",
            extra,
            out.stdout
        );
        assert!(!out.stdout.contains("APPLIED"), "{}", out.stdout);

        // And nothing at all changed on disk — no edit, no backup, no
        // leftover candidate file.
        assert_eq!(std::fs::read(&config).unwrap(), before, "{extra:?}");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            entries_before,
            "{extra:?} left a file behind"
        );
    }
    assert_eq!(policy_field(&config, "fee_bps").as_deref(), Some("300"));
}

#[test]
fn dry_run_and_execute_together_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let before = std::fs::read(&config).unwrap();
    let out = admin(&[
        "chain-policy-apply",
        "--config",
        config.to_str().unwrap(),
        "--network",
        "robinhood",
        "--fee-percent",
        "6",
        "--per-transfer-glc",
        "20000",
        "--rolling-glc",
        "10000000",
        "--note",
        "contradictory",
        "--dry-run",
        "--execute",
    ]);
    assert!(!out.ok, "{}", out.all());
    assert_eq!(std::fs::read(&config).unwrap(), before);
}

#[test]
fn applying_backs_up_the_original_and_installs_the_new_policy() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let original = std::fs::read_to_string(&config).unwrap();

    let out = admin(&[
        "chain-policy-apply",
        "--config",
        config.to_str().unwrap(),
        "--network",
        "robinhood",
        "--fee-percent",
        "6",
        "--per-transfer-glc",
        "20000",
        "--rolling-glc",
        "10000000",
        "--note",
        "Robinhood mainnet launch policy",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    assert!(out.stdout.contains("APPLIED."), "{}", out.stdout);

    // A timestamped backup holding the original byte for byte.
    let names = backups(dir.path());
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names[0].starts_with("config.toml.bak."), "{names:?}");
    assert!(names[0].ends_with('Z'), "UTC-stamped: {names:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join(&names[0])).unwrap(),
        original
    );

    // No candidate file survives the rename.
    assert!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .all(|e| !e.file_name().to_string_lossy().contains("candidate")),
        "a candidate file was left behind"
    );

    // The new values are in force, and the operator comment survived.
    assert_eq!(policy_field(&config, "fee_bps").as_deref(), Some("600"));
    // The legacy one-figure flag installs the directional pair.
    assert_eq!(
        policy_field(&config, "inbound_per_transfer_limit").as_deref(),
        Some("2000000000000")
    );
    assert_eq!(
        policy_field(&config, "outbound_per_transfer_limit").as_deref(),
        Some("2000000000000")
    );
    assert_eq!(
        policy_field(&config, "rolling_daily_limit").as_deref(),
        Some("1000000000000000")
    );
    assert!(std::fs::read_to_string(&config)
        .unwrap()
        .contains("OPERATOR NOTE:"));

    // It does not restart anything, and it says so.
    assert!(
        out.stdout.contains("has NOT been restarted"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("No route was enabled"),
        "{}",
        out.stdout
    );
}

/// An audit note is mandatory, exactly as it is for every other
/// state-changing `glc-admin` command.
#[test]
fn applying_without_a_note_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let before = std::fs::read(&config).unwrap();
    let out = admin(&[
        "chain-policy-apply",
        "--config",
        config.to_str().unwrap(),
        "--network",
        "robinhood",
        "--fee-percent",
        "6",
        "--per-transfer-glc",
        "20000",
        "--rolling-glc",
        "10000000",
        "--execute",
    ]);
    assert!(!out.ok, "{}", out.all());
    assert_eq!(std::fs::read(&config).unwrap(), before);
    assert!(backups(dir.path()).is_empty());
}

/// An invalid policy never reaches the file, and never leaves a backup or
/// a partial write behind.
#[test]
fn an_invalid_policy_never_partially_modifies_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let before = std::fs::read(&config).unwrap();
    let entries_before = std::fs::read_dir(dir.path()).unwrap().count();

    let out = admin(&[
        "chain-policy-apply",
        "--config",
        config.to_str().unwrap(),
        "--network",
        "robinhood",
        "--fee-percent",
        "6",
        "--per-transfer-glc",
        "20000",
        "--rolling-glc",
        "10000",
        "--note",
        "rolling below per-transfer",
        "--execute",
    ]);
    assert!(!out.ok, "{}", out.all());
    assert_eq!(std::fs::read(&config).unwrap(), before);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        entries_before
    );
    assert!(backups(dir.path()).is_empty());
}

// =====================================================================
// The script's own confirmation gate
// =====================================================================

/// The script asks before it applies, and anything other than the
/// confirmation word leaves the file alone.
#[test]
fn the_script_aborts_without_the_confirmation_word() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let before = std::fs::read(&config).unwrap();

    // network 2 (robinhood) -> 5 (change all) -> fee, inbound, outbound,
    // rolling -> note -> "no"
    let out = script(&config, "2\n5\n6\n20000\n20000\n10000000\nlaunch\nno\n8\n");
    let text = out.all();

    assert!(text.contains("Step 1/3"), "{text}");
    assert!(text.contains("Step 2/3"), "{text}");
    assert!(text.contains("Aborted. Nothing was changed."), "{text}");
    assert!(!text.contains("APPLIED."), "{text}");
    assert_eq!(std::fs::read(&config).unwrap(), before);
    assert!(backups(dir.path()).is_empty());
}

/// And the whole session, driven from the menu, produces exactly the
/// launch policy.
#[test]
fn the_script_applies_the_launch_policy_after_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());

    let out = script(
        &config,
        "2\n5\n6\n20000\n20000\n10000000\nRobinhood mainnet launch\nAPPLY\n8\n",
    );
    let text = out.all();

    assert!(
        text.contains("Recommended on-chain bucket limit:  5,000,000 GLC"),
        "{text}"
    );
    assert!(text.contains("APPLIED."), "{text}");
    assert_eq!(policy_field(&config, "fee_bps").as_deref(), Some("600"));
    // The legacy one-figure flag installs the directional pair.
    assert_eq!(
        policy_field(&config, "inbound_per_transfer_limit").as_deref(),
        Some("2000000000000")
    );
    assert_eq!(
        policy_field(&config, "outbound_per_transfer_limit").as_deref(),
        Some("2000000000000")
    );
    assert_eq!(
        policy_field(&config, "rolling_daily_limit").as_deref(),
        Some("1000000000000000")
    );
    assert_eq!(backups(dir.path()).len(), 1);
}

/// Changing ONE field carries the other two across untouched — the
/// "change just the fee" path must not quietly reset a limit.
#[test]
fn changing_one_field_leaves_the_others_exactly_as_they_were() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());

    let out = script(&config, "2\n1\n6\nfee only\nAPPLY\n8\n");
    assert!(out.all().contains("APPLIED."), "{}", out.all());

    assert_eq!(policy_field(&config, "fee_bps").as_deref(), Some("600"));
    assert_eq!(
        policy_field(&config, "inbound_per_transfer_limit").as_deref(),
        Some("1000000000000"),
        "the inbound per-transfer limit must be unchanged"
    );
    assert_eq!(
        policy_field(&config, "outbound_per_transfer_limit").as_deref(),
        Some("1000000000000"),
        "the outbound per-transfer limit must be unchanged"
    );
    assert_eq!(
        policy_field(&config, "rolling_daily_limit").as_deref(),
        Some("4000000000000"),
        "the rolling limit must be unchanged"
    );
}

// =====================================================================
// The path an operator typed is not always a config file
// =====================================================================
//
// `docs/robinhood/launch-policy.toml.example` states the approved
// Robinhood policy and nothing else. Pointing `--config` at it used to
// produce the config parser's literal answer —
//
//     TOML parse error at line 1, column 1
//     missing field `solana`
//
// — once per menu action, which names the wrong problem: the file is not
// broken, it was never a config file. These tests pin the replacement.

/// The fragment as shipped, so a change to that file that made it stop
/// being a fragment would fail here rather than silently.
fn shipped_fragment() -> PathBuf {
    repo_root().join("docs/robinhood/launch-policy.toml.example")
}

/// A throwaway copy, for the tests that must prove nothing was written
/// without risking the repo's own file.
fn fragment_copy(dir: &Path) -> PathBuf {
    let path = dir.join("launch-policy.toml.example");
    std::fs::copy(shipped_fragment(), &path).unwrap();
    path
}

#[test]
fn check_config_accepts_the_file_the_daemon_loads() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let out = admin(&[
        "chain-policy-check-config",
        "--config",
        config.to_str().unwrap(),
    ]);
    assert!(out.ok, "{}", out.all());
    assert!(out.all().contains("OK —"), "{}", out.all());
}

/// The bug, end to end: the shipped fragment is named as a fragment,
/// the missing sections are listed, and the parser's `missing field
/// solana` is demoted to a supporting detail rather than being the whole
/// answer.
#[test]
fn check_config_names_a_policy_fragment_as_one() {
    let out = admin(&[
        "chain-policy-check-config",
        "--config",
        shipped_fragment().to_str().unwrap(),
    ]);
    let text = out.all();
    assert!(!out.ok, "a fragment must not exit 0: {text}");
    assert!(text.contains("POLICY FRAGMENT"), "{text}");
    assert!(text.contains("[robinhood.policy]"), "{text}");
    for section in ["solana", "goldcoin", "reserve", "operators", "service"] {
        assert!(
            text.contains(section),
            "the missing section {section} must be named: {text}"
        );
    }
    assert!(
        text.contains("/etc/glc-bridge/config.toml"),
        "it must say what to pass instead: {text}"
    );
}

/// Option 2 of the brief, in the safe direction: the fragment's policy
/// is PREVIEWED — read back in operator units, with the fixed-bucket
/// note and the exact flags that would install it — and nothing is
/// written or edited.
#[test]
fn check_config_previews_the_policy_a_fragment_states() {
    let dir = tempfile::tempdir().unwrap();
    let fragment = fragment_copy(dir.path());
    let before = std::fs::read(&fragment).unwrap();

    let out = admin(&[
        "chain-policy-check-config",
        "--config",
        fragment.to_str().unwrap(),
    ]);
    let text = out.all();
    assert!(text.contains("6%"), "the fee, in operator units: {text}");
    assert!(text.contains("20,000 GLC"), "{text}");
    assert!(text.contains("10,000,000 GLC"), "{text}");
    assert!(
        text.contains("5,000,000 GLC"),
        "the on-chain half of the rolling limit must still be spelled out: {text}"
    );
    assert!(
        text.contains("--fee-bps 600")
            && text.contains("--inbound-per-transfer-limit 2000000000000")
            && text.contains("--outbound-per-transfer-limit 2000000000000")
            && text.contains("--rolling-daily-limit 1000000000000000"),
        "the flags that would install it must be printed: {text}"
    );
    assert!(text.contains("Nothing was written"), "{text}");

    assert_eq!(std::fs::read(&fragment).unwrap(), before, "byte-identical");
    assert!(backups(dir.path()).is_empty());
}

#[test]
fn check_config_porcelain_is_a_stable_contract() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let out = admin(&[
        "chain-policy-check-config",
        "--config",
        config.to_str().unwrap(),
        "--porcelain",
    ]);
    assert!(out.ok, "{}", out.all());
    assert!(out.stdout.contains("kind\tfull-config"), "{}", out.stdout);
    assert!(out.stdout.contains("usable\ttrue"), "{}", out.stdout);

    let out = admin(&[
        "chain-policy-check-config",
        "--config",
        shipped_fragment().to_str().unwrap(),
        "--porcelain",
    ]);
    assert!(!out.ok, "{}", out.all());
    for line in [
        "kind\tpolicy-fragment",
        "usable\tfalse",
        "missing_section\tsolana",
        "fragment_network\trobinhood",
        "fragment_fee_bps\t600",
        "fragment_inbound_per_transfer_limit\t2000000000000",
        "fragment_outbound_per_transfer_limit\t2000000000000",
        "fragment_rolling_daily_limit\t1000000000000000",
    ] {
        assert!(
            out.stdout.contains(line),
            "missing {line:?}: {}",
            out.stdout
        );
    }
}

#[test]
fn check_config_names_a_missing_file_and_a_non_toml_one() {
    let dir = tempfile::tempdir().unwrap();
    let out = admin(&[
        "chain-policy-check-config",
        "--config",
        dir.path().join("nope.toml").to_str().unwrap(),
    ]);
    assert!(!out.ok);
    assert!(out.all().contains("NO SUCH FILE"), "{}", out.all());

    let junk = dir.path().join("notes.txt");
    std::fs::write(&junk, "not [[[ toml = =\n").unwrap();
    let out = admin(&[
        "chain-policy-check-config",
        "--config",
        junk.to_str().unwrap(),
    ]);
    assert!(!out.ok);
    assert!(out.all().contains("NOT TOML"), "{}", out.all());
}

/// The other three commands stop guessing too: a fragment is named as a
/// fragment by each of them, not reported as a missing field.
#[test]
fn every_chain_policy_command_names_a_fragment_rather_than_a_missing_field() {
    let dir = tempfile::tempdir().unwrap();
    let fragment = fragment_copy(dir.path());
    let path = fragment.to_str().unwrap();
    let values = [
        "--fee-bps",
        "600",
        "--per-transfer-limit",
        "2000000000000",
        "--rolling-daily-limit",
        "1000000000000000",
    ];

    let mut invocations: Vec<Vec<&str>> = vec![
        vec![
            "chain-policy-show",
            "--config",
            path,
            "--network",
            "robinhood",
        ],
        vec![
            "chain-policy-validate",
            "--config",
            path,
            "--network",
            "robinhood",
        ],
        vec![
            "chain-policy-apply",
            "--config",
            path,
            "--network",
            "robinhood",
            "--note",
            "should never get this far",
            "--execute",
        ],
    ];
    for invocation in invocations.iter_mut().skip(1) {
        invocation.extend_from_slice(&values);
    }

    for invocation in &invocations {
        let out = admin(invocation);
        let text = out.all();
        assert!(!out.ok, "{invocation:?} must be refused: {text}");
        assert!(
            text.contains("POLICY FRAGMENT"),
            "{invocation:?} must name the fragment: {text}"
        );
        assert!(
            text.contains("is not usable as a bridge config"),
            "{invocation:?}: {text}"
        );
    }

    // Including the one that was allowed to write.
    assert!(backups(dir.path()).is_empty(), "nothing was backed up");
    assert_eq!(
        std::fs::read_to_string(&fragment).unwrap(),
        std::fs::read_to_string(shipped_fragment()).unwrap(),
        "the fragment must be byte-identical"
    );
}

// ---------------------------------------------------------------------
// ...and the interactive manager checks before it draws anything
// ---------------------------------------------------------------------

/// No menu is ever drawn over an unusable config, so an operator cannot
/// collect the same parse error once per action.
#[test]
fn the_script_refuses_a_fragment_before_it_draws_a_menu() {
    let dir = tempfile::tempdir().unwrap();
    let fragment = fragment_copy(dir.path());
    // EOF immediately: the operator has no better path to offer.
    let out = script(&fragment, "");
    let text = out.all();

    assert!(!out.ok, "the script must not exit 0 here: {text}");
    assert!(text.contains("POLICY FRAGMENT"), "{text}");
    assert!(
        !text.contains("Select network:"),
        "the menu must never appear over an unusable config: {text}"
    );
    assert!(
        text.contains("Path to full bridge config.toml"),
        "the re-prompt must name what is wanted: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(&fragment).unwrap(),
        std::fs::read_to_string(shipped_fragment()).unwrap()
    );
}

/// Given the right path at the re-prompt, the session continues normally
/// — the check is a gate, not a dead end.
#[test]
fn the_script_asks_for_the_full_config_and_then_carries_on() {
    let dir = tempfile::tempdir().unwrap();
    let fragment = fragment_copy(dir.path());
    let config = config_with_policy(dir.path());

    // Re-prompt -> the real config -> Exit.
    let out = script(&fragment, &format!("{}\n3\n", config.display()));
    let text = out.all();

    // Ordering is asserted within ONE stream: the prompts go to stderr so
    // they cannot be swallowed by a redirect, so stdout and stderr
    // interleave on the operator's terminal but not in a captured
    // `stdout + stderr` string.
    let refused_at = out
        .stdout
        .find("POLICY FRAGMENT")
        .unwrap_or_else(|| panic!("the fragment must be named: {text}"));
    let menu_at = out
        .stdout
        .find("Select network:")
        .unwrap_or_else(|| panic!("the menu must appear once the path is right: {text}"));
    assert!(
        refused_at < menu_at,
        "refuse first, draw the menu only after a usable path:\n{text}"
    );
    assert!(
        out.stderr.contains("Path to full bridge config.toml"),
        "the re-prompt must appear: {text}"
    );
    // The good config was reached through the same check, not around it.
    assert!(
        out.stdout
            .matches("Goldcoin Bridge — config file check")
            .count()
            == 2,
        "every candidate path is checked: {text}"
    );
}

/// A path that does not exist at all gets the same treatment, and the
/// prompt asks for the same thing every time.
#[test]
fn a_nonexistent_path_re_prompts_for_the_full_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let missing = dir.path().join("not-there.toml");

    let out = script(&missing, &format!("{}\n3\n", config.display()));
    let text = out.all();
    assert!(text.contains("NO SUCH FILE"), "{text}");
    assert!(text.contains("Path to full bridge config.toml"), "{text}");
    assert!(text.contains("Select network:"), "{text}");
}

// ============================================== per-route fees, end to end ==
//
// The `[fees]` table through the real binary: reading it, changing exactly
// one route, and proving the other three did not move. The unit tests in
// `fees::edit` prove the file mechanics; these prove the COMMANDS wire
// them up the way an operator will actually type them.

fn fee_bps(config: &Path, route: &str) -> Option<u64> {
    let out = admin(&[
        "fees-show",
        "--config",
        config.to_str().unwrap(),
        "--route",
        route,
        "--porcelain",
    ]);
    assert!(out.ok, "fees-show failed: {}", out.all());
    out.stdout.lines().find_map(|line| {
        let mut fields = line.split('\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some("fee"), Some(name), Some(bps)) if name == route => bps.parse().ok(),
            _ => None,
        }
    })
}

/// Every route's rate, as the binary resolves them.
fn all_fees(config: &Path) -> Vec<(String, u64)> {
    let out = admin(&[
        "fees-show",
        "--config",
        config.to_str().unwrap(),
        "--porcelain",
    ]);
    assert!(out.ok, "fees-show failed: {}", out.all());
    out.stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            match (fields.next(), fields.next(), fields.next()) {
                // An unpriced Solana<->Robinhood route reports
                // `unpriced` rather than a number and is not a rate.
                (Some("fee"), Some(name), Some(bps)) => {
                    bps.parse().ok().map(|bps| (name.to_string(), bps))
                }
                _ => None,
            }
        })
        .collect()
}

#[test]
fn fees_show_reports_every_executable_route_and_where_its_rate_came_from() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());

    let out = admin(&["fees-show", "--config", config.to_str().unwrap()]);
    assert!(out.ok, "{}", out.all());
    let text = out.all();

    // This fixture has `[robinhood.policy].fee_bps = 300` and no `[fees]`,
    // so every route resolves through the documented migration fallback.
    assert!(text.contains("GlcToSol"), "{text}");
    assert!(text.contains("SolToGlc"), "{text}");
    assert!(text.contains("GlcToRhn"), "{text}");
    assert!(text.contains("RhnToGlc"), "{text}");
    assert!(text.contains("migration fallback"), "{text}");
    // The two Solana<->Robinhood routes have no rate in force in this
    // fixture: listed as UNPRICED, never as a number, and the way to
    // price one is named.
    assert!(text.contains("SolToRhn"), "{text}");
    assert!(text.contains("UNPRICED"), "{text}");
    assert!(text.contains("SolToRhn and RhnToSol unpriced"), "{text}");
    // And the contract's lack of a fee is stated, because "do I also need
    // a governance transaction?" is the first question a fee change raises.
    assert!(text.contains("contract stores NO fee"), "{text}");
    assert!(text.contains("Nothing was written"), "{text}");
}

#[test]
fn fees_set_is_a_dry_run_by_default_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let before = std::fs::read_to_string(&config).unwrap();

    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "RhnToGlc",
        "--fee-percent",
        "6",
        "--note",
        "raise the inbound Robinhood fee, OPS-2400",
    ]);
    assert!(out.ok, "{}", out.all());
    let text = out.all();
    assert!(text.contains("BEFORE:"), "{text}");
    assert!(text.contains("AFTER:"), "{text}");
    assert!(text.contains("DRY RUN"), "{text}");
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        before,
        "a dry run must not touch the file"
    );
}

#[test]
fn fees_set_changes_exactly_one_route_and_leaves_the_rest_alone() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let before = all_fees(&config);
    assert_eq!(before.len(), 4, "{before:?}");

    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "RhnToGlc",
        "--fee-percent",
        "6",
        "--note",
        "raise the inbound Robinhood fee, OPS-2400",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    assert!(out.all().contains("APPLIED."), "{}", out.all());

    assert_eq!(fee_bps(&config, "RhnToGlc"), Some(600));
    for (route, was) in before {
        if route == "RhnToGlc" {
            continue;
        }
        assert_eq!(
            fee_bps(&config, &route),
            Some(was),
            "{route} must not have moved"
        );
    }
}

#[test]
fn changing_a_robinhood_fee_never_changes_a_solana_fee_and_vice_versa() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());

    // Robinhood up; Solana untouched.
    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "GlcToRhn",
        "--fee-percent",
        "6",
        "--note",
        "OPS-2401",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    assert_eq!(fee_bps(&config, "GlcToRhn"), Some(600));
    assert_eq!(fee_bps(&config, "GlcToSol"), Some(300));
    assert_eq!(fee_bps(&config, "SolToGlc"), Some(300));
    assert_eq!(fee_bps(&config, "RhnToGlc"), Some(300));

    // Solana down; Robinhood untouched.
    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "GlcToSol",
        "--fee-percent",
        "1",
        "--note",
        "OPS-2402",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    assert_eq!(fee_bps(&config, "GlcToSol"), Some(100));
    assert_eq!(fee_bps(&config, "GlcToRhn"), Some(600));
    assert_eq!(fee_bps(&config, "RhnToGlc"), Some(300));
    assert_eq!(fee_bps(&config, "SolToGlc"), Some(300));
}

#[test]
fn the_first_fees_set_creates_a_complete_section_and_says_which_keys_it_seeded() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());

    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "RhnToGlc",
        "--fee-percent",
        "6",
        "--note",
        "OPS-2403",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    let text = out.all();
    assert!(text.contains("had no [fees] section"), "{text}");
    assert!(text.contains("rates ALREADY IN FORCE"), "{text}");

    let file = std::fs::read_to_string(&config).unwrap();
    assert!(file.contains("[fees]"), "{file}");
    for route in ["GlcToSol", "SolToGlc", "GlcToRhn", "RhnToGlc"] {
        assert!(file.contains(route), "{route} missing from {file}");
    }
    // The operator's comment and unrelated sections survive.
    assert!(file.contains("[operators]"), "{file}");
    assert!(file.contains("[robinhood.policy]"), "{file}");
}

#[test]
fn fees_set_prices_a_cross_route_for_the_first_time_without_enabling_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let before = all_fees(&config);
    assert_eq!(before.len(), 4, "{before:?}");

    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "SolToRhn",
        "--fee-percent",
        "6",
        "--note",
        "price the Solana->Robinhood route, OPS-2500",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    assert!(out.all().contains("APPLIED."), "{}", out.all());
    assert!(
        out.all()
            .contains("NONE — this route has no rate in force yet"),
        "{}",
        out.all()
    );

    assert_eq!(fee_bps(&config, "SolToRhn"), Some(600));
    assert_eq!(
        fee_bps(&config, "RhnToSol"),
        None,
        "the twin stays unpriced"
    );
    for (route, was) in before {
        assert_eq!(
            fee_bps(&config, &route),
            Some(was),
            "{route} must not have moved"
        );
    }
    // Pricing is not enablement: the file still names no enabled route.
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(!text.contains("sol_to_rhn_enabled = true"), "{text}");
}

#[test]
fn fees_set_refuses_an_invalid_rate_without_touching_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());
    let before = std::fs::read_to_string(&config).unwrap();

    for (value, expect) in [
        // 100% and above: nothing delivered, or a negative net.
        ("100", "deliver nothing"),
        ("101", "deliver nothing"),
        // Malformed input never reaches the range check at all.
        ("-3", "fee percentage"),
        ("abc", "fee percentage"),
        ("", "fee percentage"),
        ("3.14159", "fee percentage"),
    ] {
        let out = admin(&[
            "fees-set",
            "--config",
            config.to_str().unwrap(),
            "--route",
            "RhnToGlc",
            "--fee-percent",
            value,
            "--note",
            "should never apply",
            "--execute",
        ]);
        assert!(
            !out.ok,
            "--fee-percent {value} must be refused: {}",
            out.all()
        );
        assert!(
            out.all().contains(expect),
            "--fee-percent {value}: expected {expect:?} in {}",
            out.all()
        );
    }
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
}

#[test]
fn fees_set_requires_a_note() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());

    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "RhnToGlc",
        "--fee-percent",
        "6",
        "--execute",
    ]);
    assert!(!out.ok, "{}", out.all());
    assert!(out.all().contains("--note is required"), "{}", out.all());
}

#[test]
fn fees_set_takes_basis_points_as_well_as_a_percentage() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());

    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "RhnToGlc",
        "--fee-bps",
        "600",
        "--note",
        "OPS-2404",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    assert_eq!(fee_bps(&config, "RhnToGlc"), Some(600));

    // The two flags are two ways to say the same thing.
    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "RhnToGlc",
        "--fee-bps",
        "300",
        "--fee-percent",
        "3",
        "--note",
        "OPS-2405",
        "--execute",
    ]);
    assert!(!out.ok, "{}", out.all());
    assert!(out.all().contains("pass exactly one"), "{}", out.all());
}

#[test]
fn fees_set_never_restarts_anything_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());

    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "GlcToRhn",
        "--fee-percent",
        "6",
        "--note",
        "OPS-2406",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    let text = out.all();
    assert!(text.contains("has NOT been restarted"), "{text}");
    assert!(text.contains("In-flight requests are unaffected"), "{text}");
    assert!(text.contains("nothing on chain to reconcile"), "{text}");
    assert!(text.contains("Backup:"), "{text}");
}

#[test]
fn a_policy_fragment_is_refused_by_the_fee_commands_too() {
    // The same classification `chain-policy-*` applies: a documentation
    // snippet is named as one rather than producing "missing field solana".
    let dir = tempfile::tempdir().unwrap();
    let fragment = dir.path().join("launch-policy.toml.example");
    std::fs::write(
        &fragment,
        "[robinhood.policy]\nfee_bps = 600\nper_transfer_limit = 2000000000000\n\
         rolling_daily_limit = 1000000000000000\n",
    )
    .unwrap();

    for args in [
        vec!["fees-show", "--config", fragment.to_str().unwrap()],
        vec![
            "fees-set",
            "--config",
            fragment.to_str().unwrap(),
            "--route",
            "RhnToGlc",
            "--fee-percent",
            "6",
            "--note",
            "n",
        ],
    ] {
        let out = admin(&args);
        assert!(!out.ok, "{}", out.all());
        assert!(out.all().contains("POLICY FRAGMENT"), "{}", out.all());
    }
}

/// The runbook's own example, executed end to end through the real binary:
/// **`RhnToGlc` 6% -> 4%**, with the other three routes proven unmoved.
///
/// 4% (400 bps) is the case that used to be refused outright for the sole
/// reason that no release had ever shipped it. It is now an ordinary
/// config change, and this test is what says so.
#[test]
fn the_runbook_example_changing_rhn_to_glc_from_six_to_four_percent() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());

    // Start from the launch shape: both Robinhood routes at 6%, both
    // Solana routes at the fixture's 3%.
    for route in ["GlcToRhn", "RhnToGlc"] {
        let out = admin(&[
            "fees-set",
            "--config",
            config.to_str().unwrap(),
            "--route",
            route,
            "--fee-percent",
            "6",
            "--note",
            "launch rate, OPS-1234",
            "--execute",
        ]);
        assert!(out.ok, "{}", out.all());
    }
    let before = all_fees(&config);
    assert_eq!(fee_bps(&config, "RhnToGlc"), Some(600));

    // The dry run: 6% -> 4%, and every other route re-read from the
    // edited file rather than asserted.
    let dry = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "RhnToGlc",
        "--fee-percent",
        "4",
        "--note",
        "commercial review, OPS-2400",
    ]);
    assert!(dry.ok, "4% must be an ordinary rate: {}", dry.all());
    assert!(dry.all().contains("DRY RUN"), "{}", dry.all());
    assert!(dry.all().contains("(600 bps)"), "{}", dry.all());
    assert!(dry.all().contains("(400 bps)"), "{}", dry.all());
    assert_eq!(
        all_fees(&config),
        before,
        "a dry run must not move a single rate"
    );

    // The apply.
    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "RhnToGlc",
        "--fee-percent",
        "4",
        "--note",
        "commercial review, OPS-2400",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    assert!(out.all().contains("APPLIED."), "{}", out.all());

    assert_eq!(fee_bps(&config, "RhnToGlc"), Some(400));
    assert_eq!(
        fee_bps(&config, "GlcToRhn"),
        Some(600),
        "the other Robinhood direction must be untouched"
    );
    assert_eq!(fee_bps(&config, "GlcToSol"), Some(300));
    assert_eq!(fee_bps(&config, "SolToGlc"), Some(300));

    // And the binary that did it is the binary that was already running:
    // nothing about this required a rebuild, which is exactly what
    // `--fee-percent 4` succeeding proves.
    assert!(
        out.all().contains("has NOT been restarted"),
        "{}",
        out.all()
    );
}

/// A rate nobody has ever charged, chosen to have no special status
/// anywhere: 1.37%.
#[test]
fn an_arbitrary_valid_rate_is_accepted_and_prices_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());

    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "GlcToSol",
        "--fee-percent",
        "1.37",
        "--note",
        "OPS-2500",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    assert_eq!(fee_bps(&config, "GlcToSol"), Some(137));
    // Every other route unmoved.
    assert_eq!(fee_bps(&config, "SolToGlc"), Some(300));
    assert_eq!(fee_bps(&config, "GlcToRhn"), Some(300));
    assert_eq!(fee_bps(&config, "RhnToGlc"), Some(300));
}

/// The two ends of the configurable range, through the CLI.
#[test]
fn zero_and_the_maximum_rate_are_both_settable_and_one_past_the_max_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_policy(dir.path());

    // 0% — a free route.
    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "GlcToRhn",
        "--fee-bps",
        "0",
        "--note",
        "promotional, OPS-2600",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    assert_eq!(fee_bps(&config, "GlcToRhn"), Some(0));

    // 9999 bps — the maximum that still delivers something.
    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "GlcToRhn",
        "--fee-bps",
        "9999",
        "--note",
        "OPS-2601",
        "--execute",
    ]);
    assert!(out.ok, "{}", out.all());
    assert_eq!(fee_bps(&config, "GlcToRhn"), Some(9_999));

    // 10000 bps — refused, and the route keeps the rate it had.
    let out = admin(&[
        "fees-set",
        "--config",
        config.to_str().unwrap(),
        "--route",
        "GlcToRhn",
        "--fee-bps",
        "10000",
        "--note",
        "OPS-2602",
        "--execute",
    ]);
    assert!(!out.ok, "{}", out.all());
    assert!(out.all().contains("deliver nothing"), "{}", out.all());
    assert_eq!(fee_bps(&config, "GlcToRhn"), Some(9_999));
}
