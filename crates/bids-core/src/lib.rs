//! Shared, schema-agnostic BIDS primitives.
//!
//! `bids-core` holds the reusable building blocks for working with a BIDS dataset on disk,
//! independent of any particular tool built on top:
//!
//! - [`filetree`] — walk a BIDS dataset directory into a [`filetree::FileTree`], honouring
//!   `.bidsignore`, hidden-file, and always-ignore rules.
//! - [`entities`] — parse a BIDS filename into its entities, suffix, and extension.
//! - [`inheritance`] — resolve a data file's effective JSON sidecar via the BIDS inheritance
//!   principle, and find associated files.
//!
//! These were extracted from the `bids-validator-rs` crate so that both the validator and
//! other tools (e.g. `bidslake`) can share one implementation. The crate deliberately has a
//! light dependency footprint (`tokio` + `ignore` + `serde_json`) so consumers pull in nothing
//! validation-specific.

pub mod entities;
pub mod filetree;
pub mod inheritance;
