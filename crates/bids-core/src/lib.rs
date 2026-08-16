//! Shared, schema-agnostic BIDS primitives.
//!
//! `bids-core` holds the reusable building blocks for working with a BIDS dataset on disk,
//! independent of any particular tool built on top:
//!
//! - [`filetree`] — walk a BIDS dataset directory into a [`filetree::FileTree`], honouring
//!   `.bidsignore`, hidden-file, and always-ignore rules.
//! - [`entities`] — parse a BIDS filename into its entities, suffix, and extension.
//! - [`datatype`] — read a file's datatype off its path position (the immediate parent
//!   directory), against a datatype set the caller supplies.
//! - [`inheritance`] — resolve a data file's effective JSON sidecar via the BIDS inheritance
//!   principle, and find associated files.
//!
//! These were extracted from the `bids-validator-rs` crate so that both the validator and
//! other tools (e.g. `bidslake`) can share one implementation. The crate deliberately has a
//! light dependency footprint (`tokio` + `ignore` + `serde_json`) so consumers pull in nothing
//! validation-specific.

pub mod datatype;
pub mod entities;
pub mod filetree;
pub mod inheritance;

/// Proptest strategies for the filename shapes this crate parses, for use by this crate's own
/// tests and by the tests of crates above it. Behind the `proptest` feature, which nothing but
/// a `[dev-dependencies]` line ever turns on — see `Cargo.toml`.
#[cfg(any(test, feature = "proptest"))]
pub mod strategy;
