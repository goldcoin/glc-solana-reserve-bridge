//! Solana-side chain plumbing: RPC client (pinned to `finalized`
//! commitment), account decoding, instruction encoding, attestation-proof
//! building, and bounded transaction confirmation (docs/03-architecture.md).

pub mod accounts;
pub mod confirm;
pub mod ed25519;
pub mod indexer;
pub mod instructions;
pub mod manual_refund;
pub mod manual_review_settle;
pub mod program_compat;
pub mod reconcile_request;
pub mod refund;
pub mod resume_destination_bound;
pub mod rpc;
