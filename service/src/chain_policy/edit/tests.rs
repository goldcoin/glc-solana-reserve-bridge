use super::*;

use crate::amount_conversion::CanonicalAtomic;
use crate::config::tests::valid_config;

/// 1 GLC in canonical 8-decimal units.
const ONE_GLC: u64 = 100_000_000;

fn approved() -> ChainPolicy {
    ChainPolicy::new_symmetric(
        Chain::Robinhood,
        600,
        CanonicalAtomic(20_000 * ONE_GLC),
        CanonicalAtomic(10_000_000 * ONE_GLC),
    )
    .expect("the approved policy")
}

/// A config file with a `[robinhood.policy]` section already in it, plus
/// a comment that must survive every edit.
fn config_with_policy(dir: &std::path::Path) -> std::path::PathBuf {
    let path = valid_config(dir);
    let mut toml = std::fs::read_to_string(&path).unwrap();
    toml.push_str(
        "\n# The reasoning behind these numbers, which must survive an edit.\n\
         [robinhood.policy]\n\
         fee_bps = 300\n\
         per_transfer_limit = 1000000000000\n\
         rolling_daily_limit = 2000000000000\n",
    );
    std::fs::write(&path, toml).unwrap();
    path
}

fn loaded_policy(path: &std::path::Path) -> Option<ChainPolicy> {
    Config::load(path)
        .expect("the config loads")
        .chain_policies
        .get(Chain::Robinhood)
        .copied()
}

fn backups(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".bak."))
        .collect();
    names.sort();
    names
}

fn candidates(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains("chain-policy-candidate"))
        .collect();
    names.sort();
    names
}

// ------------------------------------------------------------ planning --

/// The whole point of the plan/commit split: planning writes a candidate
/// and proves it loads, and the original file is byte-for-byte unchanged
/// until `commit` is called.
#[test]
fn planning_never_modifies_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with_policy(dir.path());
    let before_bytes = std::fs::read(&path).unwrap();

    let plan = plan(&path, Chain::Robinhood, approved()).expect("the edit plans");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before_bytes,
        "planning must not touch the target file"
    );
    assert!(plan.candidate_path().exists(), "the candidate exists");
    assert_eq!(plan.before().map(|p| p.fee_bps()), Some(300));
    assert_eq!(plan.after().fee_bps(), 600);

    plan.discard();
    assert!(candidates(dir.path()).is_empty(), "discard removes it");
    assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
}

/// A dry run leaves the filesystem exactly as it found it: no candidate,
/// no backup, no change.
#[test]
fn a_dry_run_leaves_no_trace() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with_policy(dir.path());
    let before_bytes = std::fs::read(&path).unwrap();
    let before_entries = std::fs::read_dir(dir.path()).unwrap().count();

    let plan = plan(&path, Chain::Robinhood, approved()).unwrap();
    let _ = plan.rendered().expect("a preview is readable");
    plan.discard();

    assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        before_entries
    );
    assert!(backups(dir.path()).is_empty());
    assert!(candidates(dir.path()).is_empty());
    assert_eq!(loaded_policy(&path).unwrap().fee_bps(), 300);
}

// ------------------------------------------------------------ applying --

#[test]
fn applying_backs_up_atomically_and_installs_exactly_the_validated_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with_policy(dir.path());
    let original = std::fs::read_to_string(&path).unwrap();

    let plan = plan(&path, Chain::Robinhood, approved()).unwrap();
    let candidate_bytes = std::fs::read(plan.candidate_path()).unwrap();
    let report = commit(plan, 1_757_404_800).expect("the edit commits");

    // The bytes that were validated are the bytes now installed — commit
    // re-renders nothing.
    assert_eq!(std::fs::read(&path).unwrap(), candidate_bytes);
    // The candidate is gone: it was renamed, not copied.
    assert!(candidates(dir.path()).is_empty());

    // The backup holds the original, byte for byte, under a UTC-stamped
    // name.
    assert_eq!(std::fs::read_to_string(&report.backup).unwrap(), original);
    assert_eq!(
        report.backup.file_name().unwrap().to_string_lossy(),
        "config.toml.bak.20250909T080000Z"
    );

    // And the new file means what it was supposed to mean.
    let after = loaded_policy(&path).expect("a policy");
    assert_eq!(after, approved());
}

/// Comments and unrelated keys survive. A tool that quietly deleted an
/// operator's reasoning would cost more than it saved, and a whole-file
/// re-serialisation is exactly how that happens.
#[test]
fn applying_preserves_comments_and_every_unrelated_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with_policy(dir.path());
    let before = Config::load(&path).unwrap();

    let plan = plan(&path, Chain::Robinhood, approved()).unwrap();
    commit(plan, 1_757_404_800).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("# The reasoning behind these numbers, which must survive an edit."),
        "the comment was destroyed:\n{text}"
    );

    // Everything the parser resolves outside the policy is identical.
    let after = Config::load(&path).unwrap();
    assert_eq!(after.goldcoin.network, before.goldcoin.network);
    assert_eq!(
        after.goldcoin.fee_rate_per_kb,
        before.goldcoin.fee_rate_per_kb
    );
    assert_eq!(
        after.solana.reserve_token_mint,
        before.solana.reserve_token_mint
    );
    assert_eq!(
        format!("{:?}", after.reserve.solana),
        format!("{:?}", before.reserve.solana)
    );
    assert_eq!(after.routes, before.routes);
    assert_eq!(after.service.db_path, before.service.db_path);
}

/// A file with no policy section gets one created, rather than being
/// refused — and the created section is a real `[robinhood.policy]`
/// table, not an inline value the parser would read differently.
#[test]
fn a_missing_policy_section_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    assert!(loaded_policy(&path).is_none());

    let plan = plan(&path, Chain::Robinhood, approved()).unwrap();
    assert!(plan.before().is_none(), "before is None, not a zero policy");
    commit(plan, 1_757_404_800).unwrap();

    assert_eq!(loaded_policy(&path), Some(approved()));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("[robinhood.policy]"), "{text}");
}

/// Applying the policy the file already states is recognised as a no-op
/// rather than silently churning the file.
#[test]
fn an_unchanged_policy_is_reported_as_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with_policy(dir.path());
    let first = plan(&path, Chain::Robinhood, approved()).unwrap();
    assert!(!first.is_noop());
    commit(first, 1_757_404_800).unwrap();

    let again = plan(&path, Chain::Robinhood, approved()).unwrap();
    assert!(again.is_noop());
    again.discard();
}

// ------------------------------------------------------- Solana safety --

/// Solana isolation, enforced at the file-writing layer as well as at the
/// policy layer: there is no argument that makes this tool write a Solana
/// policy section.
#[test]
fn solana_can_never_be_written_to_a_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with_policy(dir.path());
    let before_bytes = std::fs::read(&path).unwrap();

    // `ChainPolicy::new` already refuses Solana, so a Solana policy value
    // cannot even be constructed — assert that, then assert the edit
    // layer refuses the chain independently of it.
    assert!(ChainPolicy::new_symmetric(
        Chain::Solana,
        600,
        CanonicalAtomic(ONE_GLC),
        CanonicalAtomic(ONE_GLC)
    )
    .is_err());

    let err = plan(&path, Chain::Solana, approved()).expect_err("Solana is refused");
    assert!(
        matches!(err, EditError::ChainNotConfigurable { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("set-limit"), "{err}");

    assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
    assert!(candidates(dir.path()).is_empty());
    assert!(backups(dir.path()).is_empty());
}

/// A Robinhood edit must not disturb the Solana reserve bounds or any
/// other Solana-facing value in the same file.
#[test]
fn a_robinhood_edit_leaves_every_solana_value_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with_policy(dir.path());
    let before = Config::load(&path).unwrap();

    commit(
        plan(&path, Chain::Robinhood, approved()).unwrap(),
        1_757_404_800,
    )
    .unwrap();
    let after = Config::load(&path).unwrap();

    assert_eq!(after.solana.rpc_url, before.solana.rpc_url);
    assert_eq!(
        after.solana.reserve_token_mint,
        before.solana.reserve_token_mint
    );
    assert_eq!(
        format!("{:?}", after.reserve.solana),
        format!("{:?}", before.reserve.solana)
    );
    assert!(after.chain_policies.get(Chain::Solana).is_none());
    assert_eq!(
        after.chain_policies.fee_bps_for(Chain::Solana),
        crate::amount_conversion::BRIDGE_FEE_BPS
    );
}

// ----------------------------------------------------------- refusals --

/// The candidate is validated by the REAL parser, and a file that would
/// not load is refused with the original left alone.
#[test]
fn a_config_that_does_not_load_is_refused_before_anything_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with_policy(dir.path());
    // Corrupt an unrelated section so the candidate cannot load.
    let text = std::fs::read_to_string(&path).unwrap();
    let broken = text.replace("network = \"regtest\"", "network = \"not-a-network\"");
    assert_ne!(text, broken);
    std::fs::write(&path, &broken).unwrap();

    let err = plan(&path, Chain::Robinhood, approved()).expect_err("refused");
    assert!(matches!(err, EditError::CandidateRejected { .. }), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    assert!(
        candidates(dir.path()).is_empty(),
        "the candidate is cleaned up"
    );
}

#[test]
fn a_file_that_is_not_toml_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "this is not toml {{{").unwrap();
    assert!(plan(&path, Chain::Robinhood, approved()).is_err());
}

/// A `policy` key that is not a table is refused with the file left
/// alone.
///
/// WHICH refusal it is depends on which check reaches it first, and the
/// honest answer is that the config parser gets there: `Config::load` on
/// the ORIGINAL file is the first thing `plan` does, and a `policy` key
/// holding a string fails to deserialise. `EditError::NotATable` is the
/// guard for the case the parser would somehow accept — a defensive arm
/// that must never silently overwrite an operator's value. Either way the
/// requirement is the same and is what this asserts: refused, nothing
/// written, nothing left behind.
#[test]
fn a_policy_key_that_is_not_a_table_is_refused_rather_than_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n[robinhood]\npolicy = \"something an operator meant\"\n");
    std::fs::write(&path, &text).unwrap();

    let err = plan(&path, Chain::Robinhood, approved()).expect_err("refused");
    assert!(
        matches!(
            err,
            EditError::NotATable { .. } | EditError::CandidateRejected { .. }
        ),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    assert!(candidates(dir.path()).is_empty());
    assert!(backups(dir.path()).is_empty());
}

// ------------------------------------------------------- backup naming --

#[test]
fn backup_timestamps_are_utc_and_sortable() {
    assert_eq!(format_utc_compact(0), "19700101T000000Z");
    assert_eq!(format_utc_compact(1_757_404_800), "20250909T080000Z");
    // A leap day, because leap-year arithmetic is the thing hand-rolled
    // date code gets wrong.
    assert_eq!(format_utc_compact(1_709_164_800), "20240229T000000Z");
    assert_eq!(format_utc_compact(951_782_400), "20000229T000000Z");

    // Sortable: later instants must produce lexicographically later names.
    let mut stamps: Vec<String> = [0i64, 1_000, 1_709_164_800, 1_757_404_800, 2_000_000_000]
        .iter()
        .map(|t| format_utc_compact(*t))
        .collect();
    let sorted = {
        let mut s = stamps.clone();
        s.sort();
        s
    };
    assert_eq!(stamps, sorted);
    stamps.dedup();
    assert_eq!(stamps.len(), 5, "distinct instants get distinct names");
}

/// Two applies in the same second would collide; two in different seconds
/// must not, and the first backup must survive the second apply.
#[test]
fn successive_applies_keep_every_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with_policy(dir.path());

    commit(
        plan(&path, Chain::Robinhood, approved()).unwrap(),
        1_757_404_800,
    )
    .unwrap();
    let stricter = ChainPolicy::new_symmetric(
        Chain::Robinhood,
        600,
        CanonicalAtomic(20_000 * ONE_GLC),
        CanonicalAtomic(8_000_000 * ONE_GLC),
    )
    .unwrap();
    commit(
        plan(&path, Chain::Robinhood, stricter).unwrap(),
        1_757_404_861,
    )
    .unwrap();

    let names = backups(dir.path());
    assert_eq!(names.len(), 2, "{names:?}");
    assert_eq!(loaded_policy(&path), Some(stricter));
}
