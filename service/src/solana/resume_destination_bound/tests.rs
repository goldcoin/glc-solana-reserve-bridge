//! Pure pieces of the recovery module; the end-to-end flow (park, dry
//! run, execute, attest, settle) and every refusal are in
//! `orchestrator::tests::destination_bound_resume`.

use super::*;

#[test]
fn the_audit_action_and_the_ledger_reason_marker_are_stable_names() {
    assert_eq!(AUDIT_ACTION, "resume_destination_bound");
    assert_eq!(
        Ledger::RESUME_DESTINATION_BOUND_REASON,
        "resume_destination_bound"
    );
}

#[test]
fn a_dry_run_clears_only_on_a_would_resume_trial() {
    let clears = |ledger: ResumeDryRunOutcome| matches!(ledger, ResumeDryRunOutcome::WouldResume);
    assert!(clears(ResumeDryRunOutcome::WouldResume));
    assert!(!clears(ResumeDryRunOutcome::AlreadyResumed {
        state: crate::ledger::RequestState::SourceFinalized
    }));
    assert!(!clears(ResumeDryRunOutcome::WouldRefuse {
        reason: "x".to_string()
    }));
}
