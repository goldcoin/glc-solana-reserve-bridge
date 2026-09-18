use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction as SolanaTx;

use super::*;
use crate::amount_conversion::{compute_fee, CanonicalAtomic};
use crate::ledger::RequestAmounts;
use crate::solana::rpc::SolanaRpcError;
use auth::AdminAuthToken;

const ALICE_TOKEN: &str = "test-token-alice-7c1";
const BOB_TOKEN: &str = "test-token-bob-9e4";

// ------------------------------------------------------------ fixtures --

/// Mirrors `api::tests`' fake `BridgeConfig` bytes (borsh layout after
/// the 8-byte discriminator, `pending_admin: None`).
fn fake_bridge_config_bytes() -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.push(1); // protocol_version
    v.extend_from_slice(&[0u8; 32]); // admin
    v.push(0); // pending_admin: None
    v.push(0); // paused
    v.push(1); // release_paused
    v.push(0); // deposit_paused
    v.push(7); // bump
    v.extend_from_slice(&[9u8; 32]); // reserve_token_mint
    v.extend_from_slice(spl_token::ID.as_ref()); // reserve_token_program
    v.push(3); // reserve_authority_bump
    v.extend_from_slice(&11u64.to_le_bytes()); // obligation_count
    v.extend_from_slice(&3600i64.to_le_bytes()); // governance_timelock_seconds
    v.extend_from_slice(&100_000_000u64.to_le_bytes()); // min_transfer_amount
    v.extend_from_slice(&10_000_000_000u64.to_le_bytes()); // per_transfer_limit
    v.extend_from_slice(&20_000_000_000u64.to_le_bytes()); // protected_minimum
    v.extend_from_slice(&100_000_000_000u64.to_le_bytes()); // rolling_volume_limit
    v.extend_from_slice(&86_400i64.to_le_bytes()); // rolling_window_seconds
    v
}

fn fake_rolling_volume_window_bytes(
    direction: u8,
    window_start: i64,
    window_total: u64,
) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.push(direction);
    v.extend_from_slice(&window_start.to_le_bytes());
    v.extend_from_slice(&window_total.to_le_bytes());
    v.push(4); // bump
    v.extend_from_slice(&[0u8; 16]); // reserved
    v
}

struct FakeSolanaRpc;

impl crate::solana::rpc::SolanaRpc for FakeSolanaRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        let data = if *pubkey == accounts::bridge_config_pda() {
            fake_bridge_config_bytes()
        } else if *pubkey == Pubkey::new_from_array([9u8; 32]) {
            // The reserve mint the fake config declares — a minimal
            // 82-byte SPL Mint buffer at 6 decimals, served so
            // `fetch_reserve_mint_decimals`'s LIVE read works (the same
            // fixture shape `api::tests` uses).
            let mut mint = vec![0u8; 82];
            mint[44] = 6; // decimals
            mint[45] = 1; // is_initialized
            mint
        } else if *pubkey == accounts::rolling_volume_window_pda(0) {
            // Fresh, recent bucket with 25,000 GLC (6dp) already used.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            fake_rolling_volume_window_bytes(0, now - 100, 25_000_000_000)
        } else if *pubkey == accounts::rolling_volume_window_pda(1) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            fake_rolling_volume_window_bytes(1, now - 100, 0)
        } else {
            return Ok(None);
        };
        Ok(Some(Account {
            lamports: 1,
            data,
            owner: accounts::PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        }))
    }
    async fn get_multiple_accounts(
        &self,
        _pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        unimplemented!()
    }
    async fn send_transaction(&self, _tx: &SolanaTx) -> Result<Signature, SolanaRpcError> {
        unimplemented!()
    }
    async fn simulate_transaction(
        &self,
        _tx: &SolanaTx,
    ) -> Result<crate::solana::rpc::SimulationOutcome, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_signature_status(
        &self,
        _signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn is_blockhash_valid(&self, _blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        unimplemented!()
    }
}

/// Configures both reserve rows so every handler that reads them works.
/// Goldcoin balance is generous relative to the fold amounts used below,
/// so folds finalize unless a test arranges otherwise.
fn configure_ledger(db_path: &std::path::Path) {
    let mut ledger = Ledger::open(db_path).unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            1_000_000_000, // balance
            1_000,         // protected_minimum
            900_000_000,   // target
            500_000_000,   // warning
            100_000,       // critical (> protected_minimum)
            0,
        )
        .unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            1_000_000_000,
            1_000,
            900_000_000,
            500_000_000,
            100_000,
            0,
        )
        .unwrap();
}

fn amounts_for_gross(gross: u64) -> RequestAmounts {
    let fb = compute_fee(CanonicalAtomic(gross)).unwrap();
    RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
        quote: None,
    }
}

fn wallet(tag: u8) -> [u8; 32] {
    [tag; 32]
}

fn recipient(tag: u8) -> Vec<u8> {
    format!("GLCRECIPIENT{tag:02}XXXXXXXXXXXXXXXXX").into_bytes()
}

/// A listener on an ephemeral loopback port, handed to the server still
/// bound.
///
/// Deliberately NOT "pick a free port, drop the listener, let the server
/// re-bind it": between the drop and the re-bind the port belongs to
/// nobody, and under a parallel test run something else on the host takes
/// it. When that happened here the server's own bind failed, the failure
/// was swallowed by the spawn, and the readiness probe plus every
/// subsequent request were answered by the OTHER test's server — which
/// returned a plausible 404 for a request id its ledger had never heard
/// of, while this test's ledger sat untouched. Holding the listener from
/// the moment the port is chosen removes the window entirely.
async fn bound_listener() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// Spawns the REAL admin server (real hyper listener, real auth
/// registry, real `AdminApi` over a temp SQLite ledger and the fake RPC)
/// on a loopback port — same approach `api::tests::spawn_real_server`
/// uses.
async fn spawn_admin_server(
    db_path: &std::path::Path,
) -> (String, tokio::sync::watch::Sender<bool>) {
    let (listener, port) = bound_listener().await;
    let (tx, rx) = tokio::sync::watch::channel(false);
    let source = Arc::new(AdminApi::new(db_path.to_path_buf(), FakeSolanaRpc));
    let registry = Arc::new(
        OperatorRegistry::new(vec![
            crate::admin_api::auth::ResolvedOperator {
                name: "alice".to_string(),
                token: AdminAuthToken::for_tests(ALICE_TOKEN),
                // Alice holds the refund-execution capability; Bob does
                // not, so the tests can prove an ordinary admin token is
                // insufficient.
                may_execute_glc_refunds: true,
            },
            crate::admin_api::auth::ResolvedOperator {
                name: "bob".to_string(),
                token: AdminAuthToken::for_tests(BOB_TOKEN),
                may_execute_glc_refunds: false,
            },
        ])
        .unwrap(),
    );
    tokio::spawn(async move {
        // `serve_on` cannot fail to bind — the listener is already ours.
        let _ = serve_on(listener, source, registry, rx).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client
            .get(format!("{base}/fee"))
            .bearer_auth(ALICE_TOKEN)
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (base, tx)
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

const GET_PATHS: [&str; 11] = [
    "/whoami",
    "/status",
    "/reserve-health",
    "/onchain",
    "/fee",
    "/manual-review",
    "/refunds",
    "/rebalances",
    "/audit-log",
    "/routes",
    "/submitters",
];

// ------------------------------------------------------------- authz --

#[tokio::test]
async fn every_endpoint_requires_a_valid_bearer_token() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    for path in GET_PATHS {
        let no_token = c.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(no_token.status(), 401, "GET {path} without a token");
        let wrong = c
            .get(format!("{base}{path}"))
            .bearer_auth("not-a-real-token")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 401, "GET {path} with a wrong token");
    }

    // Every mutation route refuses without a token — and, critically,
    // does not mutate.
    for (path, body) in [
        ("/pause", r#"{"direction":"goldcoin","note":"n"}"#),
        ("/unpause", r#"{"direction":"goldcoin","note":"n"}"#),
        ("/admission/close", r#"{"direction":"goldcoin","note":"n"}"#),
        ("/admission/open", r#"{"direction":"goldcoin","note":"n"}"#),
        ("/manual-review/1/resume", r#"{"note":"n"}"#),
        (
            "/rebalances",
            r#"{"direction":"goldcoin","kind":"deposit","amount_atomic":5,"required_approvals":1,"note":"n"}"#,
        ),
        ("/rebalances/1/approve", r#"{"note":"n"}"#),
        (
            "/rebalances/1/record-executed",
            r#"{"tx_reference":"t","note":"n"}"#,
        ),
        (
            "/rebalances/1/confirm",
            r#"{"observed_amount_atomic":5,"note":"n"}"#,
        ),
        ("/rebalances/1/fail", r#"{"note":"n"}"#),
        (
            "/cli-command",
            r#"{"action":"onchain-pause","scope":"global"}"#,
        ),
    ] {
        let resp = c
            .post(format!("{base}{path}"))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "POST {path} without a token");
    }

    let ledger = Ledger::open(&db_path).unwrap();
    assert!(
        !ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        "an unauthorized request must never mutate state"
    );
    assert!(
        ledger
            .list_admin_audit(&AdminAuditFilter::default())
            .unwrap()
            .is_empty(),
        "an unauthorized request must never reach the audit-writing layer"
    );
}

#[tokio::test]
async fn cookie_or_origin_bearing_requests_are_rejected_even_with_a_valid_token() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    let with_cookie = c
        .get(format!("{base}/status"))
        .bearer_auth(ALICE_TOKEN)
        .header("cookie", "session=whatever")
        .send()
        .await
        .unwrap();
    assert_eq!(with_cookie.status(), 403);

    let with_origin = c
        .post(format!("{base}/pause"))
        .bearer_auth(ALICE_TOKEN)
        .header("origin", "https://evil.example")
        .header("content-type", "application/json")
        .body(r#"{"direction":"goldcoin","note":"n"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(with_origin.status(), 403);

    let ledger = Ledger::open(&db_path).unwrap();
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
}

#[tokio::test]
async fn whoami_reports_the_operator_the_token_resolves_to() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let body: serde_json::Value = client()
        .get(format!("{base}/whoami"))
        .bearer_auth(BOB_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["operator"], "bob");
}

// ------------------------------------------------------------ fee view --

#[tokio::test]
async fn fee_endpoint_is_read_only_and_reports_every_routes_rate() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    let body: serde_json::Value = c
        .get(format!("{base}/fee"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // The endpoint reports the TABLE, not one number: fees are per route
    // now, and a single `bridge_fee_bps` could not answer "what does this
    // bridge charge?" without picking a route and not saying which.
    //
    // This server is built without `with_route_fees`, so the honest
    // answer is an empty table — not a rate, and specifically not a rate
    // borrowed from a constant, which is exactly the shape of the bug
    // per-route fees removed.
    assert!(
        body["routes"].is_array(),
        "expected a per-route table, got {body}"
    );
    assert_eq!(body["routes"].as_array().unwrap().len(), 0);
    assert!(
        body["provenance"]
            .as_str()
            .unwrap()
            .contains("one rate per executable route"),
        "provenance must describe where the rates come from: {body}"
    );
    // The old single-number field is gone rather than left behind holding
    // one route's rate under a name that implies it is everyone's.
    assert!(body["bridge_fee_bps"].is_null(), "{body}");

    // There is no mutation route for the fee — a POST is not found.
    let post = c
        .post(format!("{base}/fee"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"bridge_fee_bps":0}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 404);
}

// -------------------------------------------------- pause / admission --

#[tokio::test]
async fn local_pause_and_unpause_mutate_and_write_audit_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    let resp = c
        .post(format!("{base}/pause"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"direction":"goldcoin","note":"incident 42"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let receipt: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(receipt["action"], "pause");
    assert_eq!(receipt["old_value"], "paused=false");
    assert_eq!(receipt["new_value"], "paused=true");
    let audit_id = receipt["audit_id"].as_i64().unwrap();

    {
        let ledger = Ledger::open(&db_path).unwrap();
        assert!(ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
        assert_eq!(
            ledger
                .pause_reason(ReserveDirection::GoldcoinReserve)
                .unwrap(),
            Some("incident 42".to_string())
        );
        let rows = ledger
            .list_admin_audit(&AdminAuditFilter::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, audit_id);
        assert_eq!(rows[0].actor, "alice");
        assert_eq!(rows[0].action, "pause");
        assert_eq!(rows[0].target.as_deref(), Some("goldcoin"));
        assert_eq!(rows[0].note, "incident 42");
        assert_eq!(rows[0].outcome, AdminAuditOutcome::Success);
    }

    let resp = c
        .post(format!("{base}/unpause"))
        .bearer_auth(BOB_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"direction":"goldcoin","note":"incident 42 resolved"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let ledger = Ledger::open(&db_path).unwrap();
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].actor, "bob");
    assert_eq!(rows[0].action, "unpause");
}

#[tokio::test]
async fn a_missing_or_empty_note_is_rejected_with_no_mutation_and_no_audit_row() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    for body in [
        r#"{"direction":"goldcoin","note":""}"#,
        r#"{"direction":"goldcoin","note":"   "}"#,
        r#"{"direction":"goldcoin"}"#,
    ] {
        let resp = c
            .post(format!("{base}/pause"))
            .bearer_auth(ALICE_TOKEN)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "body {body} must be rejected");
    }

    let ledger = Ledger::open(&db_path).unwrap();
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
    assert!(ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn admission_open_runs_the_invariant_gate_and_audits_the_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    // Close admission (only the goldcoin direction implements it).
    let resp = c
        .post(format!("{base}/admission/close"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"direction":"goldcoin","note":"maintenance"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    {
        let ledger = Ledger::open(&db_path).unwrap();
        assert!(ledger
            .is_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap());
    }

    // Solana direction is refused, exactly like the CLI.
    let resp = c
        .post(format!("{base}/admission/close"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"direction":"solana","note":"maintenance"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Break the reserve invariant (balance below protected minimum with
    // no obligations), then attempt to re-open: refused, admission stays
    // closed, and the REFUSAL is in the audit log.
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .refresh_reserve_balance(ReserveDirection::GoldcoinReserve, 10, 1)
            .unwrap();
    }
    let resp = c
        .post(format!("{base}/admission/open"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"direction":"goldcoin","note":"reopening"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("refusing to open admission"),
        "{body}"
    );
    {
        let ledger = Ledger::open(&db_path).unwrap();
        assert!(ledger
            .is_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap());
        let rows = ledger
            .list_admin_audit(&AdminAuditFilter::default())
            .unwrap();
        assert_eq!(rows[0].action, "admission_open");
        assert!(matches!(rows[0].outcome, AdminAuditOutcome::Error(_)));
    }

    // Restore the balance; the open now succeeds through the same gate.
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .refresh_reserve_balance(ReserveDirection::GoldcoinReserve, 1_000_000_000, 2)
            .unwrap();
    }
    let resp = c
        .post(format!("{base}/admission/open"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"direction":"goldcoin","note":"reopening"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ledger = Ledger::open(&db_path).unwrap();
    assert!(!ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

/// `open-admission` must also refuse while the AUTOMATIC
/// confirmed-liquidity gate is still closed (docs/09-runbook.md's
/// "Confirmed-liquidity admission safety buffer") — otherwise clearing
/// the operator flag would appear to succeed while every new fold kept
/// parking, and the operator would have no explanation for why nothing
/// changed. Same shape as the invariant and UTXO-count gates above: a
/// 409 with the reason, admission left closed, the refusal audited.
#[tokio::test]
async fn admission_open_refuses_while_the_confirmed_liquidity_gate_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // Headroom is 1_000_000_000 - 1_000 = 999_999_000. A buffer above
        // that closes the gate while the reserve stays entirely solvent —
        // the hard invariant and the UTXO-count gate both still pass, so
        // this test can only be refused by the new check.
        ledger
            .set_admission_liquidity_thresholds(
                ReserveDirection::GoldcoinReserve,
                2_000_000_000,
                3_000_000_000,
            )
            .unwrap();
        ledger
            .set_admission(ReserveDirection::GoldcoinReserve, true, Some("maintenance"))
            .unwrap();
        ledger
            .check_invariant(ReserveDirection::GoldcoinReserve)
            .unwrap();
    }
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    let resp = c
        .post(format!("{base}/admission/open"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"direction":"goldcoin","note":"reopening"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    let message = body["error"].as_str().unwrap();
    assert!(
        message.contains("liquidity_admission_closed"),
        "the refusal must name the liquidity gate, not a generic failure: {body}"
    );

    {
        let ledger = Ledger::open(&db_path).unwrap();
        assert!(ledger
            .is_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap());
        let rows = ledger
            .list_admin_audit(&AdminAuditFilter::default())
            .unwrap();
        assert_eq!(rows[0].action, "admission_open");
        assert!(matches!(rows[0].outcome, AdminAuditOutcome::Error(_)));
    }

    // Once confirmed headroom recovers past the reopen threshold, the
    // identical request succeeds through the identical gate.
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .refresh_reserve_balance(ReserveDirection::GoldcoinReserve, 4_000_000_000, 2)
            .unwrap();
    }
    let resp = c
        .post(format!("{base}/admission/open"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"direction":"goldcoin","note":"reopening"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ledger = Ledger::open(&db_path).unwrap();
    assert!(!ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
    assert!(!ledger
        .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

// ------------------------------------------------------ manual review --

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Parks one SolToGlc request in ManualReview by folding it against a
/// paused Goldcoin reserve, then unpauses. Returns the request id.
fn park_request(db_path: &std::path::Path, obligation: u64, tag: u8, at: i64) -> i64 {
    let mut ledger = Ledger::open(db_path).unwrap();
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("test park"))
        .unwrap();
    let outcome = ledger
        .fold_sol_deposit(
            obligation,
            amounts_for_gross(100_000),
            wallet(tag),
            &recipient(tag),
            None,
            at,
        )
        .unwrap();
    ledger
        .set_paused(
            ReserveDirection::GoldcoinReserve,
            false,
            Some("test unpark"),
        )
        .unwrap();
    match outcome {
        crate::ledger::SolFoldOutcome::FoldedManualReview { request_id } => request_id,
        other => panic!("expected a ManualReview park, got {other:?}"),
    }
}

#[tokio::test]
async fn resume_manual_review_succeeds_through_the_real_ledger_path() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let request_id = park_request(&db_path, 1, 10, now_unix());
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let resp = client()
        .post(format!("{base}/manual-review/{request_id}/resume"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"note":"capacity restored"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let ledger = Ledger::open(&db_path).unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::SourceFinalized);
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows[0].action, "resume_manual_review");
    assert_eq!(rows[0].outcome, AdminAuditOutcome::Success);
    assert_eq!(rows[0].old_value.as_deref(), Some("ManualReview"));
}

/// `POST /manual-review/{id}/process` on a HELD request: the ManualReview
/// listing shows the hold fields; the ordinary resume route is refused
/// (409) while held; `process` records the decision and re-admits through
/// the real ledger path, audited as `manual_review_process`.
#[tokio::test]
async fn process_manual_review_decides_a_held_request_through_the_real_ledger_path() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let now = now_unix();
    let request_id = park_request(&db_path, 1, 10, now - 100);
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_manual_review_hold(request_id, None, "snapshot freeze", "cli:ops", now - 50)
            .unwrap();
    }
    let (base, _tx) = spawn_admin_server(&db_path).await;

    // The listing carries the hold.
    let listing: serde_json::Value = client()
        .get(format!("{base}/manual-review"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let item = listing["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["request_id"] == request_id)
        .unwrap()
        .clone();
    assert_eq!(item["disposition"], "operator_hold");
    assert_eq!(item["held"], true);
    assert_eq!(item["hold_reason"], "operator_hold");
    assert_eq!(item["hold_note"], "snapshot freeze");
    assert_eq!(item["held_by"], "cli:ops");
    assert_eq!(item["hold_started_at"], now - 50);
    assert!(item["review_after"].is_null());
    assert_eq!(item["review_available"], true);
    assert!(item["operator_decision"].is_null());

    // The plain resume route is refused while held.
    let resp = client()
        .post(format!("{base}/manual-review/{request_id}/resume"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"note":"trying"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // The process decision goes through.
    let resp = client()
        .post(format!("{base}/manual-review/{request_id}/process"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"note":"reviewed: legitimate"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    let ledger = Ledger::open(&db_path).unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::SourceFinalized);
    assert_eq!(
        req.operator_decision,
        Some(crate::ledger::OperatorDecision::Process)
    );
    assert_eq!(req.operator_note.as_deref(), Some("reviewed: legitimate"));
    assert!(!req.is_held());
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows[0].action, "manual_review_process");
    assert_eq!(rows[0].outcome, AdminAuditOutcome::Success);
    assert!(rows[0]
        .old_value
        .as_deref()
        .unwrap()
        .contains("disposition=operator_hold"));
    assert_eq!(
        rows[0].new_value.as_deref(),
        Some("decision=process state=SourceFinalized")
    );
    // A second process is refused (not held) and audited as a failure.
    let resp = client()
        .post(format!("{base}/manual-review/{request_id}/process"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"note":"again"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn resume_refuses_a_rate_limited_recipient_exactly_like_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let t0 = now_unix() - 600;

    // Request A: same recipient, DIFFERENT wallet, folded normally and
    // inside the rolling 24h window — the strict predecessor that makes
    // the recipient rate limit apply to B.
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        let outcome = ledger
            .fold_sol_deposit(
                1,
                amounts_for_gross(100_000),
                wallet(21),
                &recipient(20),
                None,
                t0,
            )
            .unwrap();
        assert!(
            matches!(
                outcome,
                crate::ledger::SolFoldOutcome::FoldedFinalized { .. }
            ),
            "{outcome:?}"
        );
    }
    // Request B: same recipient, parked.
    let request_id = park_request(&db_path, 2, 20, t0 + 60);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let resp = client()
        .post(format!("{base}/manual-review/{request_id}/resume"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"note":"trying anyway"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("wallet_destination_24h_limit"),
        "{body}"
    );

    let ledger = Ledger::open(&db_path).unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        req.state,
        RequestState::ManualReview,
        "a refused resume must not mutate the request"
    );
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows[0].action, "resume_manual_review");
    assert!(matches!(&rows[0].outcome, AdminAuditOutcome::Error(e)
        if e.contains("wallet_destination_24h_limit")));
}

#[tokio::test]
async fn resume_of_a_missing_request_is_404_and_still_audited() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let resp = client()
        .post(format!("{base}/manual-review/999/resume"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"note":"typo'd id"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let ledger = Ledger::open(&db_path).unwrap();
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert!(matches!(rows[0].outcome, AdminAuditOutcome::Error(_)));
}

#[tokio::test]
async fn manual_review_listing_carries_reason_and_rate_limit_context() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let t0 = now_unix() - 600;
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .fold_sol_deposit(
                1,
                amounts_for_gross(100_000),
                wallet(31),
                &recipient(30),
                None,
                t0,
            )
            .unwrap();
    }
    let request_id = park_request(&db_path, 2, 30, t0 + 60);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let body: serde_json::Value = client()
        .get(format!("{base}/manual-review"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let requests = body["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 1);
    let item = &requests[0];
    assert_eq!(item["request_id"].as_i64().unwrap(), request_id);
    assert_eq!(item["direction"], "SolToGlc");
    assert!(item["reason"].is_string());
    assert!(
        item["recipient_rate_limited_until"].is_i64(),
        "the same-recipient predecessor must surface a retry-after: {item}"
    );
    assert_eq!(item["gross_amount_atomic"].as_u64().unwrap(), 100_000);
}

// ---------------------------------------------------------- rebalance --

#[tokio::test]
async fn rebalance_workflow_runs_end_to_end_with_audit_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    // Propose.
    let resp = c
        .post(format!("{base}/rebalances"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(
            r#"{"direction":"goldcoin","kind":"deposit","amount_atomic":5000,"required_approvals":1,"note":"top up"}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let receipt: serde_json::Value = resp.json().await.unwrap();
    let id: i64 = receipt["target"].as_str().unwrap().parse().unwrap();

    // Listed with both per-direction assessments.
    let list: serde_json::Value = c
        .get(format!("{base}/rebalances"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["assessments"].as_array().unwrap().len(), 2);
    assert_eq!(list["requests"][0]["state"], "Proposed");

    // Approve -> record-executed -> confirm.
    for (verb, body) in [
        ("approve", r#"{"note":"looks right"}"#.to_string()),
        (
            "record-executed",
            r#"{"tx_reference":"goldcoin:txid:abc123","note":"sent from treasury"}"#.to_string(),
        ),
        (
            "confirm",
            r#"{"observed_amount_atomic":5000,"note":"landed"}"#.to_string(),
        ),
    ] {
        let resp = c
            .post(format!("{base}/rebalances/{id}/{verb}"))
            .bearer_auth(BOB_TOKEN)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{verb}");
    }
    let detail: serde_json::Value = c
        .get(format!("{base}/rebalances/{id}"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["state"], "Confirmed");
    assert_eq!(detail["tx_reference"], "goldcoin:txid:abc123");

    // A second proposal is rejected through the same audited path.
    let resp = c
        .post(format!("{base}/rebalances"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(
            r#"{"direction":"solana","kind":"withdraw","amount_atomic":7,"required_approvals":1,"note":"mistake"}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let second: serde_json::Value = resp.json().await.unwrap();
    let second_id: i64 = second["target"].as_str().unwrap().parse().unwrap();
    let resp = c
        .post(format!("{base}/rebalances/{second_id}/reject"))
        .bearer_auth(BOB_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"note":"not needed"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Confirming an already-confirmed rebalance refuses AND audits.
    let resp = c
        .post(format!("{base}/rebalances/{id}/confirm"))
        .bearer_auth(BOB_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"observed_amount_atomic":5000,"note":"again"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    let ledger = Ledger::open(&db_path).unwrap();
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    let actions: Vec<&str> = rows.iter().map(|r| r.action.as_str()).collect();
    assert_eq!(
        actions,
        vec![
            "rebalance_confirm", // the audited refusal
            "rebalance_reject",
            "rebalance_propose",
            "rebalance_confirm",
            "rebalance_record_executed",
            "rebalance_approve",
            "rebalance_propose",
        ]
    );
    assert!(matches!(rows[0].outcome, AdminAuditOutcome::Error(_)));
}

// --------------------------------------------------- on-chain reads --

#[tokio::test]
async fn onchain_view_decodes_config_and_rolling_windows() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let body: serde_json::Value = client()
        .get(format!("{base}/onchain"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["per_transfer_limit"].as_u64().unwrap(), 10_000_000_000);
    assert_eq!(body["min_transfer_amount"].as_u64().unwrap(), 100_000_000);
    assert_eq!(
        body["rolling_volume_limit"].as_u64().unwrap(),
        100_000_000_000
    );
    assert_eq!(body["release_paused"], true);
    assert_eq!(body["deposit_paused"], false);
    let windows = body["rolling_windows"].as_array().unwrap();
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0]["window"], "glc-to-sol");
    assert_eq!(
        windows[0]["remaining"].as_u64().unwrap(),
        75_000_000_000,
        "100,000 GLC limit minus 25,000 GLC used this bucket"
    );
    assert_eq!(windows[1]["remaining"].as_u64().unwrap(), 100_000_000_000);
}

#[tokio::test]
async fn cli_command_endpoint_generates_the_exact_set_limit_command() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let body: serde_json::Value = client()
        .post(format!("{base}/cli-command"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"action":"set-limit","field":"per-transfer","value_glc":"20000"}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["command"],
        "glc-admin set-limit --rpc-url <RPC_URL> --keypair <ADMIN_KEYPAIR_PATH> --field per-transfer --value 20000000000 --note '<NOTE>'"
    );
    assert_eq!(
        body["old_value"]["atomic"].as_u64().unwrap(),
        10_000_000_000
    );
    assert_eq!(body["old_value"]["display_glc"], "10000");
    assert_eq!(
        body["new_value"]["atomic"].as_u64().unwrap(),
        20_000_000_000
    );
    assert_eq!(body["label"], "CLI approval required");
}

// ----------------------------------------------------- audit-log API --

#[tokio::test]
async fn audit_log_endpoint_supports_actor_action_filters_and_keyset_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    for (token, direction) in [(ALICE_TOKEN, "goldcoin"), (BOB_TOKEN, "solana")] {
        for verb in ["pause", "unpause"] {
            let resp = c
                .post(format!("{base}/{verb}"))
                .bearer_auth(token)
                .header("content-type", "application/json")
                .body(format!(r#"{{"direction":"{direction}","note":"cycling"}}"#))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
        }
    }

    let all: serde_json::Value = c
        .get(format!("{base}/audit-log"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all["rows"].as_array().unwrap().len(), 4);

    let alices: serde_json::Value = c
        .get(format!("{base}/audit-log?actor=alice"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = alices["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r["actor"] == "alice"));

    let pauses: serde_json::Value = c
        .get(format!("{base}/audit-log?action=pause&limit=1"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = pauses["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let first_id = rows[0]["id"].as_i64().unwrap();

    let next: serde_json::Value = c
        .get(format!(
            "{base}/audit-log?action=pause&before_id={first_id}"
        ))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = next["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["id"].as_i64().unwrap() < first_id);
}

// ------------------------------------------------------- no secrets --

#[tokio::test]
async fn no_admin_response_ever_contains_token_material() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    let mut bodies: Vec<(String, String)> = Vec::new();
    for path in GET_PATHS {
        let text = c
            .get(format!("{base}{path}"))
            .bearer_auth(ALICE_TOKEN)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        bodies.push((path.to_string(), text));
    }
    // A mutation response and an error response, too.
    let text = c
        .post(format!("{base}/pause"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"direction":"goldcoin","note":"sweep"}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    bodies.push(("/pause".to_string(), text));
    let text = c
        .post(format!("{base}/pause"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"direction":"nonsense","note":"sweep"}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    bodies.push(("/pause (error)".to_string(), text));

    for (path, body) in &bodies {
        assert!(
            !body.contains(ALICE_TOKEN) && !body.contains(BOB_TOKEN),
            "{path} leaked token material: {body}"
        );
    }
}

// ------------------------------------------- review-fix regressions --

/// Finding: mutation + audit append must be atomic. An audit append that
/// fails AFTER the mutation succeeded must roll the mutation back —
/// never leave it committed and unaudited (where a retry would duplicate
/// a non-idempotent action). Forced here by an entry the schema itself
/// refuses (empty note), which is exactly the append-failure shape.
#[tokio::test]
async fn a_failed_audit_append_rolls_the_mutation_back() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let mut ledger = Ledger::open(&db_path).unwrap();

    let err = audited_mutation(
        &mut ledger,
        AuditedAction {
            actor: "alice",
            action: "pause",
            target: "goldcoin".to_string(),
            note: "", // schema CHECK (note <> '') fails the append
            new_value: None,
        },
        |_| Ok(None),
        |l| {
            l.set_paused(ReserveDirection::GoldcoinReserve, true, Some("x"))
                .map_err(AdminError::from)
        },
        |_: &(), _| {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("rolled back"), "{err}");

    assert!(
        !ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        "the committed-but-unaudited state must be impossible: the pause was rolled back"
    );
    assert!(ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap()
        .is_empty());
}

/// The other half of atomicity: a validated REFUSAL rolls back only the
/// mutation's own writes (its nested savepoint) while the failure audit
/// row still commits — through the real Ledger method that opens an
/// inner write transaction.
#[tokio::test]
async fn a_refused_mutation_still_commits_its_failure_audit_row() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let mut ledger = Ledger::open(&db_path).unwrap();

    let err = audited_resume_manual_review(&mut ledger, 999, "typo'd id", "alice").unwrap_err();
    assert!(matches!(err, AdminError::NotFound(_)), "{err:?}");

    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].action, "resume_manual_review");
    assert!(matches!(rows[0].outcome, AdminAuditOutcome::Error(_)));
}

/// Finding: the resume transition must carry the AUTHENTICATED operator
/// into bridge_request_state_log — never a hardcoded "operator" — so
/// per-person tokens buy per-person attribution in the request's
/// authoritative history too.
#[tokio::test]
async fn resume_records_the_authenticated_operator_in_the_state_log() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let request_id = park_request(&db_path, 1, 40, now_unix());
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let resp = client()
        .post(format!("{base}/manual-review/{request_id}/resume"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"note":"capacity restored"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let ledger = Ledger::open(&db_path).unwrap();
    let actor: String = ledger
        .raw()
        .query_row(
            "SELECT actor FROM bridge_request_state_log
             WHERE request_id = ?1 AND to_state = 'SourceFinalized'
             ORDER BY id DESC LIMIT 1",
            [request_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actor, "alice", "never the hardcoded placeholder");
}

/// Finding: the solana-direction admission refusal was the one refusal
/// raised inside the AdminSource that never left an audit row.
#[tokio::test]
async fn the_solana_direction_admission_refusal_is_audited() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let resp = client()
        .post(format!("{base}/admission/close"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"direction":"solana","note":"misclick"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let ledger = Ledger::open(&db_path).unwrap();
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].action, "admission_close");
    assert_eq!(rows[0].target.as_deref(), Some("solana"));
    assert!(matches!(rows[0].outcome, AdminAuditOutcome::Error(_)));
}

/// Finding: responses must round-trip into the API's own inputs — the
/// direction/kind slugs a GET returns are the same slugs POST bodies
/// accept, and never Rust Debug spellings.
#[tokio::test]
async fn rebalance_views_round_trip_into_request_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    let resp = c
        .post(format!("{base}/rebalances"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(
            r#"{"direction":"goldcoin","kind":"deposit","amount_atomic":5,"required_approvals":1,"note":"n"}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    let list: serde_json::Value = c
        .get(format!("{base}/rebalances"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let request = &list["requests"][0];
    assert_eq!(
        request["direction"], "goldcoin",
        "slug, not GoldcoinReserve"
    );
    assert_eq!(request["kind"], "deposit", "slug, not Deposit");
    assert_eq!(request["state"], "Proposed");

    // The direction read from the response works verbatim as an input.
    let direction = request["direction"].as_str().unwrap();
    let resp = c
        .post(format!("{base}/pause"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"direction":"{direction}","note":"round trip"}}"#
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // ManualReview direction spelling is Direction::as_str, which
    // Direction::from_str parses back.
    let backlog: serde_json::Value = c
        .get(format!("{base}/manual-review"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for item in backlog["requests"].as_array().unwrap() {
        let d = item["direction"].as_str().unwrap();
        assert!(d.parse::<Direction>().is_ok(), "{d:?} must round-trip");
    }
}

/// Findings: `?limit=0` must be a 400 (never a permanently empty page
/// that reads as "no audit rows"), and filter values must be
/// percent-decoded so an actor name with a space is filterable at all.
#[tokio::test]
async fn audit_query_rejects_zero_limit_and_decodes_filters() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .append_admin_audit(&AdminAuditEntry {
                at: 1,
                actor: "ops team".to_string(),
                action: "pause".to_string(),
                target: Some("goldcoin".to_string()),
                old_value: None,
                new_value: None,
                note: "spaced actor".to_string(),
                outcome: AdminAuditOutcome::Success,
            })
            .unwrap();
    }
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    let resp = c
        .get(format!("{base}/audit-log?limit=0"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    for encoded in ["ops%20team", "ops+team"] {
        let body: serde_json::Value = c
            .get(format!("{base}/audit-log?actor={encoded}"))
            .bearer_auth(ALICE_TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let rows = body["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{encoded} must decode to 'ops team'");
        assert_eq!(rows[0]["actor"], "ops team");
    }

    let resp = c
        .get(format!("{base}/audit-log?actor=bad%zz"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "malformed escapes fail loudly");
}

/// Finding: the on-chain view must carry the mint's LIVE decimals, and
/// `/cli-command` must convert with them (pinned end-to-end at the
/// fixture's 6; the not-6 case is unit-tested in cli_command).
#[tokio::test]
async fn onchain_view_reports_live_mint_decimals() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let body: serde_json::Value = client()
        .get(format!("{base}/onchain"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["reserve_mint_decimals"], 6);
}

/// Finding: the reset-rolling-window preview must surface the on-chain
/// `BridgeConfig.paused == true` precondition (the fixture bridge is
/// unpaused, so it applies).
#[tokio::test]
async fn cli_command_reset_reports_the_pause_precondition_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let body: serde_json::Value = client()
        .post(format!("{base}/cli-command"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"action":"reset-rolling-window","direction":"glc-to-sol"}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        body["precondition"]
            .as_str()
            .unwrap()
            .contains("BridgeConfig.paused == true"),
        "{body}"
    );
}

// -------------------------------------- second-round review regressions --

/// A repeated resume is a documented no-op — its audit row must say so,
/// never assert a state transition that did not happen.
#[tokio::test]
async fn a_repeated_resume_audits_as_a_no_op_not_a_transition() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let request_id = park_request(&db_path, 1, 50, now_unix());
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    for _ in 0..2 {
        let resp = c
            .post(format!("{base}/manual-review/{request_id}/resume"))
            .bearer_auth(ALICE_TOKEN)
            .header("content-type", "application/json")
            .body(r#"{"note":"double click"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    let ledger = Ledger::open(&db_path).unwrap();
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows.len(), 2);
    // Newest first: the repeat is a no-op with the ACTUAL state, the
    // original records the real transition.
    assert!(
        rows[0]
            .new_value
            .as_deref()
            .unwrap()
            .starts_with("no-op: already resumed"),
        "{:?}",
        rows[0].new_value
    );
    assert_eq!(rows[1].new_value.as_deref(), Some("SourceFinalized"));
}

/// A fresh, unconfigured database must produce the actionable
/// "not initialized" message on both surfaces — not a redacted storage
/// error (the pre-read regression the second review caught).
#[tokio::test]
async fn an_unconfigured_reserve_reports_not_initialized_not_a_storage_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    // Deliberately NOT configure_ledger().
    let mut ledger = Ledger::open(&db_path).unwrap();
    let err = audited_set_local_pause(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        true,
        "note",
        "alice",
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("has not been initialized"),
        "operators on a fresh database need the actionable message, got: {err}"
    );
}

/// Notes are normalized to one shape regardless of surface: a padded
/// note audits trimmed, exactly as the HTTP layer's require_note would
/// have stored it.
#[tokio::test]
async fn padded_notes_audit_trimmed_on_every_surface() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let mut ledger = Ledger::open(&db_path).unwrap();
    audited_set_local_pause(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        true,
        "  incident 42  ",
        "cli:reaper",
    )
    .unwrap();
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows[0].note, "incident 42");
}

/// Strict audit filters: an empty value or a typo'd key must 400, never
/// silently return every operator's rows under a heading a reviewer
/// reads as filtered.
#[tokio::test]
async fn audit_query_rejects_empty_values_and_unknown_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let c = client();

    for bad in ["actor=", "action=", "acton=pause", "before=7"] {
        let resp = c
            .get(format!("{base}/audit-log?{bad}"))
            .bearer_auth(ALICE_TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "?{bad} must be rejected");
    }
}

/// Mutation bodies are capped: a multi-gigabyte POST must be refused
/// with 413, not buffered into the settlement daemon's memory.
#[tokio::test]
async fn oversized_mutation_bodies_are_rejected_with_413() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let huge_note = "x".repeat(100 * 1024);
    let resp = client()
        .post(format!("{base}/pause"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"direction":"goldcoin","note":"{huge_note}"}}"#
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);

    let ledger = Ledger::open(&db_path).unwrap();
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
}

// -------------------------------------------- ManualReview refunds --
//
// The console's refund surface is READ-ONLY by construction: a listing, a
// strict dry run, and a generated `glc-admin` command line. Execution
// needs the admin keypair and the attestation signer stack, which this
// API never holds — these tests pin that boundary as much as they pin the
// happy path.

/// A parked request whose reason is on the refund whitelist appears as a
/// refund CANDIDATE, with no destination or refund state yet — those only
/// exist once a refund lifecycle has actually begun and derived them.
#[tokio::test]
async fn refunds_listing_shows_whitelisted_candidates_without_a_destination() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let request_id = park_request(&db_path, 0, 1, now_unix());

    let (base, _shutdown) = spawn_admin_server(&db_path).await;
    let body: serde_json::Value = client()
        .get(format!("{base}/refunds"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let rows = body["refunds"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "the parked request must be listed: {body}");
    let row = &rows[0];
    assert_eq!(row["request_id"].as_i64(), Some(request_id));
    assert_eq!(row["request_state"], "ManualReview");
    assert_eq!(row["direction"], "SolToGlc");
    assert_eq!(row["manual_review_reason"], "reserve_paused_at_fold");
    assert!(
        row["destination_token_account"].is_null(),
        "no destination before a dry run"
    );
    assert!(row["refund_state"].is_null());
    assert!(row["refund_signature"].is_null());
    assert_eq!(row["terminal"], false);
    assert_eq!(row["dry_run_available"], true);
    // The GLC display is derived server-side from the canonical gross.
    assert!(row["gross_amount_display_glc"].is_string());
}

/// A request parked for a reason NOT on the refund whitelist is never
/// offered as a refund candidate — the listing reads the same constant
/// the refund path enforces.
#[tokio::test]
async fn refunds_listing_excludes_non_whitelisted_manual_review_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let request_id = park_request(&db_path, 0, 1, now_unix());
    {
        let ledger = Ledger::open(&db_path).unwrap();
        ledger
            .raw()
            .execute(
                "UPDATE bridge_requests SET manual_review_note = 'deposit_spent_before_finalized'
                 WHERE id = ?1",
                [request_id],
            )
            .unwrap();
    }

    let (base, _shutdown) = spawn_admin_server(&db_path).await;
    let body: serde_json::Value = client()
        .get(format!("{base}/refunds"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["refunds"].as_array().unwrap().len(),
        0,
        "a non-whitelisted reason must never be offered as refundable: {body}"
    );
}

/// The dry run is STRICTLY read-only: it must not create a refund row,
/// change the request's state, or write a single audit entry.
#[tokio::test]
async fn refund_dry_run_mutates_absolutely_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let request_id = park_request(&db_path, 0, 1, now_unix());

    let (base, _shutdown) = spawn_admin_server(&db_path).await;
    let resp = client()
        .get(format!("{base}/refunds/{request_id}/dry-run"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["request_id"].as_i64(), Some(request_id));
    assert!(body["checks"].as_array().is_some_and(|c| !c.is_empty()));
    assert!(body["verdict"].is_string());

    let ledger = Ledger::open(&db_path).unwrap();
    assert!(
        ledger.get_solana_refund(request_id).unwrap().is_none(),
        "a dry run must never create a refund row"
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview,
        "a dry run must never change the request state"
    );
    assert!(
        ledger
            .list_admin_audit(&AdminAuditFilter::default())
            .unwrap()
            .is_empty(),
        "a read-only dry run must not write an audit row"
    );
    assert_eq!(
        ledger.list_solana_refunds(false).unwrap().len(),
        0,
        "no refund lifecycle may exist after a dry run"
    );
}

#[tokio::test]
async fn refund_dry_run_for_an_unknown_request_is_a_404() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _shutdown) = spawn_admin_server(&db_path).await;
    let resp = client()
        .get(format!("{base}/refunds/424242/dry-run"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// THE custody boundary: there is no HTTP path that executes a refund,
/// under any verb or spelling. Execution requires the admin keypair and
/// the attestation signers, which this API never holds.
#[tokio::test]
async fn there_is_no_http_refund_execution_route() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let request_id = park_request(&db_path, 0, 1, now_unix());

    let (base, _shutdown) = spawn_admin_server(&db_path).await;
    for path in [
        format!("/refunds/{request_id}/execute"),
        format!("/refunds/{request_id}/refund"),
        format!("/manual-review/{request_id}/refund"),
        format!("/refunds/{request_id}"),
        "/refunds/execute".to_string(),
    ] {
        for status in [
            client()
                .post(format!("{base}{path}"))
                .bearer_auth(ALICE_TOKEN)
                .json(&serde_json::json!({ "note": "attempt to execute over HTTP" }))
                .send()
                .await
                .unwrap()
                .status(),
            client()
                .get(format!("{base}{path}"))
                .bearer_auth(ALICE_TOKEN)
                .send()
                .await
                .unwrap()
                .status(),
        ] {
            assert_eq!(status, 404, "{path} must not exist");
        }
    }

    // And nothing was created by trying.
    let ledger = Ledger::open(&db_path).unwrap();
    assert!(ledger.get_solana_refund(request_id).unwrap().is_none());
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview
    );
}

/// The generated command carries only the request id and the note
/// placeholder — never a destination and never an amount, because the
/// CLI derives both from the verified on-chain deposit.
#[tokio::test]
async fn refund_cli_command_carries_no_destination_and_no_amount() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let request_id = park_request(&db_path, 0, 1, now_unix());

    let (base, _shutdown) = spawn_admin_server(&db_path).await;
    let body: serde_json::Value = client()
        .post(format!("{base}/cli-command"))
        .bearer_auth(ALICE_TOKEN)
        .json(&serde_json::json!({
            "action": "refund-manual-review",
            "request_id": request_id,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let command = body["command"].as_str().unwrap();
    assert!(
        command.contains("glc-admin refund-manual-review"),
        "{command}"
    );
    assert!(
        command.contains(&format!("--request-id {request_id}")),
        "{command}"
    );
    assert!(command.contains("--execute"), "{command}");
    assert!(
        command.contains("<NOTE>"),
        "the note stays a placeholder: {command}"
    );
    assert!(
        !command.contains("--destination"),
        "a destination must never appear in a refund command: {command}"
    );
    assert!(
        !command.contains("--amount"),
        "an amount must never appear in a refund command: {command}"
    );
    assert_eq!(body["label"], "CLI approval required");
    // The fixture's on-chain config is not paused, so the precondition
    // must be surfaced rather than silently omitted.
    assert!(
        body["precondition"]
            .as_str()
            .is_some_and(|p| p.contains("not globally paused")),
        "an unmet pause precondition must be reported: {body}"
    );
}

/// A caller cannot smuggle a destination or an amount into the refund
/// command by adding fields the schema does not define — unknown JSON is
/// ignored by serde, and the generated command is built only from the
/// request id.
#[tokio::test]
async fn refund_cli_command_ignores_caller_supplied_destination_and_amount() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let request_id = park_request(&db_path, 0, 1, now_unix());

    let (base, _shutdown) = spawn_admin_server(&db_path).await;
    let body: serde_json::Value = client()
        .post(format!("{base}/cli-command"))
        .bearer_auth(ALICE_TOKEN)
        .json(&serde_json::json!({
            "action": "refund-manual-review",
            "request_id": request_id,
            "destination": "AttackerOwnedTokenAccount1111111111111111111",
            "destination_token_account": "AttackerOwnedTokenAccount1111111111111111111",
            "amount": 999_999_999,
            "value_glc": "999999",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let command = body["command"].as_str().unwrap();
    assert!(
        !command.contains("Attacker"),
        "a caller-supplied destination must never reach the command: {command}"
    );
    assert!(
        !command.contains("999999") && !command.contains("999_999_999"),
        "a caller-supplied amount must never reach the command: {command}"
    );
}

#[tokio::test]
async fn refund_cli_command_requires_a_positive_request_id() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _shutdown) = spawn_admin_server(&db_path).await;
    for body in [
        serde_json::json!({ "action": "refund-manual-review" }),
        serde_json::json!({ "action": "refund-manual-review", "request_id": 0 }),
        serde_json::json!({ "action": "refund-manual-review", "request_id": -3 }),
    ] {
        let status = client()
            .post(format!("{base}/cli-command"))
            .bearer_auth(ALICE_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 400, "must reject: {body}");
    }
}

/// A confirmed refund is terminal: it stays listed for the audit trail,
/// carries its transaction signature, and offers no further action.
#[tokio::test]
async fn a_confirmed_refund_is_listed_as_terminal_with_its_signature() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let request_id = park_request(&db_path, 0, 1, now_unix());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        let request = ledger.get_request(request_id).unwrap().unwrap();
        let verified = crate::ledger::VerifiedRefundInputs {
            obligation_index: request.source_obligation_index.unwrap(),
            amount_solana_atomic: 1_000,
            gross_canonical_atomic: request.gross_amount_atomic,
            requester: request.requester.unwrap(),
            destination_token_account: [0xDD; 32],
            reserve_mint: [0xEE; 32],
            token_program: [0xFF; 32],
        };
        ledger
            .begin_solana_refund(
                request_id,
                &verified,
                "console test",
                "cli:test",
                now_unix(),
            )
            .unwrap();
        ledger
            .record_solana_refund_broadcast(request_id, "TESTSIG", "TESTHASH", 0, now_unix())
            .unwrap();
        ledger
            .mark_solana_refund_confirmed(request_id, now_unix())
            .unwrap();
    }

    let (base, _shutdown) = spawn_admin_server(&db_path).await;
    let body: serde_json::Value = client()
        .get(format!("{base}/refunds"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let rows = body["refunds"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["request_state"], "Refunded");
    assert_eq!(row["refund_state"], "Confirmed");
    assert_eq!(row["refund_signature"], "TESTSIG");
    assert_eq!(row["terminal"], true);
    assert_eq!(
        row["dry_run_available"], false,
        "a terminal refund must offer no further action"
    );
    // The destination is present and is the one the LEDGER recorded.
    assert!(row["destination_token_account"].is_string());
}

// ---------------------------------------------------------------------------
// The ONE fund-moving route: POST /refunds/glc/{id}/execute
//
// These tests are about the CAPABILITY GATE, not about the refund itself
// (that is covered in `goldcoin::refund::tests`). The executor is a stub
// that records calls, so "did the request reach the executor?" is a
// precise, observable question.
// ---------------------------------------------------------------------------

use crate::admin_api::glc_refund_exec::tests::RecordingExecutor;

/// Spawns a server whose refund executor is a recording stub, with a
/// registry whose capabilities the caller chooses.
async fn spawn_refund_server(
    db_path: &std::path::Path,
    alice_may_execute: bool,
    bob_may_execute: bool,
) -> (
    String,
    Arc<RecordingExecutor>,
    tokio::sync::watch::Sender<bool>,
) {
    let (listener, port) = bound_listener().await;
    let (tx, rx) = tokio::sync::watch::channel(false);
    let executor = Arc::new(RecordingExecutor::new());
    let source = Arc::new(
        AdminApi::new(db_path.to_path_buf(), FakeSolanaRpc)
            .with_refund_executor(Arc::clone(&executor)
                as Arc<dyn crate::admin_api::glc_refund_exec::GlcRefundExecutor>),
    );
    let registry = Arc::new(
        OperatorRegistry::new(vec![
            crate::admin_api::auth::ResolvedOperator {
                name: "alice".to_string(),
                token: AdminAuthToken::for_tests(ALICE_TOKEN),
                may_execute_glc_refunds: alice_may_execute,
            },
            crate::admin_api::auth::ResolvedOperator {
                name: "bob".to_string(),
                token: AdminAuthToken::for_tests(BOB_TOKEN),
                may_execute_glc_refunds: bob_may_execute,
            },
        ])
        .unwrap(),
    );
    tokio::spawn(async move {
        // `serve_on` cannot fail to bind — the listener is already ours.
        let _ = serve_on(listener, source, registry, rx).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    let c = reqwest::Client::new();
    for _ in 0..100 {
        if c.get(format!("{base}/fee"))
            .bearer_auth(ALICE_TOKEN)
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (base, executor, tx)
}

#[tokio::test]
async fn an_allow_listed_operator_may_execute_a_refund() {
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().join("l.db");
    configure_ledger(&db_path);
    let (base, executor, _tx) = spawn_refund_server(&db_path, true, false).await;

    let resp = client()
        .post(format!("{base}/refunds/glc/42/execute"))
        .bearer_auth(ALICE_TOKEN)
        .json(&serde_json::json!({"note": "incident refund"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["request_id"], 42);
    assert_eq!(body["action"], "broadcast");
    // The daemon attributes the action to the authenticated operator, not
    // to anything the caller supplied.
    assert_eq!(body["actor"], "alice");
    assert_eq!(executor.call_count(), 1);
    assert_eq!(
        executor.calls.lock().unwrap()[0],
        (42, "incident refund".to_string(), "alice".to_string())
    );
}

#[tokio::test]
async fn an_ordinary_admin_token_is_not_enough_to_execute_a_refund() {
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().join("l.db");
    configure_ledger(&db_path);
    let (base, executor, _tx) = spawn_refund_server(&db_path, true, false).await;

    // Bob is a fully valid admin operator — every other endpoint works
    // for him — but he is not on the refund-execution allow-list.
    let ok = client()
        .get(format!("{base}/status"))
        .bearer_auth(BOB_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200, "bob is a valid admin operator");

    let resp = client()
        .post(format!("{base}/refunds/glc/42/execute"))
        .bearer_auth(BOB_TOKEN)
        .json(&serde_json::json!({"note": "incident refund"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("allow-list"),
        "got: {body}"
    );
    assert_eq!(
        executor.call_count(),
        0,
        "a non-allow-listed operator must never reach the executor"
    );
}

#[tokio::test]
async fn refund_execution_is_refused_outright_when_no_operator_holds_the_capability() {
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().join("l.db");
    configure_ledger(&db_path);
    // Nobody has it: a deployment that never opted in.
    let (base, executor, _tx) = spawn_refund_server(&db_path, false, false).await;

    let resp = client()
        .post(format!("{base}/refunds/glc/42/execute"))
        .bearer_auth(ALICE_TOKEN)
        .json(&serde_json::json!({"note": "incident refund"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("not enabled"),
        "an empty allow-list must fail closed, got: {body}"
    );
    assert_eq!(executor.call_count(), 0);
}

#[tokio::test]
async fn an_unauthenticated_caller_cannot_execute_a_refund() {
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().join("l.db");
    configure_ledger(&db_path);
    let (base, executor, _tx) = spawn_refund_server(&db_path, true, false).await;

    for (label, req) in [
        (
            "no token",
            client().post(format!("{base}/refunds/glc/42/execute")),
        ),
        (
            "wrong token",
            client()
                .post(format!("{base}/refunds/glc/42/execute"))
                .bearer_auth("not-a-real-token"),
        ),
    ] {
        let resp = req
            .json(&serde_json::json!({"note": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "{label} must be unauthorized");
    }
    assert_eq!(executor.call_count(), 0);
}

#[tokio::test]
async fn a_browser_originated_refund_execution_is_refused_before_auth() {
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().join("l.db");
    configure_ledger(&db_path);
    let (base, executor, _tx) = spawn_refund_server(&db_path, true, false).await;

    let resp = client()
        .post(format!("{base}/refunds/glc/42/execute"))
        .bearer_auth(ALICE_TOKEN)
        .header(reqwest::header::ORIGIN, "https://evil.example")
        .json(&serde_json::json!({"note": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(executor.call_count(), 0);
}

#[tokio::test]
async fn a_refund_execution_without_a_note_is_refused() {
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().join("l.db");
    configure_ledger(&db_path);
    let (base, executor, _tx) = spawn_refund_server(&db_path, true, false).await;

    for body in [
        serde_json::json!({"note": ""}),
        serde_json::json!({"note": "   "}),
    ] {
        let resp = client()
            .post(format!("{base}/refunds/glc/42/execute"))
            .bearer_auth(ALICE_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_client_error(),
            "an empty note must be refused: {body}"
        );
    }
    assert_eq!(executor.call_count(), 0);
}

#[tokio::test]
async fn executing_request_a_never_advances_request_b() {
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().join("l.db");
    configure_ledger(&db_path);
    let (base, executor, _tx) = spawn_refund_server(&db_path, true, false).await;

    client()
        .post(format!("{base}/refunds/glc/7/execute"))
        .bearer_auth(ALICE_TOKEN)
        .json(&serde_json::json!({"note": "only seven"}))
        .send()
        .await
        .unwrap();

    let calls = executor.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "exactly one refund may be touched");
    assert_eq!(calls[0].0, 7, "and it must be the one that was asked for");
}

#[tokio::test]
async fn the_route_is_absent_when_no_executor_was_wired() {
    // The default AdminApi — every construction except the daemon's —
    // holds no executor and therefore cannot move funds at all.
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().join("l.db");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let resp = client()
        .post(format!("{base}/refunds/glc/42/execute"))
        .bearer_auth(ALICE_TOKEN)
        .json(&serde_json::json!({"note": "x"}))
        .send()
        .await
        .unwrap();
    assert!(
        !resp.status().is_success(),
        "an AdminApi without an executor must never move funds"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not wired"),
        "got: {body}"
    );
}

#[tokio::test]
async fn the_solana_refund_route_still_has_no_execute_counterpart() {
    // The Goldcoin route must not have quietly opened a Solana one: that
    // still requires the admin keypair on the operator's own machine.
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().join("l.db");
    configure_ledger(&db_path);
    let (base, executor, _tx) = spawn_refund_server(&db_path, true, false).await;

    let resp = client()
        .post(format!("{base}/refunds/42/execute"))
        .bearer_auth(ALICE_TOKEN)
        .json(&serde_json::json!({"note": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(executor.call_count(), 0);
}

// -------------------------------------------------- route ledger state --

/// The audited operator write behind `glc-admin robinhood-route-enable`.
/// One gate of three, and it must leave the same audit trail every other
/// mutation on this surface leaves.
#[test]
fn enabling_a_robinhood_route_is_audited_with_both_values() {
    let mut ledger = Ledger::open_in_memory().unwrap();

    let receipt = audited_set_route_enabled(
        &mut ledger,
        crate::routes::Route::GlcToRhn,
        true,
        "  launch window 3  ",
        "alice",
    )
    .unwrap();

    assert_eq!(receipt.action, "route_enable");
    assert_eq!(receipt.target, "GlcToRhn");
    assert_eq!(receipt.old_value.as_deref(), Some("enabled=false"));
    assert_eq!(receipt.new_value.as_deref(), Some("enabled=true"));
    assert!(ledger.route_enabled("GlcToRhn", false).unwrap());

    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].actor, "alice");
    assert_eq!(rows[0].action, "route_enable");
    assert_eq!(rows[0].target.as_deref(), Some("GlcToRhn"));
    // Trimmed to one shape, as on every other audited surface.
    assert_eq!(rows[0].note, "launch window 3");
    assert_eq!(rows[0].outcome, AdminAuditOutcome::Success);
}

#[test]
fn a_refused_route_write_is_audited_and_changes_nothing() {
    // The restriction lives inside the audited scope, so "an operator
    // tried to switch a route they may not switch" is itself recorded —
    // the same discipline admission's direction check follows.
    let mut ledger = Ledger::open_in_memory().unwrap();

    let err = audited_set_route_enabled(
        &mut ledger,
        crate::routes::Route::GlcToSol,
        false,
        "close production",
        "mallory",
    )
    .unwrap_err();
    assert!(
        matches!(err, AdminError::Conflict(ref m) if m.contains("not operator-settable")),
        "expected a validated refusal, got {err:?}"
    );

    assert!(
        ledger.route_enabled("GlcToSol", false).unwrap(),
        "the refused write must not have closed production traffic"
    );
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "a refusal is audit-relevant and must be kept"
    );
    assert_eq!(rows[0].actor, "mallory");
    assert!(matches!(rows[0].outcome, AdminAuditOutcome::Error(_)));
}

// ------------------------------------- Robinhood per-transfer maximum --

/// The approved Robinhood policy, at the documented production figures
/// (docs/robinhood/mainnet-deployment.md's limits table). Canonical 8dp.
fn approved_policy() -> crate::chain_policy::ChainPolicy {
    crate::chain_policy::ChainPolicy::new(
        crate::routes::Chain::Robinhood,
        600,                                    // 6.00%
        CanonicalAtomic(2_000_000_000_000),     // 20,000 GLC per transfer
        CanonicalAtomic(1_000_000_000_000_000), // 10,000,000 GLC strict 24h
    )
    .expect("the documented production policy is valid")
}

fn robinhood_context(policy: Option<crate::chain_policy::ChainPolicy>) -> RobinhoodAdminContext {
    RobinhoodAdminContext {
        route_gate: Arc::new(crate::routes::RouteGate::new(
            crate::routes::RoutesConfig::default(),
            crate::chains::ChainRegistry::default(),
        )),
        readiness: crate::robinhood::admin::RobinhoodReadiness::default(),
        policy,
    }
}

async fn robinhood_status(policy: Option<crate::chain_policy::ChainPolicy>) -> AdminStatusView {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    AdminApi::new(db_path, FakeSolanaRpc)
        .with_robinhood(robinhood_context(policy))
        .status()
        .await
        .expect("status is a pure ledger projection here")
}

#[tokio::test]
async fn every_robinhood_route_reports_the_approved_per_transfer_maximum() {
    let view = robinhood_status(Some(approved_policy())).await;

    assert_eq!(view.robinhood_routes.len(), 4, "all four routes are listed");
    for route in &view.robinhood_routes {
        assert_eq!(
            route.per_transfer_limit_atomic,
            Some(2_000_000_000_000),
            "{} must report the configured per-transfer ceiling",
            route.route
        );
    }
}

#[tokio::test]
async fn the_maximum_is_the_configured_policy_and_not_the_solana_limit() {
    // `FakeSolanaRpc`'s `BridgeConfig` carries `per_transfer_limit =
    // 10_000_000_000` — a DIFFERENT number in a different program's
    // units, which `/onchain` reports and which must never leak onto a
    // Robinhood route. The two endpoints describe two chains.
    let view = robinhood_status(Some(approved_policy())).await;

    for route in &view.robinhood_routes {
        assert_ne!(
            route.per_transfer_limit_atomic,
            Some(10_000_000_000),
            "{} is reporting the SOLANA program's ceiling",
            route.route
        );
    }
}

#[tokio::test]
async fn an_unconfigured_policy_reports_null_rather_than_zero() {
    // A deployment with a Robinhood indexer and no `[robinhood.policy]`.
    // Zero would say "this route accepts nothing", which is a different
    // claim from "nobody has approved a ceiling".
    let view = robinhood_status(None).await;

    assert_eq!(view.robinhood_routes.len(), 4);
    for route in &view.robinhood_routes {
        assert_eq!(
            route.per_transfer_limit_atomic, None,
            "{} must not invent a ceiling",
            route.route
        );
    }
}

#[tokio::test]
async fn the_maximum_is_absent_entirely_on_a_deployment_without_robinhood() {
    // No `with_robinhood` at all: the pre-existing shape, unchanged.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let view = AdminApi::new(db_path, FakeSolanaRpc)
        .status()
        .await
        .unwrap();

    assert!(view.robinhood_routes.is_empty());
    let json = serde_json::to_value(&view).unwrap();
    assert!(
        json.get("robinhood_routes").is_none(),
        "an existing operator console must see exactly the response it always did"
    );
}

#[tokio::test]
async fn the_maximum_serialises_under_its_documented_name_and_unit() {
    let view = robinhood_status(Some(approved_policy())).await;
    let json = serde_json::to_value(&view).unwrap();
    let routes = json["robinhood_routes"].as_array().unwrap();

    let glc_to_rhn = routes
        .iter()
        .find(|r| r["route"] == "GlcToRhn")
        .expect("GlcToRhn is listed");
    assert_eq!(
        glc_to_rhn["per_transfer_limit_atomic"].as_u64().unwrap(),
        2_000_000_000_000,
        "canonical 8dp, not Robinhood's native 18"
    );

    let rhn_to_glc = routes
        .iter()
        .find(|r| r["route"] == "RhnToGlc")
        .expect("RhnToGlc is listed");
    assert_eq!(
        rhn_to_glc["per_transfer_limit_atomic"], glc_to_rhn["per_transfer_limit_atomic"],
        "policy is keyed by chain, so both directions state one ceiling"
    );
}

#[tokio::test]
async fn the_per_transfer_maximum_is_not_a_rolling_figure() {
    // The two ceilings are independent, and this pins that the per-tx
    // field never carries the rolling one: a policy whose strict daily
    // budget is five hundred times its per-transfer maximum must report
    // the per-transfer maximum here, so a console cannot label one as
    // the other.
    let view = robinhood_status(Some(approved_policy())).await;

    for route in &view.robinhood_routes {
        assert_eq!(route.per_transfer_limit_atomic, Some(2_000_000_000_000));
        assert_ne!(
            route.per_transfer_limit_atomic,
            Some(1_000_000_000_000_000),
            "{} is reporting the rolling daily limit as a per-transfer one",
            route.route
        );
    }
}

#[tokio::test]
async fn close_manual_review_records_a_terminal_disposition_with_its_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let now = now_unix();
    let request_id = park_request(&db_path, 1, 10, now - 100);
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_manual_review_hold(request_id, None, "snapshot freeze", "cli:ops", now - 50)
            .unwrap();
    }
    let (base, _tx) = spawn_admin_server(&db_path).await;

    // The retained-principal CANCEL is a known disposition but
    // feature-flagged OFF: the ledger refuses it (409) naming the Terms,
    // and nothing is written.
    let resp = client()
        .post(format!("{base}/manual-review/{request_id}/close"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"disposition":"retained_per_terms","reference":"LEGAL-1","note":"abuse"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let text = resp.text().await.unwrap();
    assert!(text.contains("not enabled on this deployment"), "{text}");
    assert!(text.contains("published Terms"), "{text}");
    let resp = client()
        .post(format!("{base}/manual-review/{request_id}/close"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"disposition":"void","reference":"x","note":"n"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("refunded_out_of_band | retained_per_terms | reconciled_to_chain"),
        "{text}"
    );

    // A missing reference is refused by the ledger (409), nothing written.
    let resp = client()
        .post(format!("{base}/manual-review/{request_id}/close"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"disposition":"reconciled_to_chain","reference":" ","note":"chain closed it"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "{}", resp.text().await.unwrap());
    assert_eq!(
        Ledger::open(&db_path)
            .unwrap()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::ManualReview
    );

    // The closure goes through, audited with the reference.
    let resp = client()
        .post(format!("{base}/manual-review/{request_id}/close"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"disposition":"reconciled_to_chain","reference":"LEGAL-2026-09-13-04","note":"obligation closed on chain by governance; ledger reconciled"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    let ledger = Ledger::open(&db_path).unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::Closed);
    let closure = ledger.request_closure(request_id).unwrap().unwrap();
    assert_eq!(
        closure.disposition,
        crate::ledger::ClosureDisposition::ReconciledToChain
    );
    assert_eq!(closure.reference, "LEGAL-2026-09-13-04");
    assert!(closure.actor.contains("alice"), "{}", closure.actor);
    let audit = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    let row = audit
        .iter()
        .find(|r| r.action == "manual_review_close")
        .expect("an audit row");
    assert!(row
        .new_value
        .as_deref()
        .unwrap()
        .contains("reference=LEGAL-2026-09-13-04"));

    // Listed, and gone from the ManualReview queue.
    let listing: serde_json::Value = client()
        .get(format!("{base}/manual-review/closures"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let c = &listing["closures"][0];
    assert_eq!(c["request_id"], request_id);
    assert_eq!(c["disposition"], "reconciled_to_chain");
    assert_eq!(c["reference"], "LEGAL-2026-09-13-04");
    assert_eq!(c["from_state"], "ManualReview");
    assert_eq!(c["manual_review_disposition"], "operator_hold");
    let queue: serde_json::Value = client()
        .get(format!("{base}/manual-review"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(queue["requests"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["request_id"] != request_id));

    // Idempotent: the same disposition again is a 200 no-op.
    let resp = client()
        .post(format!("{base}/manual-review/{request_id}/close"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"disposition":"reconciled_to_chain","reference":"LEGAL-2026-09-13-04","note":"again"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(ledger.request_closures(10).unwrap().len(), 1);
}

/// The v33 auto-resume switch over the admin API: `GET` reports the
/// default (false, never set), `PUT` needs a note and is audited with
/// actor / old / new, `GET /manual-review` carries the switch and each
/// row's class + block reason, and `/status` reports both policy flags.
#[tokio::test]
async fn manual_review_auto_resume_setting_is_read_written_and_audited_over_the_api() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let now = now_unix();
    let technical = park_request(&db_path, 1, 10, now - 100);
    let held = park_request(&db_path, 2, 11, now - 90);
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_manual_review_hold(held, None, "TOS violation", "cli:ops", now - 50)
            .unwrap();
    }
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let v: serde_json::Value = client()
        .get(format!("{base}/settings/manual-review-auto-resume"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["enabled"], false);
    assert!(v["updated_by"].is_null());
    assert!(v["never_auto_resumed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x == "rapid_burst_hold"));
    assert!(v["eligible_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x == "liquidity_buffer_low_at_fold"));

    let s: serde_json::Value = client()
        .get(format!("{base}/status"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["manual_review_auto_resume_enabled"], false);
    assert_eq!(s["abuse_detection_enabled"], false);

    let mr: serde_json::Value = client()
        .get(format!("{base}/manual-review"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mr["auto_resume_enabled"], false);
    let row = |id: i64| {
        mr["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["request_id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(row(technical)["manual_review_class"], "technical");
    assert_eq!(row(technical)["auto_resume_eligible"], false);
    assert_eq!(
        row(technical)["auto_resume_block_reason"],
        "global_disabled"
    );
    assert_eq!(row(held)["manual_review_class"], "operator_hold");
    assert_eq!(row(held)["auto_resume_block_reason"], "operator_hold");

    // PUT without a note: refused; nothing changes.
    let resp = client()
        .put(format!("{base}/settings/manual-review-auto-resume"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"enabled":true,"note":"  "}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let resp = client()
        .put(format!("{base}/settings/manual-review-auto-resume"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"enabled":true,"note":"liquidity restored; draining technical parks"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let receipt: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(receipt["action"], "manual_review_auto_resume");
    assert_eq!(receipt["old_value"], "false");
    assert_eq!(receipt["new_value"], "true");

    let v: serde_json::Value = client()
        .get(format!("{base}/settings/manual-review-auto-resume"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["enabled"], true);
    assert_eq!(v["updated_by"], "alice");

    // With the switch ON the technical park's reason is on the allowlist
    // (`reserve_paused_at_fold` is NOT — it stays blocked as
    // unsupported_reason), the held row stays blocked by its hold.
    let mr: serde_json::Value = client()
        .get(format!("{base}/manual-review"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mr["auto_resume_enabled"], true);
    let row = |id: i64| {
        mr["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["request_id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        row(technical)["auto_resume_block_reason"],
        "unsupported_reason"
    );
    assert_eq!(row(held)["auto_resume_block_reason"], "operator_hold");

    let audit: serde_json::Value = client()
        .get(format!("{base}/audit-log?limit=10"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = audit["rows"].as_array().unwrap();
    let e = entries
        .iter()
        .find(|e| e["action"] == "manual_review_auto_resume")
        .expect("audited");
    assert_eq!(e["actor"], "alice");
    assert_eq!(e["old_value"], "false");
    assert_eq!(e["new_value"], "true");
}

/// A closure written by the manual-refund import (schema v35) carries the
/// verified refund on `GET /manual-review/closures`, and `GET
/// /manual-refunds` lists it — refund wallet, batch, importer, signature.
#[tokio::test]
async fn manual_refund_closures_carry_the_verified_refund_and_are_listed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let now = now_unix();
    let request_id = park_request(&db_path, 1, 10, now - 100);
    let requester = wallet(10);
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        let (outcome, receipt) = audited_manual_refund_import(
            &mut ledger,
            request_id,
            &crate::ledger::ManualSolanaRefundInputs {
                batch_id: "mrb-20260914T120000Z-abcd1234".into(),
                mint: [0xaa; 32],
                refund_wallet: [0x11; 32],
                recipient: requester,
                recipient_token_account: [0x22; 32],
                amount_atomic: 100_000_000_000,
                amount_canonical_atomic: 100_000,
                tx_signature: "5VERYrealSIGNATURE".into(),
                slot: 446_700_000,
                block_time: Some(now - 30),
                submitted_at: Some(now - 60),
                finalized_at: Some(now - 20),
            },
            "backlog batch 1",
            "cli:ops",
        )
        .unwrap();
        assert!(matches!(
            outcome,
            crate::ledger::ManualRefundRecordOutcome::Recorded(..)
        ));
        assert_eq!(receipt.action, "manual_refund_import");
    }
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let listing: serde_json::Value = client()
        .get(format!("{base}/manual-review/closures"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let c = &listing["closures"][0];
    assert_eq!(c["request_id"], request_id);
    assert_eq!(c["disposition"], "refunded_out_of_band");
    assert_eq!(c["reference"], "5VERYrealSIGNATURE");
    let mr = &c["manual_refund"];
    assert_eq!(mr["tx_signature"], "5VERYrealSIGNATURE");
    assert_eq!(mr["batch_id"], "mrb-20260914T120000Z-abcd1234");
    assert_eq!(
        mr["refund_wallet"],
        solana_sdk::pubkey::Pubkey::new_from_array([0x11; 32]).to_string()
    );
    assert_eq!(
        mr["recipient"],
        solana_sdk::pubkey::Pubkey::new_from_array(requester).to_string()
    );
    assert_eq!(mr["amount_atomic"], "100000000000");
    assert_eq!(mr["amount_canonical_atomic"], "100000");
    assert_eq!(mr["network"], "solana");
    assert_eq!(mr["slot"], 446_700_000);
    assert_eq!(mr["imported_by"], "cli:ops");
    assert_eq!(mr["note"], "backlog batch 1");

    let refunds: serde_json::Value = client()
        .get(format!("{base}/manual-refunds"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(refunds["refunds"].as_array().unwrap().len(), 1);
    assert_eq!(refunds["refunds"][0]["request_id"], request_id);
    assert_eq!(refunds["refunds"][0]["tx_signature"], "5VERYrealSIGNATURE");

    // An ordinary closure carries no manual_refund.
    let other = park_request(&db_path, 2, 11, now - 90);
    Ledger::open(&db_path)
        .unwrap()
        .close_manual_review(
            other,
            crate::ledger::ClosureDisposition::ReconciledToChain,
            "chain-tx",
            "n",
            "cli:ops",
            now,
        )
        .unwrap();
    let listing: serde_json::Value = client()
        .get(format!("{base}/manual-review/closures"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plain = listing["closures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["request_id"] == other)
        .unwrap();
    assert!(plain["manual_refund"].is_null());
}

// ------------------------------ admin console v2: routes and admission --

const ALL_ROUTES: [&str; 6] = [
    "GlcToSol", "SolToGlc", "GlcToRhn", "RhnToGlc", "SolToRhn", "RhnToSol",
];

async fn routes_view(base: &str) -> serde_json::Value {
    let resp = client()
        .get(format!("{base}/routes"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

fn route_of<'a>(view: &'a serde_json::Value, route: &str) -> &'a serde_json::Value {
    view["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["route"] == route)
        .unwrap_or_else(|| panic!("{route} missing from GET /routes"))
}

fn blockers(route: &serde_json::Value) -> Vec<String> {
    route["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_str().unwrap().to_string())
        .collect()
}

async fn post_note(base: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    let resp = client()
        .post(format!("{base}{path}"))
        .bearer_auth(ALICE_TOKEN)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

/// `GET /routes` lists every route with the fields the console keys on,
/// and no route's `available` disagrees with what its blockers say.
#[tokio::test]
async fn routes_view_lists_all_six_routes_with_consistent_verdicts() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let view = routes_view(&base).await;
    let routes = view["routes"].as_array().unwrap();
    assert_eq!(
        routes
            .iter()
            .map(|r| r["route"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ALL_ROUTES
    );
    for r in routes {
        let available = r["available"].as_bool().unwrap();
        let blockers = blockers(r);
        assert_eq!(
            available,
            blockers.is_empty(),
            "{}: available={available} but blockers={blockers:?}",
            r["route"]
        );
        if !available {
            assert!(
                r["primary_reason"].is_string(),
                "{}: an unavailable route names a primary reason",
                r["route"]
            );
        }
        assert!(r["reserve_siblings"].is_array());
        assert!(r["destination_reserve"].is_string());
        assert_eq!(
            r["route_admission_closed"], false,
            "{} seeded open",
            r["route"]
        );
    }
    // The fake program reports release_paused: the two Solana-bound
    // routes are blocked on chain, and the view says which layer.
    for route in ["GlcToSol", "RhnToSol"] {
        let r = route_of(&view, route);
        assert_eq!(r["onchain_blocked"], true);
        assert!(blockers(r).contains(&"onchain_paused".to_string()));
    }
    // The Solana-sourced routes are not.
    assert_eq!(route_of(&view, "SolToGlc")["onchain_blocked"], false);
    // No Robinhood reader on this server: the contract is reported as
    // not configured, never as enabled or paused.
    assert_eq!(view["robinhood_contract"]["availability"], "not_configured");
    assert!(route_of(&view, "GlcToRhn")["contract_route_enabled"].is_null());
    // The unconfigured Robinhood reserve is reported as exactly that.
    assert_eq!(route_of(&view, "GlcToRhn")["reserve_not_configured"], true);
    // Siblings: the two routes drawing on the Solana reserve name each
    // other and nobody else.
    assert_eq!(
        route_of(&view, "GlcToSol")["reserve_siblings"],
        serde_json::json!(["RhnToSol"])
    );
    assert_eq!(
        route_of(&view, "SolToGlc")["reserve_siblings"],
        serde_json::json!(["RhnToGlc"])
    );
    assert_eq!(view["solana_program"]["release_paused"], true);
}

/// The six-route isolation matrix: closing ONE route's admission adds
/// exactly one blocker to exactly that route, every other route's
/// blocker list is byte-identical before and after, and re-opening
/// restores the original view. One audit row per mutation, success or
/// refusal.
#[tokio::test]
async fn closing_one_routes_admission_touches_exactly_that_route() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    configure_robinhood_reserve(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let baseline = routes_view(&base).await;
    let mut audit_rows_expected = 0usize;
    for route in ALL_ROUTES {
        let (status, receipt) = post_note(
            &base,
            &format!("/routes/{route}/admission/close"),
            r#"{"note":"matrix"}"#,
        )
        .await;
        assert_eq!(status, 200, "{route}: {receipt}");
        assert_eq!(receipt["action"], "route_admission_close");
        assert_eq!(receipt["target"], route);
        assert_eq!(receipt["old_value"], "admission_closed=false");
        assert_eq!(receipt["new_value"], "admission_closed=true");
        audit_rows_expected += 1;

        let closed = routes_view(&base).await;
        for other in ALL_ROUTES {
            let before = route_of(&baseline, other);
            let after = route_of(&closed, other);
            if other == route {
                assert_eq!(after["route_admission_closed"], true);
                assert_eq!(after["available"], false);
                assert_eq!(after["route_admission_reason"], "matrix");
                let mut expected = blockers(before);
                // Ranked right after enablement, before the reserve gates.
                let at = if expected.first().is_some_and(|b| b == "route_disabled") {
                    1
                } else {
                    0
                };
                expected.insert(at, "route_admission_closed".to_string());
                assert_eq!(blockers(after), expected, "{route} after closing itself");
            } else {
                assert_eq!(
                    after["route_admission_closed"], false,
                    "{other} after closing {route}"
                );
                assert_eq!(
                    blockers(after),
                    blockers(before),
                    "{other} must be untouched by closing {route}"
                );
                assert_eq!(after["available"], before["available"]);
            }
        }

        let (status, receipt) = post_note(
            &base,
            &format!("/routes/{route}/admission/open"),
            r#"{"note":"matrix over"}"#,
        )
        .await;
        assert_eq!(status, 200, "{route}: {receipt}");
        assert_eq!(receipt["action"], "route_admission_open");
        assert_eq!(receipt["old_value"], "admission_closed=true");
        assert_eq!(receipt["new_value"], "admission_closed=false");
        let reopened = routes_view(&base).await;
        for other in ALL_ROUTES {
            assert_eq!(
                blockers(route_of(&reopened, other)),
                blockers(route_of(&baseline, other)),
                "{other} restored after re-opening {route}"
            );
        }
        audit_rows_expected += 1;
    }

    let ledger = Ledger::open(&db_path).unwrap();
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter {
            limit: Some(100),
            ..AdminAuditFilter::default()
        })
        .unwrap();
    assert_eq!(rows.len(), audit_rows_expected);
    assert!(rows.iter().all(|r| r.actor == "alice"));
    assert!(rows.iter().all(|r| r.outcome == AdminAuditOutcome::Success));
}

fn configure_robinhood_reserve(db_path: &std::path::Path) {
    let mut ledger = Ledger::open(db_path).unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::RobinhoodReserve,
            1_000_000_000,
            1_000,
            900_000_000,
            500_000_000,
            100_000,
            0,
        )
        .unwrap();
}

/// Opening a route whose destination reserve cannot pass the safety
/// checks — here: a Robinhood reserve that was never configured — is
/// refused (409), leaves the gate CLOSED, and the refusal is audited.
/// Closing needed no such check.
#[tokio::test]
async fn opening_a_route_onto_an_unconfigured_reserve_is_refused_and_audited() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let (status, _) = post_note(
        &base,
        "/routes/GlcToRhn/admission/close",
        r#"{"note":"close"}"#,
    )
    .await;
    assert_eq!(status, 200);
    let (status, body) = post_note(
        &base,
        "/routes/GlcToRhn/admission/open",
        r#"{"note":"open"}"#,
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("refusing to open admission for route GlcToRhn"));

    let view = routes_view(&base).await;
    assert_eq!(route_of(&view, "GlcToRhn")["route_admission_closed"], true);
    let ledger = Ledger::open(&db_path).unwrap();
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].action, "route_admission_open");
    assert!(matches!(rows[0].outcome, AdminAuditOutcome::Error(_)));
    assert_eq!(rows[1].action, "route_admission_close");
    assert_eq!(rows[1].outcome, AdminAuditOutcome::Success);
}

/// An unknown route, an unknown verb and a missing note are refused
/// before anything is written.
#[tokio::test]
async fn route_admission_endpoint_validates_route_verb_and_note() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let (status, _) = post_note(&base, "/routes/GlcToGlc/admission/close", r#"{"note":"x"}"#).await;
    assert_eq!(status, 404);
    let (status, _) = post_note(
        &base,
        "/routes/GlcToSol/admission/toggle",
        r#"{"note":"x"}"#,
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = post_note(&base, "/routes/GlcToSol/admission/close", r#"{"note":""}"#).await;
    assert_eq!(status, 400);
    let (status, _) = post_note(&base, "/routes/GlcToSol/admission/close", r#"{}"#).await;
    assert_eq!(status, 400);

    let ledger = Ledger::open(&db_path).unwrap();
    assert!(!ledger
        .route_admission_closed(crate::routes::Route::GlcToSol)
        .unwrap());
    assert!(ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap()
        .is_empty());
}

/// Pausing a reserve blocks exactly the routes drawing on it — the
/// `reserve_siblings` the view advertises — and no other; resuming
/// restores the baseline. Pause and admission remain distinguishable
/// in the blockers.
#[tokio::test]
async fn pausing_a_reserve_blocks_exactly_its_routes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let baseline = routes_view(&base).await;

    for (direction, expected_routes) in [
        ("solana", vec!["GlcToSol", "RhnToSol"]),
        ("goldcoin", vec!["SolToGlc", "RhnToGlc"]),
    ] {
        let (status, receipt) = post_note(
            &base,
            "/pause",
            &format!(r#"{{"direction":"{direction}","note":"stop"}}"#),
        )
        .await;
        assert_eq!(status, 200, "{receipt}");
        let paused = routes_view(&base).await;
        for route in ALL_ROUTES {
            let r = route_of(&paused, route);
            let has = blockers(r).contains(&"reserve_paused".to_string());
            assert_eq!(
                has,
                expected_routes.contains(&route),
                "{route} while {direction} is paused"
            );
            assert!(
                !blockers(r).contains(&"route_admission_closed".to_string()),
                "a pause is never reported as a closed admission"
            );
            if expected_routes.contains(&route) {
                assert_eq!(r["reserve_paused"], true);
                assert_eq!(r["reserve_pause_reason"], "stop");
                assert_eq!(r["available"], false);
            } else {
                assert_eq!(blockers(r), blockers(route_of(&baseline, route)));
            }
        }
        let (status, _) = post_note(
            &base,
            "/unpause",
            &format!(r#"{{"direction":"{direction}","note":"go"}}"#),
        )
        .await;
        assert_eq!(status, 200);
        let resumed = routes_view(&base).await;
        for route in ALL_ROUTES {
            assert_eq!(
                blockers(route_of(&resumed, route)),
                blockers(route_of(&baseline, route)),
                "{route} restored after resuming {direction}"
            );
        }
    }
}

/// `direction: robinhood` on `/pause` and `/unpause` reaches the third
/// reserve's own guarded implementation: pausing is unconditional,
/// unpausing runs the guard, both are audited under the same action
/// names as the other two reserves.
#[tokio::test]
async fn robinhood_local_pause_over_http_is_guarded_and_audited() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    configure_robinhood_reserve(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;

    let (status, receipt) = post_note(
        &base,
        "/pause",
        r#"{"direction":"robinhood","note":"rhn stop"}"#,
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["action"], "pause");
    assert_eq!(receipt["target"], "robinhood");
    let view = routes_view(&base).await;
    for route in ["GlcToRhn", "SolToRhn"] {
        assert_eq!(route_of(&view, route)["reserve_paused"], true, "{route}");
    }
    for route in ["GlcToSol", "SolToGlc", "RhnToGlc", "RhnToSol"] {
        assert_eq!(route_of(&view, route)["reserve_paused"], false, "{route}");
    }

    let (status, receipt) = post_note(
        &base,
        "/unpause",
        r#"{"direction":"robinhood","note":"rhn go"}"#,
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["action"], "unpause");
    let ledger = Ledger::open(&db_path).unwrap();
    assert!(!ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].target.as_deref(), Some("robinhood"));

    // An unknown direction is still a 400 with nothing written.
    let (status, _) = post_note(&base, "/pause", r#"{"direction":"mars","note":"x"}"#).await;
    assert_eq!(status, 400);
}

/// `GET /submitters` on a server with neither submitter wired reports
/// both as absent — never a zero balance for an address it does not
/// know.
#[tokio::test]
async fn submitters_view_reports_absence_when_not_configured() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let (base, _tx) = spawn_admin_server(&db_path).await;
    let resp = client()
        .get(format!("{base}/submitters"))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["solana"].is_null());
    assert!(body["robinhood"].is_null());
    assert!(body["as_of"].is_i64());
}

/// The contract flags, when a reader is wired, fold into the view: a
/// route the contract has disabled, or whose leg it has paused, is
/// unavailable with the contract gate named — while a failed read is
/// reported as `contract_unread` and closes nothing.
#[tokio::test]
async fn contract_flags_fold_into_the_route_view_and_unread_closes_nothing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct FakeReader {
        fail: AtomicBool,
    }
    impl robinhood_read::RobinhoodAdminReader for FakeReader {
        fn contract_flags(
            &self,
        ) -> Pin<Box<dyn Future<Output = Option<robinhood_read::RobinhoodContractFlags>> + Send + '_>>
        {
            Box::pin(async move {
                if self.fail.load(Ordering::SeqCst) {
                    return None;
                }
                Some(robinhood_read::RobinhoodContractFlags {
                    deposits_paused: true,
                    payouts_paused: false,
                    route_enabled: vec![
                        (crate::routes::Route::GlcToRhn, false),
                        (crate::routes::Route::RhnToGlc, true),
                        (crate::routes::Route::SolToRhn, true),
                        (crate::routes::Route::RhnToSol, true),
                    ],
                    read_at: now_unix(),
                })
            })
        }
        fn submitter(
            &self,
        ) -> Pin<Box<dyn Future<Output = robinhood_read::RobinhoodSubmitterRead> + Send + '_>>
        {
            Box::pin(async move {
                robinhood_read::RobinhoodSubmitterRead {
                    address: crate::evm::EvmAddress::from_bytes([7u8; 20]),
                    balance_wei: Some(crate::evm::EvmU256::from_u128(5)),
                    min_balance_wei: crate::evm::EvmU256::from_u128(10),
                }
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    configure_ledger(&db_path);
    let reader = Arc::new(FakeReader {
        fail: AtomicBool::new(false),
    });
    let api = AdminApi::new(db_path.clone(), FakeSolanaRpc)
        .with_robinhood_reader(reader.clone() as Arc<dyn robinhood_read::RobinhoodAdminReader>);

    let view = api.routes().await.unwrap();
    let find = |route: &str| view.routes.iter().find(|r| r.route == route).unwrap();
    assert_eq!(view.robinhood_contract.availability, "available");
    assert_eq!(view.robinhood_contract.deposits_paused, Some(true));
    // GlcToRhn: routeEnabled=false on chain.
    let r = find("GlcToRhn");
    assert_eq!(r.contract_route_enabled, Some(false));
    assert_eq!(r.contract_paused, Some(false), "payouts are not paused");
    assert!(r.blockers.contains(&"contract_route_disabled".to_string()));
    assert!(!r.available);
    // RhnToGlc: enabled on chain but deposits are paused (Robinhood is
    // its source).
    let r = find("RhnToGlc");
    assert_eq!(r.contract_route_enabled, Some(true));
    assert_eq!(r.contract_paused, Some(true));
    assert!(r.blockers.contains(&"contract_deposits_paused".to_string()));
    // SolToRhn: enabled, payouts not paused: no contract blocker.
    let r = find("SolToRhn");
    assert!(!r.blockers.iter().any(|b| b.starts_with("contract_")));
    // A Solana-only route never carries a contract field.
    let r = find("GlcToSol");
    assert_eq!(r.contract_route_enabled, None);
    assert_eq!(r.contract_paused, None);

    let submitters = api.submitters().await.unwrap();
    let rhn = submitters.robinhood.unwrap();
    assert_eq!(rhn.balance_wei.as_deref(), Some("5"));
    assert_eq!(rhn.min_balance_wei, "10");
    assert_eq!(rhn.funded, Some(false));
    assert!(submitters.solana.is_none());

    // The cache serves the read for CONTRACT_FLAGS_CACHE_SECS; a
    // failing reader afterwards is reported as unread and adds no
    // blocker.
    reader.fail.store(true, Ordering::SeqCst);
    let cached = api.routes().await.unwrap();
    assert_eq!(cached.robinhood_contract.availability, "available");
    {
        let mut cache = api.contract_flags_cache.lock().await;
        if let Some(flags) = cache.as_mut() {
            flags.read_at -= CONTRACT_FLAGS_CACHE_SECS + 1;
        }
    }
    let unread = api.routes().await.unwrap();
    assert_eq!(unread.robinhood_contract.availability, "unavailable");
    let r = unread
        .routes
        .iter()
        .find(|r| r.route == "GlcToRhn")
        .unwrap();
    assert_eq!(r.contract_route_enabled, None);
    assert!(r.warnings.contains(&"contract_unread".to_string()));
    assert!(!r.blockers.iter().any(|b| b.starts_with("contract_")));
}
