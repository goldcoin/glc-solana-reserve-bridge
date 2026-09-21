use super::*;

use crate::config::tests::valid_config;

/// The exact fragment an operator reached for, verbatim from the repo,
/// so this test fails the day that file grows a `[solana]` section and
/// stops being a fragment.
fn shipped_launch_policy_example() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the service crate has a parent directory")
        .join("docs/robinhood/launch-policy.toml.example")
}

#[test]
fn a_real_config_is_a_full_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let kind = inspect(&path);
    assert_eq!(kind, FileKind::FullConfig, "{kind:?}");
    assert!(kind.is_usable());
    assert_eq!(kind.tag(), "full-config");
}

/// The bug, as a test: the file that produced "missing field `solana`"
/// is classified as a fragment and its policy is readable.
#[test]
fn the_shipped_launch_policy_example_is_a_fragment_with_a_readable_policy() {
    let kind = inspect(&shipped_launch_policy_example());
    let FileKind::PolicyFragment {
        policies,
        missing_sections,
    } = &kind
    else {
        panic!("expected a policy fragment, got {kind:?}");
    };
    assert!(!kind.is_usable());
    assert_eq!(kind.tag(), "policy-fragment");
    // Every required section is absent — this is not a config that lost
    // one table, it is a different kind of file.
    assert_eq!(missing_sections, REQUIRED_SECTIONS);

    assert_eq!(policies.len(), 1, "{policies:?}");
    assert_eq!(policies[0].chain, Chain::Robinhood);
    assert_eq!(policies[0].section(), "robinhood.policy");
    let policy = policies[0].policy.as_ref().expect("the approved policy");
    assert_eq!(policy.fee_bps(), 600);
    assert_eq!(
        policy.inbound_per_transfer_limit(),
        CanonicalAtomic(2_000_000_000_000)
    );
    assert_eq!(
        policy.outbound_per_transfer_limit(),
        CanonicalAtomic(2_000_000_000_000)
    );
    assert_eq!(
        policy.rolling_daily_limit(),
        CanonicalAtomic(1_000_000_000_000_000)
    );
}

/// A fragment whose values are wrong is still a fragment. Classification
/// must not depend on the snippet being valid, or a typo would turn a
/// clear "that is not a config file" back into a mystery.
#[test]
fn a_fragment_with_an_unusable_policy_is_still_a_fragment() {
    // 601 bps used to be the unusable value here, purely for being a rate
    // no release had shipped. It is an ordinary rate now, so the unusable
    // value is one that is genuinely out of range: 100%, at which every
    // transfer would deliver nothing.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snippet.toml");
    std::fs::write(
        &path,
        "[robinhood.policy]\nfee_bps = 10000\nper_transfer_limit = 1\nrolling_daily_limit = 2\n",
    )
    .unwrap();

    let kind = inspect(&path);
    let FileKind::PolicyFragment { policies, .. } = &kind else {
        panic!("expected a policy fragment, got {kind:?}");
    };
    let detail = policies[0]
        .policy
        .as_ref()
        .expect_err("10000 bps leaves the user nothing and cannot be a fee");
    assert!(detail.contains("9999"), "{detail}");
}

#[test]
fn a_fragment_missing_a_key_names_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snippet.toml");
    std::fs::write(&path, "[robinhood.policy]\nfee_bps = 600\n").unwrap();

    let FileKind::PolicyFragment { policies, .. } = inspect(&path) else {
        panic!("expected a policy fragment");
    };
    let detail = policies[0].policy.as_ref().expect_err("incomplete");
    assert!(detail.contains("per_transfer_limit"), "{detail}");
}

/// A negative amount is refused as a read, not silently wrapped into an
/// enormous unsigned one.
#[test]
fn a_negative_amount_is_refused_rather_than_wrapped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snippet.toml");
    std::fs::write(
        &path,
        "[robinhood.policy]\nfee_bps = 600\nper_transfer_limit = -1\nrolling_daily_limit = 2\n",
    )
    .unwrap();

    let FileKind::PolicyFragment { policies, .. } = inspect(&path) else {
        panic!("expected a policy fragment");
    };
    let detail = policies[0].policy.as_ref().expect_err("negative");
    assert!(detail.contains("negative"), "{detail}");
}

/// The mainnet template: comments only, so no policy section is live in
/// it. It is still not a config file, and says so as an incomplete one
/// rather than claiming a policy it does not state.
#[test]
fn a_commented_out_template_is_incomplete_not_a_fragment() {
    let path = shipped_launch_policy_example()
        .parent()
        .unwrap()
        .join("mainnet-disabled.toml.example");
    let kind = inspect(&path);
    let FileKind::IncompleteConfig { missing_sections } = &kind else {
        panic!("expected an incomplete config, got {kind:?}");
    };
    assert_eq!(missing_sections, REQUIRED_SECTIONS);
    assert_eq!(kind.tag(), "incomplete-config");
}

#[test]
fn a_config_missing_one_section_names_that_section() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    // Drop `[operators]` and everything under it, up to the next table.
    let mut kept = String::new();
    let mut skipping = false;
    for line in text.lines() {
        if line.starts_with('[') {
            skipping = line.starts_with("[operators]");
        }
        if !skipping {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    std::fs::write(&path, kept).unwrap();

    let kind = inspect(&path);
    let FileKind::IncompleteConfig { missing_sections } = &kind else {
        panic!("expected an incomplete config, got {kind:?}");
    };
    assert_eq!(missing_sections, &["operators"]);
}

/// Every section present and the parser still says no: the parser's own
/// words are reported, because nothing here could improve on them.
#[test]
fn a_complete_but_rejected_config_reports_the_parsers_words() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("attestation_threshold = 2", "attestation_threshold = 99");
    std::fs::write(&path, text).unwrap();

    let kind = inspect(&path);
    let FileKind::InvalidConfig { detail } = &kind else {
        panic!("expected an invalid config, got {kind:?}");
    };
    assert!(detail.contains("attestation"), "{detail}");
    assert_eq!(kind.tag(), "invalid-config");
}

#[test]
fn a_file_that_is_not_toml_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, "this is not = = toml\n[[[\n").unwrap();
    assert_eq!(inspect(&path).tag(), "not-toml");
}

#[test]
fn a_missing_file_is_missing_not_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(inspect(&dir.path().join("nope.toml")), FileKind::Missing);
}

#[test]
fn a_directory_is_unreadable_rather_than_missing() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(inspect(dir.path()).tag(), "unreadable");
}

/// Inspecting never writes, whatever the answer — an operator pointing
/// this at the live config must not risk a stray file beside it.
#[test]
fn inspecting_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let before = entries(dir.path());
    for _ in 0..3 {
        inspect(&path);
        inspect(&dir.path().join("nope.toml"));
    }
    assert_eq!(entries(dir.path()), before);
}

fn entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}
