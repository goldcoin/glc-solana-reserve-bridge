//! Writing a per-chain policy back into an operator's config file:
//! planned, validated through the real parser, backed up, and installed
//! atomically.
//!
//! # Three rules, and the reason for each
//!
//! **The file is edited as a DOCUMENT, never as text and never as a
//! re-serialised struct.** `toml_edit` parses the file, three keys are
//! set, and the document renders back out with every other key, every
//! blank line and — the point — every COMMENT intact. A config file's
//! comments are the reasoning behind its numbers; a tool that silently
//! deleted them would cost an operator more than it saved. Rewriting the
//! text with a pattern substitution is the other failure mode: it cannot
//! tell `fee_bps` under `[robinhood.policy]` from the same word in a
//! comment, in a different table, or in a string.
//!
//! **Nothing is written until the candidate file has been loaded by the
//! real parser.** [`plan`] renders the edited document into a temporary
//! file beside the target and runs [`Config::load`] on it — the exact
//! function the daemon runs at startup, with the same validation and the
//! same environment overrides. A candidate that does not load, or that
//! loads to a policy other than the one requested, is refused before the
//! original file has been touched at all. This is why there is no
//! separate re-implementation of policy validation here: there is one
//! validator, and it is the one production uses.
//!
//! **The bytes that were validated are the bytes that get installed.**
//! [`commit`] does not re-render anything. It backs the original up, then
//! `rename`s the already-validated candidate over it — one atomic
//! syscall on the same filesystem. There is no window in which the config
//! file is half-written: a reader either sees the whole old file or the
//! whole new one, and a crash at any point leaves one of those two, never
//! a third thing.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use toml_edit::{value, DocumentMut, Item, Table};

use super::{ChainPolicy, ChainPolicyError};
use crate::config::Config;
use crate::routes::Chain;

/// Why a policy edit could not be planned or committed.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid TOML: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error(
        "the config file has a `{path_in_file}` key that is not a table, so there is nowhere to \
         put a policy without destroying it. Fix the file by hand"
    )]
    NotATable { path_in_file: String },
    #[error(
        "{chain} has no configurable policy — {detail}. Nothing was written; this tool refuses to \
         invent a config section that the parser would ignore"
    )]
    ChainNotConfigurable {
        chain: &'static str,
        detail: &'static str,
    },
    #[error(transparent)]
    Policy(#[from] ChainPolicyError),
    #[error(
        "the edited config does not load: {detail}. The original file was NOT modified and the \
         candidate has been removed"
    )]
    CandidateRejected { detail: String },
    #[error(
        "the edited config loads, but its {chain} policy is not the one requested — refusing to \
         install a file whose effect this tool cannot predict. The original was NOT modified"
    )]
    CandidateDisagrees { chain: &'static str },
    #[error("the config path {path} has no parent directory")]
    NoParentDirectory { path: PathBuf },
}

/// A validated, ready-to-install edit.
///
/// Holding one means the new config file already exists on disk, beside
/// the target, and has already been loaded successfully by
/// [`Config::load`]. It has NOT been installed: dropping this value
/// without calling [`commit`] leaves the original file untouched — call
/// [`ApplyPlan::discard`] to also remove the candidate.
#[derive(Debug)]
pub struct ApplyPlan {
    path: PathBuf,
    candidate: PathBuf,
    chain: Chain,
    before: Option<ChainPolicy>,
    after: ChainPolicy,
}

impl ApplyPlan {
    /// The config file this plan would replace.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The temporary file holding the validated new contents.
    pub fn candidate_path(&self) -> &Path {
        &self.candidate
    }

    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// The policy the file holds now — `None` when the chain has no
    /// policy section yet, which is not the same as a zero policy.
    pub fn before(&self) -> Option<&ChainPolicy> {
        self.before.as_ref()
    }

    /// The policy the file would hold.
    pub fn after(&self) -> &ChainPolicy {
        &self.after
    }

    /// Whether this edit would change anything at all.
    pub fn is_noop(&self) -> bool {
        self.before.as_ref() == Some(&self.after)
    }

    /// The new file's full text, for a preview.
    pub fn rendered(&self) -> Result<String, EditError> {
        fs::read_to_string(&self.candidate).map_err(|source| EditError::Read {
            path: self.candidate.clone(),
            source,
        })
    }

    /// Removes the candidate without installing it — what a dry run does
    /// when it is finished looking.
    pub fn discard(self) {
        let _ = fs::remove_file(&self.candidate);
    }
}

/// What [`commit`] did, for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReport {
    pub path: PathBuf,
    pub backup: PathBuf,
}

/// Plans an edit: reads the file, sets the three policy keys, renders the
/// candidate beside the original and proves it loads.
///
/// Writes ONLY the candidate file. The target is untouched whatever
/// happens here.
pub fn plan(path: &Path, chain: Chain, after: ChainPolicy) -> Result<ApplyPlan, EditError> {
    if !super::POLICY_GOVERNED_CHAINS.contains(&chain) {
        return Err(EditError::ChainNotConfigurable {
            chain: chain.as_str(),
            detail: super::governance(chain).why_not_configurable,
        });
    }
    if after.chain() != chain {
        return Err(EditError::Policy(
            ChainPolicyError::ChainNotPolicyGoverned {
                chain: after.chain().as_str(),
            },
        ));
    }

    // The BEFORE value comes from the real parser too, not from the
    // document: what the file means is what `Config::load` says it means.
    let before = Config::load(path)
        .map_err(|e| EditError::CandidateRejected {
            detail: format!("the EXISTING config file does not load: {e}"),
        })?
        .chain_policies
        .get(chain)
        .copied();

    let text = fs::read_to_string(path).map_err(|source| EditError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut doc: DocumentMut = text.parse().map_err(|source| EditError::Parse {
        path: path.to_path_buf(),
        source,
    })?;

    let section = chain.as_str();
    let chain_table = ensure_table(doc.as_table_mut(), section)?;
    let policy_table = ensure_table(chain_table, "policy")?;
    policy_table["fee_bps"] = value(i64::try_from(after.fee_bps()).map_err(|_| {
        EditError::Policy(ChainPolicyError::FeeBpsOutOfRange {
            chain: section,
            fee_bps: after.fee_bps(),
        })
    })?);
    // The directional pair is always written; the legacy one-figure key
    // is removed so the file never states both forms (the parser refuses
    // that).
    policy_table.remove("per_transfer_limit");
    policy_table["inbound_per_transfer_limit"] = value(to_toml_integer(
        after.inbound_per_transfer_limit().0,
        "inbound_per_transfer_limit",
    )?);
    policy_table["outbound_per_transfer_limit"] = value(to_toml_integer(
        after.outbound_per_transfer_limit().0,
        "outbound_per_transfer_limit",
    )?);
    policy_table["rolling_daily_limit"] = value(to_toml_integer(
        after.rolling_daily_limit().0,
        "rolling_daily_limit",
    )?);

    let parent = path.parent().ok_or_else(|| EditError::NoParentDirectory {
        path: path.to_path_buf(),
    })?;
    let candidate = parent.join(format!(
        "{}.chain-policy-candidate.{}",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "config.toml".to_string()),
        std::process::id()
    ));
    write_file_synced(&candidate, doc.to_string().as_bytes())?;

    // The proof. Same function the daemon runs at startup.
    let loaded = match Config::load(&candidate) {
        Ok(loaded) => loaded,
        Err(e) => {
            let _ = fs::remove_file(&candidate);
            return Err(EditError::CandidateRejected {
                detail: e.to_string(),
            });
        }
    };
    if loaded.chain_policies.get(chain) != Some(&after) {
        let _ = fs::remove_file(&candidate);
        return Err(EditError::CandidateDisagrees {
            chain: chain.as_str(),
        });
    }

    Ok(ApplyPlan {
        path: path.to_path_buf(),
        candidate,
        chain,
        before,
        after,
    })
}

/// Installs a planned edit: timestamped backup first, then one atomic
/// rename of the already-validated candidate.
///
/// `now_unix` is a parameter rather than a clock read so the backup name
/// is a deterministic function of its inputs and can be asserted in a
/// test.
pub fn commit(plan: ApplyPlan, now_unix: i64) -> Result<CommitReport, EditError> {
    install(&plan.path, &plan.candidate, now_unix)
}

/// Backs `path` up and renames `candidate` over it.
///
/// The install half of [`commit`], extracted so `crate::fees::edit` —
/// which writes a different table into the same kind of file — installs
/// through the SAME implementation rather than a second one that has to
/// be kept in step with this one's ordering, durability and backup-naming
/// guarantees.
///
/// `candidate` MUST already have been validated by the real parser; this
/// function checks nothing about it.
pub(crate) fn install(
    path: &Path,
    candidate: &Path,
    now_unix: i64,
) -> Result<CommitReport, EditError> {
    let backup = path.with_file_name(format!(
        "{}.bak.{}",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "config.toml".to_string()),
        format_utc_compact(now_unix)
    ));

    // Backup BEFORE the rename, and by copying rather than by renaming:
    // a rename would leave no file at the config path if the process died
    // between the two steps.
    let original = fs::read(path).map_err(|source| EditError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    write_file_synced(&backup, &original)?;

    fs::rename(candidate, path).map_err(|source| EditError::Write {
        path: path.to_path_buf(),
        source,
    })?;

    // Durability of the rename itself, not of the file contents: the
    // directory entry is what changed.
    if let Some(parent) = path.parent() {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(CommitReport {
        path: path.to_path_buf(),
        backup,
    })
}

/// Returns the table at `key`, creating it if absent, or refusing if the
/// key exists as something else. Never replaces a non-table value: that
/// would destroy configuration this tool does not understand.
pub(crate) fn ensure_table<'a>(
    parent: &'a mut Table,
    key: &str,
) -> Result<&'a mut Table, EditError> {
    if parent.get(key).is_none() {
        let mut created = Table::new();
        // Rendered as `[chain.policy]` rather than inline, matching how
        // every other section in these files is written.
        created.set_implicit(false);
        parent.insert(key, Item::Table(created));
    }
    match parent.get_mut(key) {
        Some(Item::Table(table)) => Ok(table),
        _ => Err(EditError::NotATable {
            path_in_file: key.to_string(),
        }),
    }
}

fn to_toml_integer(value: u64, field: &'static str) -> Result<i64, EditError> {
    i64::try_from(value).map_err(|_| EditError::CandidateRejected {
        detail: format!(
            "{field} {value} is above TOML's signed 64-bit integer range, so it cannot be \
             written to a config file at all"
        ),
    })
}

pub(crate) fn write_file_synced(path: &Path, bytes: &[u8]) -> Result<(), EditError> {
    let mut file = fs::File::create(path).map_err(|source| EditError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(bytes).map_err(|source| EditError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| EditError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// `1757404800` -> `20250909T092000Z`. A compact, sortable, unambiguous
/// backup suffix.
///
/// Implemented here rather than pulled from a date library because it is
/// the only date formatting this crate does, and because a backup name
/// must never depend on a local timezone: two operators in different
/// zones must produce comparable names.
pub fn format_utc_compact(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60,
    );
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a
/// proleptic Gregorian date. Chosen because it is exact for every input
/// and has no leap-year special cases to get wrong.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests;
