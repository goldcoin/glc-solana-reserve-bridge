//! What the operator actually pointed `--config` at.
//!
//! # The failure this exists to remove
//!
//! Every chain-policy command takes `--config PATH` and hands it
//! straight to [`Config::load`]. Given a *documentation snippet* — say
//! `docs/robinhood/launch-policy.toml.example`, which holds a
//! `[robinhood.policy]` section and nothing else — the parser answers
//! with the literal truth:
//!
//! ```text
//! TOML parse error at line 1, column 1
//! missing field `solana`
//! ```
//!
//! which is correct, useless, and easy to read as "the tool is broken"
//! rather than "that file is not a config file". An operator who repeats
//! the command gets the same sentence again, and the interactive manager
//! used to hand it to them once per menu action.
//!
//! So before anything is parsed *as a config*, the file is CLASSIFIED:
//! is it the full bridge config the daemon loads, a policy fragment, an
//! incomplete config, something that is not TOML at all, or simply not
//! there? Each answer names itself, and the fragment answer additionally
//! shows the policy the fragment states — read-only, because a fragment
//! is documentation and this module writes nothing under any
//! classification.
//!
//! # It classifies, it does not validate
//!
//! [`FileKind::FullConfig`] means [`Config::load`] returned `Ok` — the
//! one authority on that question, called directly, never re-implemented
//! here. The document parsing below exists only to EXPLAIN a failure
//! after the real parser has already refused, and to read a fragment's
//! three policy keys for display. It never decides that a file is
//! loadable.

use std::path::Path;

use toml_edit::{DocumentMut, TableLike};

use super::{ChainPolicy, POLICY_GOVERNED_CHAINS};
use crate::amount_conversion::CanonicalAtomic;
use crate::config::Config;
use crate::routes::Chain;

/// The top-level tables [`Config::load`] requires in every bridge config.
///
/// Kept in the order `RawConfig` declares them, which is the order the
/// parser complains about them in, so an operator comparing this list
/// against an error message reads the two the same way round.
pub const REQUIRED_SECTIONS: &[&str] = &["solana", "goldcoin", "reserve", "operators", "service"];

/// One `[<chain>.policy]` section found in a file that is not a config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FragmentPolicy {
    pub chain: Chain,
    /// The policy the fragment states, or why it does not state one.
    ///
    /// A fragment with a typo in it is still a fragment — the
    /// classification does not depend on the keys being readable, so a
    /// half-written snippet is named as a snippet rather than as a
    /// mystery.
    pub policy: Result<ChainPolicy, String>,
}

impl FragmentPolicy {
    /// `robinhood.policy` — the section's dotted name, for messages.
    pub fn section(&self) -> String {
        format!("{}.policy", self.chain.as_str())
    }
}

/// What a path holds, from the point of view of a command that wants a
/// bridge config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    /// [`Config::load`] accepted it. The only kind a chain-policy
    /// command may act on.
    FullConfig,
    /// Valid TOML that states at least one `[<chain>.policy]` section
    /// but is missing config sections the parser requires — a snippet,
    /// a template, or an excerpt someone saved out of a runbook.
    PolicyFragment {
        policies: Vec<FragmentPolicy>,
        missing_sections: Vec<&'static str>,
    },
    /// Valid TOML, missing required sections, and stating no policy
    /// either — some other file entirely, or a config that was cut off.
    IncompleteConfig { missing_sections: Vec<&'static str> },
    /// Every required section is present and the parser still refused
    /// it. The parser's own words are the whole answer here; nothing in
    /// this module could improve on them.
    InvalidConfig { detail: String },
    /// Not TOML at all.
    NotToml { detail: String },
    /// No such file.
    Missing,
    /// It exists and could not be read — permissions, a directory, a
    /// broken symlink.
    Unreadable { detail: String },
}

impl FileKind {
    /// Whether a chain-policy command may proceed with this file.
    pub fn is_usable(&self) -> bool {
        matches!(self, FileKind::FullConfig)
    }

    /// A stable, script-readable tag. The interactive manager branches
    /// on this rather than on prose, so the prose can be reworded
    /// without breaking it.
    pub fn tag(&self) -> &'static str {
        match self {
            FileKind::FullConfig => "full-config",
            FileKind::PolicyFragment { .. } => "policy-fragment",
            FileKind::IncompleteConfig { .. } => "incomplete-config",
            FileKind::InvalidConfig { .. } => "invalid-config",
            FileKind::NotToml { .. } => "not-toml",
            FileKind::Missing => "missing",
            FileKind::Unreadable { .. } => "unreadable",
        }
    }

    /// One line naming what is wrong, or that nothing is.
    pub fn headline(&self) -> String {
        match self {
            FileKind::FullConfig => {
                "OK — this is a bridge config file and the config parser loads it.".to_string()
            }
            FileKind::PolicyFragment { .. } => {
                "NOT A CONFIG FILE — this is a POLICY FRAGMENT.".to_string()
            }
            FileKind::IncompleteConfig { .. } => {
                "NOT A CONFIG FILE — required sections are missing.".to_string()
            }
            FileKind::InvalidConfig { .. } => {
                "NOT LOADABLE — this looks like a config file, but the parser refuses it."
                    .to_string()
            }
            FileKind::NotToml { .. } => "NOT TOML — this file cannot be parsed at all.".to_string(),
            FileKind::Missing => "NO SUCH FILE.".to_string(),
            FileKind::Unreadable { .. } => "UNREADABLE.".to_string(),
        }
    }
}

/// Classifies `path`. Reads it and nothing else; writes nothing, ever.
pub fn inspect(path: &Path) -> FileKind {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return FileKind::Missing,
        Err(source) => {
            return FileKind::Unreadable {
                detail: source.to_string(),
            }
        }
    };

    let doc: DocumentMut = match text.parse() {
        Ok(doc) => doc,
        Err(source) => {
            return FileKind::NotToml {
                detail: source.to_string(),
            }
        }
    };

    let missing_sections: Vec<&'static str> = REQUIRED_SECTIONS
        .iter()
        .copied()
        .filter(|name| doc.get(name).is_none())
        .collect();

    // The real parser decides loadability, always — a file with every
    // section present can still be refused for a hundred reasons this
    // module knows nothing about, and must be reported in the parser's
    // own words rather than guessed at.
    if missing_sections.is_empty() {
        return match Config::load(path) {
            Ok(_) => FileKind::FullConfig,
            Err(e) => FileKind::InvalidConfig {
                detail: e.to_string(),
            },
        };
    }

    let policies = fragment_policies(&doc);
    if policies.is_empty() {
        FileKind::IncompleteConfig { missing_sections }
    } else {
        FileKind::PolicyFragment {
            policies,
            missing_sections,
        }
    }
}

/// Every `[<chain>.policy]` section a policy-governed chain could own,
/// read out of an already-parsed document.
///
/// Only [`POLICY_GOVERNED_CHAINS`] are looked for: a `[solana.policy]`
/// table in some file is not a policy this bridge would ever honour, and
/// naming it as one here would contradict the parser, which ignores it.
fn fragment_policies(doc: &DocumentMut) -> Vec<FragmentPolicy> {
    POLICY_GOVERNED_CHAINS
        .iter()
        .copied()
        .filter_map(|chain| {
            let table = doc
                .get(chain.as_str())
                .and_then(|item| item.as_table_like())
                .and_then(|chain_table| chain_table.get("policy"))
                .and_then(|item| item.as_table_like())?;
            Some(FragmentPolicy {
                chain,
                policy: read_policy(table, chain),
            })
        })
        .collect()
}

fn read_policy(table: &dyn TableLike, chain: Chain) -> Result<ChainPolicy, String> {
    let fee_bps = integer(table, "fee_bps")?;
    // The same two forms, and the same refusal to mix them, as the
    // config parser (`config::resolve_chain_policies`).
    let legacy = table.get("per_transfer_limit").is_some();
    let inbound = table.get("inbound_per_transfer_limit").is_some();
    let outbound = table.get("outbound_per_transfer_limit").is_some();
    let (inbound, outbound) = match (legacy, inbound, outbound) {
        (true, false, false) => {
            let both = integer(table, "per_transfer_limit")?;
            (both, both)
        }
        (false, true, true) => (
            integer(table, "inbound_per_transfer_limit")?,
            integer(table, "outbound_per_transfer_limit")?,
        ),
        (true, _, _) => {
            return Err("`per_transfer_limit` (legacy) cannot be combined with \
                        `inbound_per_transfer_limit` / `outbound_per_transfer_limit`"
                .to_string())
        }
        _ => {
            return Err("state either `per_transfer_limit` (legacy) or BOTH \
                        `inbound_per_transfer_limit` and `outbound_per_transfer_limit`"
                .to_string())
        }
    };
    let rolling = integer(table, "rolling_daily_limit")?;
    ChainPolicy::new(
        chain,
        fee_bps,
        CanonicalAtomic(inbound),
        CanonicalAtomic(outbound),
        CanonicalAtomic(rolling),
    )
    .map_err(|e| e.to_string())
}

fn integer(table: &dyn TableLike, key: &str) -> Result<u64, String> {
    let item = table
        .get(key)
        .ok_or_else(|| format!("`{key}` is missing from the section"))?;
    let value = item
        .as_integer()
        .ok_or_else(|| format!("`{key}` is not an integer"))?;
    u64::try_from(value).map_err(|_| format!("`{key}` is negative: {value}"))
}

#[cfg(test)]
mod tests;
