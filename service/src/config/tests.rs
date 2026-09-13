use super::*;

fn write(dir: &std::path::Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn solana_keypair_file(dir: &std::path::Path, name: &str) -> (PathBuf, Keypair) {
    let keypair = Keypair::new();
    let json = serde_json::to_string(&keypair.to_bytes().to_vec()).unwrap();
    (write(dir, name, &json), keypair)
}

fn vault_key_file(dir: &std::path::Path, name: &str) -> (PathBuf, [u8; 33]) {
    let secret_key = libsecp256k1::SecretKey::random(&mut rand::rngs::OsRng);
    let pubkey = libsecp256k1::PublicKey::from_secret_key(&secret_key).serialize_compressed();
    let hex = crate::goldcoin::hex::encode(&secret_key.serialize());
    (write(dir, name, &hex), pubkey)
}

/// A complete, valid config file plus the key files it references, all
/// written into `dir`. Returns the config file's path.
pub(crate) fn valid_config(dir: &std::path::Path) -> PathBuf {
    let (a1_path, a1) = solana_keypair_file(dir, "attest1.json");
    let (a2_path, a2) = solana_keypair_file(dir, "attest2.json");
    let (a3_path, a3) = solana_keypair_file(dir, "attest3.json");
    let (v1_path, v1) = vault_key_file(dir, "vault1.hex");
    let (v2_path, v2) = vault_key_file(dir, "vault2.hex");
    let (v3_path, v3) = vault_key_file(dir, "vault3.hex");
    let (sub_path, _sub) = solana_keypair_file(dir, "submitter.json");
    let admin = Keypair::new().pubkey();
    let mint = Keypair::new().pubkey();

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

[service]
db_path = "/tmp/does-not-need-to-exist-for-config-loading/ledger.sqlite3"
tick_interval_ms = 5000
health_bind_addr = "127.0.0.1:9100"
reservation_ttl_secs = 3600
"#,
        mint = mint,
        admin = admin,
        a1 = a1.pubkey(),
        a2 = a2.pubkey(),
        a3 = a3.pubkey(),
        a1_path = a1_path.display(),
        a2_path = a2_path.display(),
        a3_path = a3_path.display(),
        v1 = crate::goldcoin::hex::encode(&v1),
        v2 = crate::goldcoin::hex::encode(&v2),
        v3 = crate::goldcoin::hex::encode(&v3),
        v1_path = v1_path.display(),
        v2_path = v2_path.display(),
        v3_path = v3_path.display(),
        sub_path = sub_path.display(),
    );
    write(dir, "config.toml", &toml)
}

#[test]
fn loads_a_well_formed_config_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();

    assert_eq!(
        config.goldcoin.network,
        crate::goldcoin::address::Network::Testnet
    );
    assert_eq!(config.operators.attestation_threshold, 2);
    assert_eq!(config.operators.attestation_pubkeys.len(), 3);
    assert_eq!(config.operators.vault_threshold, 2);
    assert_eq!(config.operators.vault_pubkeys.len(), 3);
    assert_eq!(
        config.service.health_bind_addr,
        "127.0.0.1:9100".parse().unwrap()
    );

    // Key files load and cross-validate cleanly against the declared
    // pubkeys.
    let signers = config.load_attestation_signers().unwrap();
    assert_eq!(signers.len(), 3);
    for (signer, expected) in signers.iter().zip(&config.operators.attestation_pubkeys) {
        assert_eq!(signer.pubkey(), *expected);
    }
    let vault_signers = config.load_vault_signers().unwrap();
    assert_eq!(vault_signers.len(), 3);
    for (signer, expected) in vault_signers.iter().zip(&config.operators.vault_pubkeys) {
        assert_eq!(signer.pubkey, *expected);
    }
    config.load_submitter().unwrap();
}

#[test]
fn missing_file_fails_closed_with_a_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = Config::load(&dir.path().join("nonexistent.toml")).unwrap_err();
    assert!(matches!(err, ConfigError::Read { .. }));
}

#[test]
fn malformed_toml_fails_closed_with_a_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "config.toml", "this is not valid toml {{{");
    let err = Config::load(&path).unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }));
}

#[test]
fn non_finalized_commitment_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(r#"commitment = "finalized""#, r#"commitment = "confirmed""#);
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "solana.commitment"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn mainnet_goldcoin_network_is_accepted() {
    // goldcoin::address now has real, verified mainnet base58check
    // version bytes (docs/16-p0-checkpoint.md) — "mainnet" must load
    // cleanly and resolve to the real mainnet Network variant, not be
    // rejected the way it was before those bytes existed.
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(r#"network = "regtest""#, r#"network = "mainnet""#);
    std::fs::write(&path, text).unwrap();

    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.goldcoin.network,
        crate::goldcoin::address::Network::Mainnet
    );
}

#[test]
fn testnet_goldcoin_network_resolves_to_the_same_bytes_as_regtest() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(r#"network = "regtest""#, r#"network = "testnet""#);
    std::fs::write(&path, text).unwrap();

    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.goldcoin.network,
        crate::goldcoin::address::Network::Testnet
    );
}

#[test]
fn unrecognized_goldcoin_network_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(r#"network = "regtest""#, r#"network = "moonnet""#);
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "goldcoin.network"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn attestation_threshold_above_pubkey_count_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace("attestation_threshold = 2", "attestation_threshold = 5");
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => {
            assert_eq!(field, "operators.attestation_threshold")
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn zero_vault_threshold_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace("vault_threshold = 2", "vault_threshold = 0");
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "operators.vault_threshold"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn critical_reserve_not_exceeding_protected_minimum_is_rejected() {
    // Mirrors Ledger::configure_reserve's own assertion — must be caught
    // here, before anything ever reaches the ledger.
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(
        "[reserve.solana]\nprotected_minimum = 0",
        "[reserve.solana]\nprotected_minimum = 20000000000",
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "reserve.solana"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn malformed_pubkey_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(
        r#"reserve_token_mint = ""#,
        r#"reserve_token_mint = "not-a-real-pubkey-XXXXX"#,
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "solana.reserve_token_mint"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn key_file_pubkey_mismatch_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());

    // Overwrite the first attestation key file with an entirely different
    // keypair, so it no longer matches the pubkey the config declares at
    // that position.
    let attest1 = dir.path().join("attest1.json");
    let other = Keypair::new();
    std::fs::write(
        &attest1,
        serde_json::to_string(&other.to_bytes().to_vec()).unwrap(),
    )
    .unwrap();

    let config = Config::load(&path).unwrap(); // parsing/validation itself doesn't touch key files
    let Err(err) = config.load_attestation_signers() else {
        panic!("expected a KeyMismatch error");
    };
    assert!(matches!(err, ConfigError::KeyMismatch { .. }));
}

#[test]
fn vault_key_file_pubkey_mismatch_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());

    let vault1 = dir.path().join("vault1.hex");
    let other = libsecp256k1::SecretKey::random(&mut rand::rngs::OsRng);
    std::fs::write(&vault1, crate::goldcoin::hex::encode(&other.serialize())).unwrap();

    let config = Config::load(&path).unwrap();
    let Err(err) = config.load_vault_signers() else {
        panic!("expected a KeyMismatch error");
    };
    assert!(matches!(err, ConfigError::KeyMismatch { .. }));
}

#[test]
fn missing_key_file_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    std::fs::remove_file(dir.path().join("attest1.json")).unwrap();

    let config = Config::load(&path).unwrap();
    let Err(err) = config.load_attestation_signers() else {
        panic!("expected a KeyFileRead error");
    };
    assert!(matches!(err, ConfigError::KeyFileRead { .. }));
}

#[test]
fn env_overrides_take_precedence_over_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());

    // SAFETY: this process-wide env mutation is scoped to this single
    // test and cleaned up before returning; `cargo test`'s default
    // single-process-many-threads model means a parallel test reading
    // unrelated env vars is unaffected, but tests touching the SAME var
    // must not run concurrently — there are none of those here.
    unsafe {
        std::env::set_var("GLC_BRIDGE_SOLANA_RPC_URL", "http://example.invalid:9999");
    }
    let config = Config::load(&path);
    unsafe {
        std::env::remove_var("GLC_BRIDGE_SOLANA_RPC_URL");
    }

    assert_eq!(
        config.unwrap().solana.rpc_url,
        "http://example.invalid:9999"
    );
}

#[test]
fn omitting_the_alert_webhook_url_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();
    assert_eq!(config.service.alert_webhook_url, None);
}

#[test]
fn a_valid_alert_webhook_url_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(
        "reservation_ttl_secs = 3600",
        "reservation_ttl_secs = 3600\nalert_webhook_url = \"https://hooks.example.com/glc-bridge\"",
    );
    std::fs::write(&path, text).unwrap();

    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.service.alert_webhook_url,
        Some("https://hooks.example.com/glc-bridge".to_string())
    );
}

#[test]
fn a_malformed_alert_webhook_url_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(
        "reservation_ttl_secs = 3600",
        "reservation_ttl_secs = 3600\nalert_webhook_url = \"not a url\"",
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "service.alert_webhook_url"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

// ------------------------------------------------------- signer mode --

#[test]
fn unrecognized_signer_mode_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace("[operators]", "[operators]\nmode = \"bogus\"");
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "operators.mode"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn omitting_mode_defaults_to_dev() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();
    assert_eq!(config.operators.mode, SignerMode::Dev);
}

#[test]
fn production_mode_refuses_to_start_with_local_attestation_key_paths_configured() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    // Production mode, but attestation_key_paths (local plaintext dev
    // signer files) are still populated — must refuse to start, even
    // though no attestation_remote_signers were added either (the
    // key-paths check fires first).
    let text = text.replace("[operators]", "[operators]\nmode = \"production\"");
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::ProductionModeForbidsLocalSigners { field } => {
            assert_eq!(field, "operators.attestation_key_paths")
        }
        other => panic!("expected ProductionModeForbidsLocalSigners, got {other:?}"),
    }
}

#[test]
fn production_mode_refuses_to_start_with_local_vault_key_paths_configured() {
    // Attestation side is fully valid production config (remote signers,
    // no key paths) so that check passes cleanly — proving the
    // vault-side guard is independently enforced, not just a duplicate
    // of the attestation-side one.
    let dir2 = tempfile::tempdir().unwrap();
    let (a1_path, a1) = solana_keypair_file(dir2.path(), "attest1.json");
    let (a2_path, a2) = solana_keypair_file(dir2.path(), "attest2.json");
    let (a3_path, a3) = solana_keypair_file(dir2.path(), "attest3.json");
    let (v1_path, v1) = vault_key_file(dir2.path(), "vault1.hex");
    let (v2_path, v2) = vault_key_file(dir2.path(), "vault2.hex");
    let (v3_path, v3) = vault_key_file(dir2.path(), "vault3.hex");
    let (sub_path, _sub) = solana_keypair_file(dir2.path(), "submitter.json");
    let admin = Keypair::new().pubkey();
    let mint = Keypair::new().pubkey();
    let toml = format!(
        r#"
[solana]
rpc_url = "http://127.0.0.1:8899"
commitment = "finalized"
reserve_token_mint = "{mint}"

[goldcoin]
network = "regtest"
rpc_url = "http://127.0.0.1:18332"
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
mode = "production"
admin_pubkey = "{admin}"
attestation_threshold = 2
attestation_pubkeys = ["{a1}", "{a2}", "{a3}"]
attestation_remote_signers = [
  {{ endpoint_url = "https://a1.example.com", expected_public_key = "{a1}", auth_token_env = "UNSET_A1" }},
  {{ endpoint_url = "https://a2.example.com", expected_public_key = "{a2}", auth_token_env = "UNSET_A2" }},
  {{ endpoint_url = "https://a3.example.com", expected_public_key = "{a3}", auth_token_env = "UNSET_A3" }},
]
vault_threshold = 2
vault_pubkeys = ["{v1}", "{v2}", "{v3}"]
vault_key_paths = ["{v1_path}", "{v2_path}", "{v3_path}"]
submitter_key_path = "{sub_path}"

[service]
db_path = "/tmp/does-not-need-to-exist-for-config-loading/ledger.sqlite3"
tick_interval_ms = 5000
health_bind_addr = "127.0.0.1:9100"
reservation_ttl_secs = 3600
"#,
        mint = mint,
        admin = admin,
        a1 = a1.pubkey(),
        a2 = a2.pubkey(),
        a3 = a3.pubkey(),
        v1 = crate::goldcoin::hex::encode(&v1),
        v2 = crate::goldcoin::hex::encode(&v2),
        v3 = crate::goldcoin::hex::encode(&v3),
        v1_path = v1_path.display(),
        v2_path = v2_path.display(),
        v3_path = v3_path.display(),
        sub_path = sub_path.display(),
    );
    let _ = (a1_path, a2_path, a3_path);
    let path2 = write(dir2.path(), "config.toml", &toml);

    let err = Config::load(&path2).unwrap_err();
    match err {
        ConfigError::ProductionModeForbidsLocalSigners { field } => {
            assert_eq!(field, "operators.vault_key_paths")
        }
        other => panic!("expected ProductionModeForbidsLocalSigners, got {other:?}"),
    }
}

#[test]
fn dev_mode_refuses_to_start_with_remote_signers_configured() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let admin_pubkey_line_pos = text.find("attestation_pubkeys").unwrap();
    let (head, tail) = text.split_at(admin_pubkey_line_pos);
    let text = format!(
        "{head}attestation_remote_signers = [{{ endpoint_url = \"https://x.example.com\", \
         expected_public_key = \"11111111111111111111111111111111111111111\", \
         auth_token_env = \"UNSET\" }}]\n{tail}"
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::DevModeForbidsRemoteSigners { field } => {
            assert_eq!(field, "operators.attestation_remote_signers")
        }
        other => panic!("expected DevModeForbidsRemoteSigners, got {other:?}"),
    }
}

/// A complete, valid PRODUCTION-mode config: 3 attestation + 3 vault
/// remote-signer endpoints (threshold 2 of 3 each, matching constraint
/// 9's exact production shape), pointed at `attestation_urls`/
/// `vault_urls` in order. No local key files/paths anywhere.
#[allow(clippy::too_many_arguments)]
fn production_config(
    dir: &std::path::Path,
    attestation_pubkeys: [Pubkey; 3],
    attestation_urls: [String; 3],
    vault_pubkeys: [[u8; 33]; 3],
    vault_urls: [String; 3],
) -> PathBuf {
    let (sub_path, _sub) = solana_keypair_file(dir, "submitter.json");
    let admin = Keypair::new().pubkey();
    let mint = Keypair::new().pubkey();
    let [a1, a2, a3] = attestation_pubkeys;
    let [au1, au2, au3] = attestation_urls;
    let [v1, v2, v3] = vault_pubkeys;
    let [vu1, vu2, vu3] = vault_urls;
    let toml = format!(
        r#"
[solana]
rpc_url = "http://127.0.0.1:8899"
commitment = "finalized"
reserve_token_mint = "{mint}"

[goldcoin]
network = "regtest"
rpc_url = "http://127.0.0.1:18332"
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
mode = "production"
admin_pubkey = "{admin}"
attestation_threshold = 2
attestation_pubkeys = ["{a1}", "{a2}", "{a3}"]
attestation_remote_signers = [
  {{ endpoint_url = "{au1}", expected_public_key = "{a1}", auth_token_env = "GLC_TEST_CFG_A1" }},
  {{ endpoint_url = "{au2}", expected_public_key = "{a2}", auth_token_env = "GLC_TEST_CFG_A2" }},
  {{ endpoint_url = "{au3}", expected_public_key = "{a3}", auth_token_env = "GLC_TEST_CFG_A3" }},
]
vault_threshold = 2
vault_pubkeys = ["{v1}", "{v2}", "{v3}"]
vault_remote_signers = [
  {{ endpoint_url = "{vu1}", expected_public_key = "{v1}", auth_token_env = "GLC_TEST_CFG_V1" }},
  {{ endpoint_url = "{vu2}", expected_public_key = "{v2}", auth_token_env = "GLC_TEST_CFG_V2" }},
  {{ endpoint_url = "{vu3}", expected_public_key = "{v3}", auth_token_env = "GLC_TEST_CFG_V3" }},
]
submitter_key_path = "{sub_path}"

[service]
db_path = "/tmp/does-not-need-to-exist-for-config-loading/ledger.sqlite3"
tick_interval_ms = 5000
health_bind_addr = "127.0.0.1:9100"
reservation_ttl_secs = 3600
"#,
        mint = mint,
        admin = admin,
        a1 = a1,
        a2 = a2,
        a3 = a3,
        v1 = crate::goldcoin::hex::encode(&v1),
        v2 = crate::goldcoin::hex::encode(&v2),
        v3 = crate::goldcoin::hex::encode(&v3),
        sub_path = sub_path.display(),
    );
    write(dir, "config.toml", &toml)
}

fn https_placeholder(n: u8) -> String {
    format!("https://signer-{n}.example.com")
}

#[test]
fn remote_signer_count_mismatch_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let path = production_config(
        dir.path(),
        a,
        [
            https_placeholder(1),
            https_placeholder(2),
            https_placeholder(3),
        ],
        v,
        [
            https_placeholder(4),
            https_placeholder(5),
            https_placeholder(6),
        ],
    );
    // Drop one attestation_remote_signers entry, leaving 3 pubkeys but
    // only 2 endpoints.
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(
        &format!(
            "  {{ endpoint_url = \"{}\", expected_public_key = \"{}\", auth_token_env = \"GLC_TEST_CFG_A3\" }},\n",
            https_placeholder(3),
            a[2]
        ),
        "",
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::RemoteSignerCountMismatch {
            field,
            expected,
            actual,
            ..
        } => {
            assert_eq!(field, "operators.attestation_remote_signers");
            assert_eq!(expected, 3);
            assert_eq!(actual, 2);
        }
        other => panic!("expected RemoteSignerCountMismatch, got {other:?}"),
    }
}

#[test]
fn remote_signer_expected_key_mismatch_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let path = production_config(
        dir.path(),
        a,
        [
            https_placeholder(1),
            https_placeholder(2),
            https_placeholder(3),
        ],
        v,
        [
            https_placeholder(4),
            https_placeholder(5),
            https_placeholder(6),
        ],
    );
    // Corrupt one vault_remote_signers entry's expected_public_key so it
    // no longer matches the positionally-corresponding vault_pubkeys
    // entry — a copy/paste-style config error. Targets ONLY the
    // `expected_public_key = "..."` occurrence (not the `vault_pubkeys`
    // array entry, which shares the same hex substring) — a naive
    // whole-file string replace on the hex value alone would corrupt
    // both identically and never produce an actual mismatch.
    let text = std::fs::read_to_string(&path).unwrap();
    let v2_hex = crate::goldcoin::hex::encode(&v[1]);
    let wrong = crate::goldcoin::hex::encode(&[9u8; 33]);
    let text = text.replace(
        &format!("expected_public_key = \"{v2_hex}\""),
        &format!("expected_public_key = \"{wrong}\""),
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::RemoteSignerExpectedKeyMismatch {
            field,
            pubkeys_field,
            ..
        } => {
            assert_eq!(field, "operators.vault_remote_signers");
            assert_eq!(pubkeys_field, "operators.vault_pubkeys");
        }
        other => panic!("expected RemoteSignerExpectedKeyMismatch, got {other:?}"),
    }
}

/// Production-mode config resolution preserves the same threshold shape
/// dev mode does — proving threshold enforcement is a property of
/// `attestation_threshold`/`vault_threshold` themselves (used later, at
/// signing time, by the same code regardless of which loader produced
/// the signers — `Orchestrator` never sees `Config` at all, only the
/// already-boxed trait objects and these two numbers), not something
/// that depends on which signer-loading path was used. The live
/// network call itself (real connect, real signing, real local
/// signature verification, every error-mapping case) is exhaustively
/// covered in `signing::remote::tests` already — this test's job is
/// specifically the config-resolution wiring, not re-proving the
/// network layer.
#[test]
fn production_mode_resolves_remote_signers_and_preserves_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let attestation_urls = [
        https_placeholder(1),
        https_placeholder(2),
        https_placeholder(3),
    ];
    let vault_urls = [
        https_placeholder(4),
        https_placeholder(5),
        https_placeholder(6),
    ];
    let path = production_config(
        dir.path(),
        a,
        attestation_urls.clone(),
        v,
        vault_urls.clone(),
    );

    let config = Config::load(&path).unwrap();
    assert_eq!(config.operators.mode, SignerMode::Production);
    // Threshold fields themselves are completely unaffected by mode —
    // same values, same validation (attestation_threshold_above_pubkey_
    // count_is_rejected/zero_vault_threshold_is_rejected already prove
    // the validation logic itself is mode-independent, since resolve()
    // checks thresholds before it ever branches on mode).
    assert_eq!(config.operators.attestation_threshold, 2);
    assert_eq!(config.operators.vault_threshold, 2);
    assert_eq!(config.operators.attestation_pubkeys.len(), 3);
    assert_eq!(config.operators.vault_pubkeys.len(), 3);

    assert!(config.operators.attestation_key_paths.is_empty());
    assert!(config.operators.vault_key_paths.is_empty());
    assert_eq!(config.operators.attestation_remote_signers.len(), 3);
    assert_eq!(config.operators.vault_remote_signers.len(), 3);
    for (resolved, expected_url) in config
        .operators
        .attestation_remote_signers
        .iter()
        .zip(&attestation_urls)
    {
        assert_eq!(&resolved.endpoint_url, expected_url);
        assert_eq!(resolved.timeout, Duration::from_millis(5_000));
    }
    for (resolved, expected_url) in config
        .operators
        .vault_remote_signers
        .iter()
        .zip(&vault_urls)
    {
        assert_eq!(&resolved.endpoint_url, expected_url);
    }
}

/// Two attestation-signer slots claiming the same public key are not two
/// custody domains — they're one, counted twice. This must fail closed
/// even though every individual `RemoteSignerExpectedKeyMismatch` check
/// passes (each entry's `expected_public_key` still agrees with its own
/// positional `attestation_pubkeys` entry).
#[test]
fn duplicate_attestation_pubkey_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let shared = Pubkey::new_unique();
    let a = [shared, shared, Pubkey::new_unique()];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let path = production_config(
        dir.path(),
        a,
        [
            https_placeholder(1),
            https_placeholder(2),
            https_placeholder(3),
        ],
        v,
        [
            https_placeholder(4),
            https_placeholder(5),
            https_placeholder(6),
        ],
    );

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::DuplicateRemoteSignerPubkey {
            field,
            first_index,
            dup_index,
            ..
        } => {
            assert_eq!(field, "operators.attestation_remote_signers");
            assert_eq!(first_index, 0);
            assert_eq!(dup_index, 1);
        }
        other => panic!("expected DuplicateRemoteSignerPubkey, got {other:?}"),
    }
}

/// Same property as `duplicate_attestation_pubkey_fails_closed`, for the
/// Goldcoin vault group.
#[test]
fn duplicate_vault_pubkey_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[7u8; 33], [8u8; 33], [8u8; 33]];
    let path = production_config(
        dir.path(),
        a,
        [
            https_placeholder(1),
            https_placeholder(2),
            https_placeholder(3),
        ],
        v,
        [
            https_placeholder(4),
            https_placeholder(5),
            https_placeholder(6),
        ],
    );

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::DuplicateRemoteSignerPubkey {
            field,
            first_index,
            dup_index,
            ..
        } => {
            assert_eq!(field, "operators.vault_remote_signers");
            assert_eq!(first_index, 1);
            assert_eq!(dup_index, 2);
        }
        other => panic!("expected DuplicateRemoteSignerPubkey, got {other:?}"),
    }
}

/// Distinct keys behind the same `endpoint_url` still share a single
/// network/operational compromise blast radius — reject this in
/// production even though the per-entry pubkey cross-checks all pass.
#[test]
fn duplicate_attestation_endpoint_url_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let shared_url = https_placeholder(1);
    let path = production_config(
        dir.path(),
        a,
        [shared_url.clone(), shared_url, https_placeholder(3)],
        v,
        [
            https_placeholder(4),
            https_placeholder(5),
            https_placeholder(6),
        ],
    );

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::DuplicateRemoteSignerEndpoint {
            field,
            first_index,
            dup_index,
            ..
        } => {
            assert_eq!(field, "operators.attestation_remote_signers");
            assert_eq!(first_index, 0);
            assert_eq!(dup_index, 1);
        }
        other => panic!("expected DuplicateRemoteSignerEndpoint, got {other:?}"),
    }
}

/// Same property as `duplicate_attestation_endpoint_url_fails_closed`,
/// for the Goldcoin vault group.
#[test]
fn duplicate_vault_endpoint_url_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let shared_url = https_placeholder(5);
    let path = production_config(
        dir.path(),
        a,
        [
            https_placeholder(1),
            https_placeholder(2),
            https_placeholder(3),
        ],
        v,
        [https_placeholder(4), shared_url.clone(), shared_url],
    );

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::DuplicateRemoteSignerEndpoint {
            field,
            first_index,
            dup_index,
            ..
        } => {
            assert_eq!(field, "operators.vault_remote_signers");
            assert_eq!(first_index, 1);
            assert_eq!(dup_index, 2);
        }
        other => panic!("expected DuplicateRemoteSignerEndpoint, got {other:?}"),
    }
}

// -------------------------------------------- goldcoin initial checkpoint --

/// Injects extra `[goldcoin]`-section lines into [`valid_config`]'s
/// otherwise-valid TOML, just before `[reserve]`.
fn valid_config_with_goldcoin_extra(dir: &std::path::Path, extra: &str) -> PathBuf {
    let path = valid_config(dir);
    let content = std::fs::read_to_string(&path).unwrap();
    let injected = content.replacen("\n[reserve]\n", &format!("\n{extra}\n[reserve]\n"), 1);
    assert_ne!(
        content, injected,
        "injection point ([reserve] section) not found"
    );
    std::fs::write(&path, injected).unwrap();
    path
}

#[test]
fn omitting_the_initial_checkpoint_fields_defaults_to_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();
    assert!(config.goldcoin.initial_checkpoint.is_none());
}

/// A config file predating the UTXO-liquidity fix (none of these 4 fields
/// present — exactly `valid_config`'s base `[goldcoin]` section) must load
/// with the SAFE, verified-floor defaults, not the earlier `8` shown
/// insufficient for the incident's own vault shape
/// (`service/tests/utxo_liquidity_production_tuning.rs::
/// test_prod_defaults_floor_8_breaches_before_backpressure_engages`).
#[test]
fn missing_utxo_liquidity_config_defaults_to_the_verified_safe_floor() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.goldcoin.utxo_pool_min_available_count, 10,
        "the shipped default must be the verified-safe floor (10), not the \
         previously-shipped, since-disproven default of 8"
    );
    assert_eq!(config.goldcoin.utxo_pool_warning_count, 15);
    assert_eq!(
        config.goldcoin.change_fanout_target_atomic, 250_000_000_000,
        "2,500 GLC — the upgrade-safe default for configs omitting the key: a binary upgrade \
         must never silently re-tune an existing deployment's chunk sizing (2026-08-31 M2). \
         Production sets 500000000000 (5,000 GLC) explicitly in the pilot template."
    );
    assert_eq!(config.goldcoin.change_fanout_max_outputs, 10);
    assert_eq!(config.goldcoin.max_auto_resumes_per_tick, 20);
}

/// A config file predating automatic UTXO liquidity shaping (none of the
/// `utxo_shaping_*` fields present) must load with NEW-split shaping
/// DISABLED — autonomous vault self-spends are explicit opt-in, never a
/// silent consequence of a binary upgrade (2026-08-31 M2). The derived
/// sizing defaults still resolve, so flipping the one flag on is the
/// whole migration.
#[test]
fn missing_utxo_shaping_config_defaults_to_disabled_with_derived_sizing() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();
    assert!(
        !config.goldcoin.utxo_shaping_enabled,
        "a config omitting utxo_shaping_enabled must not begin autonomous splits on upgrade"
    );
    assert_eq!(config.goldcoin.utxo_shaping_target_available_count, 15);
    assert_eq!(
        config.goldcoin.utxo_shaping_min_source_atomic,
        4 * config.goldcoin.change_fanout_target_atomic,
        "the split-candidate floor derives from the canonical chunk target"
    );
    assert_eq!(config.goldcoin.utxo_shaping_max_outputs_per_split, 25);
}

/// The explicit production keys (exactly what the pilot template sets)
/// load to the reviewed production behavior: shaping on, 5,000 GLC
/// canonical chunks.
#[test]
fn explicit_production_shaping_keys_load_as_reviewed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_goldcoin_extra(
        dir.path(),
        "utxo_shaping_enabled = true\nchange_fanout_target_atomic = 500000000000\n",
    );
    let config = Config::load(&path).unwrap();
    assert!(config.goldcoin.utxo_shaping_enabled);
    assert_eq!(config.goldcoin.change_fanout_target_atomic, 500_000_000_000);
    assert_eq!(
        config.goldcoin.utxo_shaping_min_source_atomic, 2_000_000_000_000,
        "4x the production chunk target = 20,000 GLC"
    );
}

/// A shaping source floor below two whole chunks could never plan a valid
/// split (`goldcoin::split::plan_split` requires >= 2 chunks) — refused at
/// load time, never an every-tick runtime error.
#[test]
fn utxo_shaping_min_source_below_two_chunks_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_goldcoin_extra(
        dir.path(),
        "utxo_shaping_min_source_atomic = 400000000000\n", // < 2 chunks at the default target
    );
    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => {
            assert_eq!(field, "goldcoin.utxo_shaping_min_source_atomic")
        }
        other => panic!("expected ConfigError::Invalid, got {other:?}"),
    }
}

#[test]
fn a_well_formed_initial_checkpoint_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let hash = "ab".repeat(32);
    let path = valid_config_with_goldcoin_extra(
        dir.path(),
        &format!(
            "initial_checkpoint_height = 2580000\n\
             initial_checkpoint_hash = \"{hash}\"\n\
             initial_checkpoint_operator_acknowledged_no_prior_deposits = true\n"
        ),
    );
    let config = Config::load(&path).unwrap();
    let checkpoint = config.goldcoin.initial_checkpoint.expect("must be Some");
    assert_eq!(checkpoint.height, 2_580_000);
    assert_eq!(checkpoint.hash, hash);
    assert!(checkpoint.operator_acknowledged_no_prior_deposits);
}

#[test]
fn initial_checkpoint_height_without_hash_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_goldcoin_extra(dir.path(), "initial_checkpoint_height = 100\n");
    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => {
            assert_eq!(field, "goldcoin.initial_checkpoint_hash")
        }
        other => panic!("expected ConfigError::Invalid, got {other:?}"),
    }
}

#[test]
fn initial_checkpoint_hash_without_height_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let hash = "cd".repeat(32);
    let path = valid_config_with_goldcoin_extra(
        dir.path(),
        &format!("initial_checkpoint_hash = \"{hash}\"\n"),
    );
    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => {
            assert_eq!(field, "goldcoin.initial_checkpoint_height")
        }
        other => panic!("expected ConfigError::Invalid, got {other:?}"),
    }
}

#[test]
fn initial_checkpoint_negative_height_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let hash = "ef".repeat(32);
    let path = valid_config_with_goldcoin_extra(
        dir.path(),
        &format!(
            "initial_checkpoint_height = -1\n\
             initial_checkpoint_hash = \"{hash}\"\n\
             initial_checkpoint_operator_acknowledged_no_prior_deposits = true\n"
        ),
    );
    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => {
            assert_eq!(field, "goldcoin.initial_checkpoint_height")
        }
        other => panic!("expected ConfigError::Invalid, got {other:?}"),
    }
}

#[test]
fn initial_checkpoint_malformed_hash_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_goldcoin_extra(
        dir.path(),
        "initial_checkpoint_height = 100\n\
         initial_checkpoint_hash = \"not-hex-and-wrong-length\"\n\
         initial_checkpoint_operator_acknowledged_no_prior_deposits = true\n",
    );
    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => {
            assert_eq!(field, "goldcoin.initial_checkpoint_hash")
        }
        other => panic!("expected ConfigError::Invalid, got {other:?}"),
    }
}

#[test]
fn initial_checkpoint_omitted_acknowledgement_defaults_to_false_not_an_error_at_load_time() {
    // Config loading itself only validates STRUCTURE (height/hash
    // well-formed, given together) — the live "does this vault actually
    // have no prior deposits" acknowledgement gate is enforced at indexer
    // bootstrap time (`goldcoin::indexer::bootstrap_from_checkpoint_or_
    // genesis`), not here, since only the indexer has a chain connection
    // to act on a verified checkpoint at all.
    let dir = tempfile::tempdir().unwrap();
    let hash = "12".repeat(32);
    let path = valid_config_with_goldcoin_extra(
        dir.path(),
        &format!(
            "initial_checkpoint_height = 100\n\
             initial_checkpoint_hash = \"{hash}\"\n"
        ),
    );
    let config = Config::load(&path).unwrap();
    let checkpoint = config.goldcoin.initial_checkpoint.expect("must be Some");
    assert!(!checkpoint.operator_acknowledged_no_prior_deposits);
}

// -------------------------------------------------- admin API config --

/// Appends `[service]`-table admin keys to an otherwise-valid config.
/// (The generated `[service]` table is last in `valid_config`'s TOML, so
/// plain appended `key = value` lines land in it.)
fn valid_config_with_admin(dir: &std::path::Path, extra_service_lines: &str) -> PathBuf {
    let path = valid_config(dir);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(extra_service_lines);
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn admin_bind_addr_without_operators_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_admin(dir.path(), "admin_bind_addr = \"127.0.0.1:9102\"\n");
    let err = Config::load(&path).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid { field, .. } if field == "service.admin_operators"),
        "{err}"
    );
}

#[test]
fn admin_operators_without_a_bind_addr_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_admin(
        dir.path(),
        "admin_operators = [{ name = \"alice\", token_env = \"GLC_TEST_ADMIN_TOKEN_ORPHAN\" }]\n",
    );
    let err = Config::load(&path).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid { field, .. } if field == "service.admin_operators"),
        "{err}"
    );
}

/// The regression that broke `glc-admin --config` recovery commands:
/// loading a config that declares admin operators must NOT require their
/// token env vars — the vars exist only in the daemon's environment, and
/// `retry-goldcoin-payout`/`split-vault-utxo` load the same file from
/// operator shells that lack them. Token resolution happens in the
/// daemon via `admin_api::auth::resolve_operator_tokens` instead.
#[test]
fn config_load_never_reads_admin_token_env_vars() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_admin(
        dir.path(),
        "admin_bind_addr = \"127.0.0.1:9102\"\n\
         admin_operators = [\n\
           { name = \"alice\", token_env = \"GLC_TEST_ADMIN_TOKEN_DELIBERATELY_NEVER_SET_A\" },\n\
           { name = \"bob\", token_env = \"GLC_TEST_ADMIN_TOKEN_DELIBERATELY_NEVER_SET_B\" },\n\
         ]\n",
    );
    let config = Config::load(&path).expect(
        "Config::load must succeed with the token env vars unset — glc-admin's --config \
         recovery commands depend on it",
    );
    assert_eq!(
        config.service.admin_bind_addr,
        Some("127.0.0.1:9102".parse().unwrap())
    );
    let declared: Vec<(&str, &str)> = config
        .service
        .admin_operators
        .iter()
        .map(|op| (op.name.as_str(), op.token_env.as_str()))
        .collect();
    assert_eq!(
        declared,
        vec![
            ("alice", "GLC_TEST_ADMIN_TOKEN_DELIBERATELY_NEVER_SET_A"),
            ("bob", "GLC_TEST_ADMIN_TOKEN_DELIBERATELY_NEVER_SET_B"),
        ]
    );
}

#[test]
fn duplicate_admin_token_env_vars_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_admin(
        dir.path(),
        "admin_bind_addr = \"127.0.0.1:9102\"\n\
         admin_operators = [\n\
           { name = \"alice\", token_env = \"GLC_TEST_ADMIN_TOKEN_SHARED\" },\n\
           { name = \"bob\", token_env = \"GLC_TEST_ADMIN_TOKEN_SHARED\" },\n\
         ]\n",
    );
    let err = Config::load(&path).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid { field, .. } if field == "service.admin_operators"),
        "{err}"
    );
    assert!(err.to_string().contains("duplicate token_env"), "{err}");
}

#[test]
fn an_empty_admin_token_env_name_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_admin(
        dir.path(),
        "admin_bind_addr = \"127.0.0.1:9102\"\n\
         admin_operators = [{ name = \"alice\", token_env = \"\" }]\n",
    );
    let err = Config::load(&path).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid { field, .. } if field == "service.admin_operators"),
        "{err}"
    );
}

#[test]
fn duplicate_admin_operator_names_fail_closed() {
    std::env::set_var("GLC_TEST_ADMIN_TOKEN_DUP", "token");
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_admin(
        dir.path(),
        "admin_bind_addr = \"127.0.0.1:9102\"\n\
         admin_operators = [\n\
           { name = \"alice\", token_env = \"GLC_TEST_ADMIN_TOKEN_DUP\" },\n\
           { name = \"alice\", token_env = \"GLC_TEST_ADMIN_TOKEN_DUP\" },\n\
         ]\n",
    );
    let err = Config::load(&path).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid { field, .. } if field == "service.admin_operators"),
        "{err}"
    );
}

// -------------------- confirmed-liquidity admission safety buffer --

/// The production policy numbers live in the shipped defaults, not only in
/// the pilot template: a deployment whose config file predates this
/// feature must come up WITH the protective posture, not without it. See
/// docs/09-runbook.md's "Confirmed-liquidity admission safety buffer".
#[test]
fn missing_admission_buffer_config_defaults_to_the_production_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.goldcoin.admission_safety_buffer_atomic,
        250_000 * 100_000_000,
        "250,000 GLC — the production close threshold"
    );
    assert_eq!(
        config.goldcoin.admission_reopen_headroom_atomic,
        350_000 * 100_000_000,
        "350,000 GLC — the production reopen threshold; the 100,000 GLC gap is the \
         anti-flapping band"
    );
}

/// A reopen threshold below the close threshold cannot express hysteresis
/// at all — the gate would close and immediately reopen on one unchanged
/// headroom, exactly the flapping the buffer exists to prevent. Fail
/// closed at load time.
#[test]
fn an_admission_reopen_threshold_below_the_buffer_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    // Inserted INTO the [goldcoin] table (appending to the file would
    // land the keys in whichever table happens to be last).
    let toml = std::fs::read_to_string(&path).unwrap().replace(
        "max_inputs = 10",
        "max_inputs = 10\nadmission_safety_buffer_atomic = 35000000000000\n\
         admission_reopen_headroom_atomic = 25000000000000",
    );
    std::fs::write(&path, toml).unwrap();
    let err = Config::load(&path).unwrap_err();
    assert!(
        format!("{err}").contains("admission_reopen_headroom_atomic"),
        "got {err}"
    );
}

/// `0` is the documented kill switch, and must load cleanly — an operator
/// disabling the buffer should never have to also satisfy the ordering
/// rule against a meaningless reopen value.
#[test]
fn a_zero_admission_buffer_disables_the_mechanism_and_loads_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let toml = std::fs::read_to_string(&path).unwrap().replace(
        "max_inputs = 10",
        "max_inputs = 10\nadmission_safety_buffer_atomic = 0\n\
         admission_reopen_headroom_atomic = 0",
    );
    std::fs::write(&path, toml).unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.goldcoin.admission_safety_buffer_atomic, 0);
}

// ===================================================================== //
// Robinhood route configuration (Phase 1)                               //
// ===================================================================== //

#[test]
fn a_config_file_with_no_robinhood_section_still_loads() {
    // The backwards-compatibility guarantee, stated as a test: every
    // production config file in existence has no `[robinhood]` section, and
    // must keep loading. `valid_config` writes exactly such a file.
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).expect("an existing config file must keep loading unchanged");

    assert!(config.routes.enabled(crate::routes::Route::GlcToSol));
    assert!(config.routes.enabled(crate::routes::Route::SolToGlc));
    assert!(
        !config.routes.enabled(crate::routes::Route::GlcToRhn),
        "a missing [robinhood] section must mean disabled, never enabled"
    );
    assert!(!config.routes.enabled(crate::routes::Route::RhnToGlc));
}

#[test]
fn an_empty_robinhood_section_is_identical_to_an_absent_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str("\n[robinhood]\n");
    std::fs::write(&path, toml).unwrap();

    let config = Config::load(&path).unwrap();
    assert!(!config.routes.enabled(crate::routes::Route::GlcToRhn));
    assert!(!config.routes.enabled(crate::routes::Route::RhnToGlc));
}

#[test]
fn each_robinhood_flag_is_independent_and_defaults_false() {
    // Setting one flag must not imply the other. A single "robinhood
    // enabled" boolean would have opened both directions at once, which is
    // exactly the shape of mistake directional configuration exists to
    // prevent.
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str("\n[robinhood]\nglc_to_rhn_enabled = true\n");
    std::fs::write(&path, toml).unwrap();

    let config = Config::load(&path).unwrap();
    assert!(config.routes.enabled(crate::routes::Route::GlcToRhn));
    assert!(
        !config.routes.enabled(crate::routes::Route::RhnToGlc),
        "the omitted flag must stay false"
    );
}

#[test]
fn config_alone_cannot_open_a_robinhood_route() {
    // Even with both config flags on, the gate must still refuse: config is
    // one of three independent votes, not the decision.
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str("\n[robinhood]\nglc_to_rhn_enabled = true\nrhn_to_glc_enabled = true\n");
    std::fs::write(&path, toml).unwrap();

    let config = Config::load(&path).unwrap();
    let gate = crate::routes::RouteGate::new(config.routes, crate::chains::ChainRegistry::phase1());
    let ledger = crate::ledger::Ledger::open_in_memory().unwrap();
    for route in [
        crate::routes::Route::GlcToRhn,
        crate::routes::Route::RhnToGlc,
    ] {
        assert!(
            gate.ensure_enabled(&ledger, route).is_err(),
            "{route:?} must stay closed even with config fully enabled"
        );
    }
}

#[test]
fn stray_chain_parameters_on_the_robinhood_section_are_not_picked_up() {
    // Chain parameters now have exactly one home — the explicit
    // `[robinhood.indexer]` section, where every field is mandatory and
    // validated. Loose keys scattered on `[robinhood]` itself are NOT a
    // second, quieter way to configure the chain: serde ignores unknown
    // keys, so this asserts they are carried nowhere.
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str(
        "\n[robinhood]\nglc_to_rhn_enabled = false\nrhn_to_glc_enabled = false\n\
         rpc_url = \"http://example.invalid\"\ntoken_contract = \"0xdeadbeef\"\ndecimals = 18\n",
    );
    std::fs::write(&path, toml).unwrap();

    // Loads (unknown keys are ignored) but carries none of it forward.
    let config = Config::load(&path).unwrap();
    assert!(!config.routes.enabled(crate::routes::Route::GlcToRhn));
    // In particular no indexer was created: only a real
    // `[robinhood.indexer]` section does that.
    assert!(config.robinhood_indexer.is_none());
    // Unknown keys are ignored, and an unrecognised `[robinhood]` shape
    // enables nothing. Phase F resolved every chain parameter the Phase-1
    // checklist listed as unknown — but resolving them did not make a
    // stray config section able to open a route, which is what this test
    // is really about.
    assert!(!crate::chains::robinhood::RESOLVED_CHAIN_PARAMETERS.is_empty());
    // And a successful resolution is still not a token audit: the
    // properties preflight does NOT establish stay recorded.
    assert!(!crate::chains::robinhood::UNVERIFIED_TOKEN_PROPERTIES.is_empty());
}

// ------------------------------------------------ [robinhood.indexer] --

/// The one-line summary of the whole phase's compatibility promise: an
/// existing production config file has no `[robinhood.indexer]` section,
/// so no indexer exists, so no Robinhood endpoint is ever contacted.
#[test]
fn no_robinhood_indexer_section_means_no_indexer_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::load(&valid_config(dir.path())).unwrap();
    assert!(config.robinhood_indexer.is_none());
}

/// And a `[robinhood]` section that only names route flags still creates
/// no indexer — the two are independently configured, because watching a
/// chain and transacting on it are different privileges.
#[test]
fn route_flags_alone_do_not_create_an_indexer() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str("\n[robinhood]\nrhn_to_glc_enabled = true\n");
    std::fs::write(&path, toml).unwrap();

    let config = Config::load(&path).unwrap();
    assert!(config.robinhood_indexer.is_none());
}

fn with_indexer_section(dir: &std::path::Path, section: &str) -> PathBuf {
    let path = valid_config(dir);
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str(section);
    std::fs::write(&path, toml).unwrap();
    path
}

const VALID_INDEXER_SECTION: &str = r#"
[robinhood]
rhn_to_glc_enabled = false

[robinhood.indexer]
rpc_url = "https://rpc.robinhood.invalid"
chain_id = 46630
bridge_contract = "0x1111111111111111111111111111111111111111"
expected_token = "0x2222222222222222222222222222222222222222"
start_block = 1234
confirmation_depth = 12
poll_interval_ms = 5000
request_timeout_ms = 10000
max_log_block_range = 2000
"#;

#[test]
fn a_complete_indexer_section_resolves_every_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = with_indexer_section(dir.path(), VALID_INDEXER_SECTION);
    let config = Config::load(&path).unwrap();

    let indexer = config.robinhood_indexer.expect("indexer is configured");
    assert_eq!(indexer.rpc_url, "https://rpc.robinhood.invalid");
    assert_eq!(
        indexer.chain_id,
        crate::evm::networks::ROBINHOOD_TESTNET_CHAIN_ID
    );
    assert_eq!(indexer.bridge_contract.to_bytes(), [0x11; 20]);
    assert_eq!(indexer.expected_token.to_bytes(), [0x22; 20]);
    assert_eq!(indexer.start_block, 1234);
    assert_eq!(indexer.confirmation_depth, 12);
    assert_eq!(indexer.max_log_block_range, 2000);

    // Configuring the indexer opens nothing: every Robinhood route is
    // still disabled.
    for route in [
        crate::routes::Route::GlcToRhn,
        crate::routes::Route::RhnToGlc,
        crate::routes::Route::SolToRhn,
        crate::routes::Route::RhnToSol,
    ] {
        assert!(
            !config.routes.enabled(route),
            "{route:?} must stay disabled when only the indexer is configured"
        );
    }
}

/// Every field is mandatory. A section that omits one is a parse error,
/// not a section with a guessed default — see
/// `crate::robinhood::config`'s module docs.
#[test]
fn an_indexer_section_missing_any_field_is_refused() {
    for omitted in [
        "rpc_url",
        "chain_id",
        "bridge_contract",
        "expected_token",
        "start_block",
        "confirmation_depth",
        "poll_interval_ms",
        "request_timeout_ms",
        "max_log_block_range",
    ] {
        let section: String = VALID_INDEXER_SECTION
            .lines()
            .filter(|line| !line.starts_with(&format!("{omitted} =")))
            .collect::<Vec<_>>()
            .join("\n");
        let dir = tempfile::tempdir().unwrap();
        let path = with_indexer_section(dir.path(), &section);
        assert!(
            matches!(Config::load(&path), Err(ConfigError::Parse { .. })),
            "omitting {omitted} must be refused rather than defaulted",
        );
    }
}

#[test]
fn indexer_validation_failures_name_the_field_they_concern() {
    let cases = [
        (
            "confirmation_depth = 12",
            "confirmation_depth = 0",
            "robinhood.indexer.confirmation_depth",
        ),
        (
            "max_log_block_range = 2000",
            "max_log_block_range = 0",
            "robinhood.indexer.max_log_block_range",
        ),
        (
            "poll_interval_ms = 5000",
            "poll_interval_ms = 0",
            "robinhood.indexer.poll_interval_ms",
        ),
        (
            "rpc_url = \"https://rpc.robinhood.invalid\"",
            "rpc_url = \"wss://rpc.robinhood.invalid\"",
            "robinhood.indexer.rpc_url",
        ),
        (
            "expected_token = \"0x2222222222222222222222222222222222222222\"",
            "expected_token = \"0x0000000000000000000000000000000000000000\"",
            "robinhood.indexer.expected_token",
        ),
        (
            "bridge_contract = \"0x1111111111111111111111111111111111111111\"",
            "bridge_contract = \"0xnot-an-address\"",
            "robinhood.indexer.bridge_contract",
        ),
        (
            "chain_id = 46630",
            "chain_id = 0",
            "robinhood.indexer.chain_id",
        ),
    ];
    for (from, to, expected_field) in cases {
        let section = VALID_INDEXER_SECTION.replace(from, to);
        let dir = tempfile::tempdir().unwrap();
        let path = with_indexer_section(dir.path(), &section);
        match Config::load(&path) {
            Err(ConfigError::Invalid { field, .. }) => assert_eq!(
                field, expected_field,
                "{to} should be reported against {expected_field}",
            ),
            other => panic!("{to} must be refused, got {other:?}"),
        }
    }
}

// ------------------------------------------------- [robinhood.policy] --

/// The exact launch policy, loaded from a config file end to end.
#[test]
fn the_robinhood_launch_policy_loads_from_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str(
        "\n[robinhood.policy]\n\
         fee_bps = 600\n\
         per_transfer_limit = 2000000000000\n\
         rolling_daily_limit = 1000000000000000\n",
    );
    std::fs::write(&path, toml).unwrap();

    let config = Config::load(&path).expect("the approved launch policy loads");
    let policy = config
        .chain_policies
        .get(crate::routes::Chain::Robinhood)
        .expect("a Robinhood policy");
    assert_eq!(policy.fee_bps(), 600);
    assert_eq!(policy.per_transfer_limit().0, 2_000_000_000_000);
    assert_eq!(policy.rolling_daily_limit().0, 1_000_000_000_000_000);

    // And the on-chain figure it implies is HALF the strict policy.
    let binding =
        crate::robinhood::RobinhoodPolicyBinding::new(*policy).expect("installable on chain");
    assert_eq!(
        binding.expected_onchain_rolling_limit_canonical().0,
        500_000_000_000_000
    );
}

/// The other half of requirement "do not change Goldcoin<->Solana": a
/// configured Robinhood policy must not move the Solana fee.
#[test]
fn a_robinhood_policy_leaves_the_solana_fee_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str(
        "\n[robinhood.policy]\n\
         fee_bps = 600\n\
         per_transfer_limit = 2000000000000\n\
         rolling_daily_limit = 1000000000000000\n",
    );
    std::fs::write(&path, toml).unwrap();

    let config = Config::load(&path).unwrap();
    assert_eq!(
        config
            .chain_policies
            .fee_bps_for(crate::routes::Chain::Solana),
        crate::amount_conversion::BRIDGE_FEE_BPS
    );
    assert_eq!(
        config
            .chain_policies
            .fee_bps_for(crate::routes::Chain::Goldcoin),
        crate::amount_conversion::BRIDGE_FEE_BPS
    );
    assert!(config
        .chain_policies
        .get(crate::routes::Chain::Solana)
        .is_none());
}

/// Every existing production config file has no `[robinhood.policy]`
/// section and must keep loading, with Robinhood pricing exactly as it
/// did before the section existed.
#[test]
fn no_policy_section_means_the_compiled_in_rate_for_every_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();

    assert!(config.chain_policies.is_empty());
    for chain in crate::routes::Chain::ALL {
        assert_eq!(
            config.chain_policies.fee_bps_for(chain),
            crate::amount_conversion::BRIDGE_FEE_BPS
        );
    }
}

/// A policy section is independent of the indexer, the settlement section
/// and the route flags: stating commercial terms is not the same act as
/// observing a chain, settling on it, or opening a route to it.
#[test]
fn a_policy_section_opens_no_route_and_starts_no_indexer() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str(
        "\n[robinhood.policy]\n\
         fee_bps = 600\n\
         per_transfer_limit = 2000000000000\n\
         rolling_daily_limit = 1000000000000000\n",
    );
    std::fs::write(&path, toml).unwrap();

    let config = Config::load(&path).unwrap();
    assert!(!config.routes.enabled(crate::routes::Route::GlcToRhn));
    assert!(!config.routes.enabled(crate::routes::Route::RhnToGlc));
    assert!(config.robinhood_indexer.is_none());
    assert!(config.robinhood_settlement.is_none());
}

/// Strict validation, each failure named separately: a config file that
/// states an impossible policy must be refused at load, not at the first
/// deposit.
#[test]
fn an_invalid_policy_is_refused_at_load() {
    let cases: [(&str, &str, &str); 5] = [
        // 100% fee: every transfer would deliver nothing.
        ("10000", "2000000000000", "1000000000000000"),
        // Above 100%: the net entitlement would be negative.
        ("10001", "2000000000000", "1000000000000000"),
        // Zero per-transfer ceiling.
        ("600", "0", "1000000000000000"),
        // Rolling below per-transfer.
        ("600", "2000000000000", "1999999999999"),
        // Installable as a bare policy, but the implied on-chain rolling
        // limit (half of it) would sit below the per-transfer maximum and
        // `_validateLimits` would revert — refused here rather than at
        // preflight months later.
        ("600", "2000000000000", "3000000000000"),
    ];
    for (fee_bps, per_transfer, rolling) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = valid_config(dir.path());
        let mut toml = std::fs::read_to_string(&path).unwrap();
        toml.push_str(&format!(
            "\n[robinhood.policy]\n\
             fee_bps = {fee_bps}\n\
             per_transfer_limit = {per_transfer}\n\
             rolling_daily_limit = {rolling}\n"
        ));
        std::fs::write(&path, toml).unwrap();

        let err = match Config::load(&path) {
            Err(err) => err,
            Ok(_) => panic!(
                "an invalid policy must be refused: fee_bps={fee_bps} \
                 per_transfer={per_transfer} rolling={rolling}"
            ),
        };
        match err {
            ConfigError::Invalid { field, .. } => assert_eq!(field, "robinhood.policy"),
            other => panic!("expected an Invalid error, got {other:?}"),
        }
    }
}

/// Every field is required — the same discipline every other Robinhood
/// section applies, and for the same reason: a defaulted fee rate would
/// price real money at a number nobody chose.
#[test]
fn a_partial_policy_section_is_refused() {
    for body in [
        "fee_bps = 600\n",
        "per_transfer_limit = 2000000000000\n",
        "fee_bps = 600\nper_transfer_limit = 2000000000000\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = valid_config(dir.path());
        let mut toml = std::fs::read_to_string(&path).unwrap();
        toml.push_str(&format!("\n[robinhood.policy]\n{body}"));
        std::fs::write(&path, toml).unwrap();
        assert!(
            Config::load(&path).is_err(),
            "a partial [robinhood.policy] must be refused: {body}"
        );
    }
}

// ============================================================ [fees] ==
//
// Per-route fees. The two things these must pin, above all: an
// unmodified production config keeps pricing EXACTLY as it did, and a
// `[fees]` section prices each route from its own entry with nothing
// leaking between them.

/// Appends a `[fees]` (or any other) section to an otherwise valid config.
fn valid_config_with_appended(dir: &std::path::Path, extra: &str) -> PathBuf {
    let path = valid_config(dir);
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str(extra);
    std::fs::write(&path, toml).unwrap();
    path
}

#[test]
fn a_config_with_no_fees_section_keeps_todays_economics_exactly() {
    // THE backward-compatibility test. Every production config file in
    // existence has no `[fees]` and no `[robinhood.policy]`; loading one
    // must still start, and must price every route at the rate it was
    // priced at before per-route fees existed — the compiled-in constant.
    use crate::routes::Route;

    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();

    for route in crate::fees::executable_routes().filter(|r| !r.is_solana_robinhood()) {
        assert_eq!(
            config.route_fees.fee_bps(route).unwrap(),
            crate::amount_conversion::BRIDGE_FEE_BPS,
            "{} must keep the pre-existing rate",
            route.as_str()
        );
    }
    // And the table is COMPLETE for every route that REQUIRES a rate —
    // the four that were priced before Phase H. The two Solana<->Robinhood
    // routes had no pre-existing rate, so the fallback prices neither:
    // they stay unpriced (and therefore fold nothing) until an explicit
    // `[fees]` entry names them.
    config
        .route_fees
        .covers_required_routes(&config.routes)
        .unwrap();
    assert!(config.route_fees.get(Route::SolToRhn).is_none());
    assert!(config.route_fees.get(Route::RhnToSol).is_none());
    assert!(config.route_fees.fee_bps(Route::SolToRhn).is_err());
    assert!(config.route_fees.fee_bps(Route::RhnToSol).is_err());
}

#[test]
fn enabling_a_cross_route_without_pricing_it_is_refused_at_startup() {
    // A Solana<->Robinhood route may go unpriced only while it is
    // disabled. Asking to open one without stating its rate is refused —
    // never priced at another route's rate or the compiled-in constant.
    for (flag, route) in [
        ("sol_to_rhn_enabled", "SolToRhn"),
        ("rhn_to_sol_enabled", "RhnToSol"),
    ] {
        // No [fees] at all: the migration fallback has nothing to carry.
        let dir = tempfile::tempdir().unwrap();
        let path =
            valid_config_with_appended(dir.path(), &format!("\n[robinhood]\n{flag} = true\n"));
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains(route), "{err}");
        assert!(err.contains("no fee is configured"), "{err}");

        // A [fees] table that names the four legacy routes but not this one.
        let dir = tempfile::tempdir().unwrap();
        let path = valid_config_with_appended(
            dir.path(),
            &format!(
                "\n[robinhood]\n{flag} = true\n\n[fees]\nGlcToSol = 300\nSolToGlc = 300\n\
                 GlcToRhn = 600\nRhnToGlc = 600\n"
            ),
        );
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains(route), "{err}");
    }
}

#[test]
fn a_priced_cross_route_resolves_its_own_rate_and_only_its_own() {
    use crate::routes::Route;
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_appended(
        dir.path(),
        "\n[fees]\nGlcToSol = 300\nSolToGlc = 300\nGlcToRhn = 600\nRhnToGlc = 600\n\
         SolToRhn = 450\n",
    );
    let config = Config::load(&path).unwrap();
    assert_eq!(config.route_fees.fee_bps(Route::SolToRhn).unwrap(), 450);
    assert!(config.route_fees.get(Route::RhnToSol).is_none());
    // Pricing a disabled cross route enables nothing.
    assert!(!config.routes.enabled(Route::SolToRhn));
    assert_eq!(config.route_fees.fee_bps(Route::GlcToRhn).unwrap(), 600);
    assert_eq!(config.route_fees.fee_bps(Route::SolToGlc).unwrap(), 300);
}

#[test]
fn without_a_fees_section_the_robinhood_routes_inherit_the_chain_policy_rate() {
    // The other half of backward compatibility: a deployment that already
    // configured `[robinhood.policy].fee_bps = 600` was charging 600 on
    // its Robinhood folds, and must keep charging exactly that after the
    // upgrade — while Solana keeps its own, unchanged.
    use crate::routes::Route;

    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_appended(
        dir.path(),
        "\n[robinhood.policy]\nfee_bps = 600\nper_transfer_limit = 2000000000000\n\
         rolling_daily_limit = 1000000000000000\n",
    );
    let config = Config::load(&path).unwrap();

    assert_eq!(config.route_fees.fee_bps(Route::GlcToRhn).unwrap(), 600);
    assert_eq!(config.route_fees.fee_bps(Route::RhnToGlc).unwrap(), 600);
    assert_eq!(
        config.route_fees.fee_bps(Route::GlcToSol).unwrap(),
        crate::amount_conversion::BRIDGE_FEE_BPS,
        "the Robinhood rate must not have reached Solana"
    );
    assert_eq!(
        config.route_fees.fee_bps(Route::SolToGlc).unwrap(),
        crate::amount_conversion::BRIDGE_FEE_BPS
    );
}

#[test]
fn a_fees_section_prices_each_route_from_its_own_entry() {
    use crate::routes::Route;

    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_appended(
        dir.path(),
        "\n[fees]\nGlcToSol = 300\nSolToGlc = 100\nGlcToRhn = 600\nRhnToGlc = 300\n",
    );
    let config = Config::load(&path).unwrap();

    assert_eq!(config.route_fees.fee_bps(Route::GlcToSol).unwrap(), 300);
    assert_eq!(config.route_fees.fee_bps(Route::SolToGlc).unwrap(), 100);
    assert_eq!(config.route_fees.fee_bps(Route::GlcToRhn).unwrap(), 600);
    assert_eq!(config.route_fees.fee_bps(Route::RhnToGlc).unwrap(), 300);
}

#[test]
fn a_fees_section_overrides_the_chain_policy_rate_for_robinhood() {
    // Both sections present. `[fees]` is authoritative for PRICING;
    // `[robinhood.policy]` keeps its meaning for the limits it governs.
    // The disagreement is deliberate here and is not an error — it is
    // reported by `glc-admin fees-show`.
    use crate::routes::Route;

    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_appended(
        dir.path(),
        "\n[robinhood.policy]\nfee_bps = 600\nper_transfer_limit = 2000000000000\n\
         rolling_daily_limit = 1000000000000000\n\
         \n[fees]\nGlcToSol = 300\nSolToGlc = 300\nGlcToRhn = 300\nRhnToGlc = 300\n",
    );
    let config = Config::load(&path).unwrap();

    assert_eq!(config.route_fees.fee_bps(Route::GlcToRhn).unwrap(), 300);
    assert_eq!(
        config
            .chain_policies
            .get(crate::routes::Chain::Robinhood)
            .unwrap()
            .fee_bps(),
        600,
        "the chain policy keeps its own stated value; only pricing moved"
    );
}

#[test]
fn a_partial_fees_section_is_refused_rather_than_topped_up() {
    // Three of four routes is far likelier to be an unfinished edit than
    // a deliberate one, and the cost of guessing wrong is mispriced money.
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_appended(
        dir.path(),
        "\n[fees]\nGlcToSol = 300\nSolToGlc = 300\nGlcToRhn = 600\n",
    );
    let err = Config::load(&path).unwrap_err().to_string();
    assert!(err.contains("RhnToGlc"), "{err}");
    assert!(err.contains("must name every executable route"), "{err}");
}

#[test]
fn a_fees_section_may_omit_a_disabled_cross_route_but_not_a_legacy_one() {
    // Every production [fees] table predates the two cross routes and
    // must keep loading unchanged...
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_appended(
        dir.path(),
        "\n[fees]\nGlcToSol = 300\nSolToGlc = 300\nGlcToRhn = 600\nRhnToGlc = 600\n",
    );
    Config::load(&path).unwrap();
    // ...while a table missing one of the four pre-existing routes is
    // still refused, exactly as before.
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_appended(
        dir.path(),
        "\n[fees]\nGlcToSol = 300\nSolToGlc = 300\nGlcToRhn = 600\n",
    );
    let err = Config::load(&path).unwrap_err().to_string();
    assert!(err.contains("RhnToGlc"), "{err}");
}

#[test]
fn a_fees_section_naming_something_that_is_not_a_route_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config_with_appended(
        dir.path(),
        "\n[fees]\nGlcToSol = 300\nSolToGlc = 300\nGlcToRhn = 600\nRhnToGlc = 600\n\
         GlcToMoon = 300\n",
    );
    let err = Config::load(&path).unwrap_err().to_string();
    assert!(err.contains("GlcToMoon"), "{err}");
    assert!(err.contains("not a route this bridge models"), "{err}");
}

#[test]
fn a_fees_section_with_an_invalid_rate_is_refused_at_startup() {
    // The ONLY invalid rates are out-of-range ones: 100% and above, where
    // the transfer would deliver nothing or the net would go negative.
    // Refused where an operator finds out immediately.
    for (bad, expect) in [
        ("10000", "deliver nothing"),
        ("10001", "deliver nothing"),
        // TOML integers are signed 64-bit, so i64::MAX is the largest
        // value a config file can even express.
        ("9223372036854775807", "deliver nothing"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = valid_config_with_appended(
            dir.path(),
            &format!(
                "\n[fees]\nGlcToSol = 300\nSolToGlc = 300\nGlcToRhn = 600\nRhnToGlc = {bad}\n"
            ),
        );
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains(expect), "rate {bad}: {err}");
        assert!(
            err.contains("RhnToGlc"),
            "rate {bad} must name the route: {err}"
        );
    }
}

#[test]
fn changing_one_routes_fee_in_the_config_moves_only_that_route() {
    // The config-level statement of the admin flow's core promise.
    use crate::routes::Route;

    let base = "\n[fees]\nGlcToSol = 300\nSolToGlc = 300\nGlcToRhn = 600\nRhnToGlc = 600\n";
    let changed = "\n[fees]\nGlcToSol = 300\nSolToGlc = 300\nGlcToRhn = 600\nRhnToGlc = 300\n";

    let dir_a = tempfile::tempdir().unwrap();
    let before = Config::load(&valid_config_with_appended(dir_a.path(), base)).unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let after = Config::load(&valid_config_with_appended(dir_b.path(), changed)).unwrap();

    assert_eq!(before.route_fees.fee_bps(Route::RhnToGlc).unwrap(), 600);
    assert_eq!(after.route_fees.fee_bps(Route::RhnToGlc).unwrap(), 300);
    for untouched in [Route::GlcToSol, Route::SolToGlc, Route::GlcToRhn] {
        assert_eq!(
            after.route_fees.fee_bps(untouched).unwrap(),
            before.route_fees.fee_bps(untouched).unwrap(),
            "{} must not have moved",
            untouched.as_str()
        );
    }
}

// ------------------------------------------------------------ [rapid_burst] --

/// Appends a `[rapid_burst]` section to [`valid_config`]'s TOML.
fn valid_config_with_rapid_burst(dir: &std::path::Path, section: &str) -> PathBuf {
    let path = valid_config(dir);
    let mut content = std::fs::read_to_string(&path).unwrap();
    content.push_str("\n[rapid_burst]\n");
    content.push_str(section);
    content.push('\n');
    std::fs::write(&path, content).unwrap();
    path
}

/// A config with no `[rapid_burst]` section — every production file that
/// exists today — resolves to the rule DISABLED, with the documented
/// defaults visible for the operator listing.
#[test]
fn absent_rapid_burst_section_disables_the_rule_with_documented_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::load(&valid_config(dir.path())).unwrap();
    assert!(!config.rapid_burst.enabled);
    assert_eq!(config.rapid_burst.window_secs, 900);
    assert_eq!(config.rapid_burst.max_per_source_wallet, 3);
    assert_eq!(config.rapid_burst.max_per_destination_wallet, 3);
    assert_eq!(config.rapid_burst.max_per_pair, 2);
    assert_eq!(config.rapid_burst.minimum_review_hold_secs, 72 * 3600);
}

#[test]
fn rapid_burst_section_is_config_driven_and_validated() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::load(&valid_config_with_rapid_burst(
        dir.path(),
        "enabled = true\nwindow_secs = 300\nmax_per_source_wallet = 4\n\
         max_per_destination_wallet = 5\nmax_per_pair = 1\nminimum_review_hold_secs = 3600",
    ))
    .unwrap();
    assert!(config.rapid_burst.enabled);
    assert_eq!(config.rapid_burst.window_secs, 300);
    assert_eq!(config.rapid_burst.max_per_source_wallet, 4);
    assert_eq!(config.rapid_burst.max_per_destination_wallet, 5);
    assert_eq!(config.rapid_burst.max_per_pair, 1);
    assert_eq!(config.rapid_burst.minimum_review_hold_secs, 3600);

    // `enabled = true` alone is a complete, sane policy.
    let config =
        Config::load(&valid_config_with_rapid_burst(dir.path(), "enabled = true")).unwrap();
    assert!(config.rapid_burst.enabled);
    assert_eq!(config.rapid_burst.window_secs, 900);

    for (section, field) in [
        ("window_secs = 0", "rapid_burst.window_secs"),
        ("max_per_pair = 0", "rapid_burst.max_per_pair"),
        (
            "max_per_source_wallet = 0",
            "rapid_burst.max_per_source_wallet",
        ),
        (
            "max_per_destination_wallet = 0",
            "rapid_burst.max_per_destination_wallet",
        ),
        (
            "minimum_review_hold_secs = -1",
            "rapid_burst.minimum_review_hold_secs",
        ),
    ] {
        let err = Config::load(&valid_config_with_rapid_burst(dir.path(), section)).unwrap_err();
        match err {
            ConfigError::Invalid { field: f, .. } => assert_eq!(f, field, "{section}"),
            other => panic!("{section}: expected Invalid, got {other:?}"),
        }
    }
    // Unknown keys are refused, not ignored.
    assert!(Config::load(&valid_config_with_rapid_burst(
        dir.path(),
        "enabled = true\nmax_per_second = 3"
    ))
    .is_err());
}
