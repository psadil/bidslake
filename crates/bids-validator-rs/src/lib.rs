//! A pure-Rust BIDS (Brain Imaging Data Structure) validator.
//!
//! # State of the Project
//!
//! This project is currently **UNSTABLE** and under active development.
//!
//! # References
//!
//! - [BIDS Standard](https://bids.neuroimaging.io/)
//! - [BIDS Specification](https://bids-specification.readthedocs.io/)
//! - [Official BIDS Validator](https://github.com/bids-standard/bids-validator)
//!
//! # TODOs
//!
//! - HED Validation
//! - Wider testing
//! - Mechanisms for using different versions of schema
//! - Benchmarking against typescript validation (and python-based validation)

pub mod associations;
pub mod config;
pub mod context;
pub mod files;
pub mod issues;
pub mod rules;
pub mod schema;
pub mod validator;

// The BIDS filetree walker, entity parser, and inheritance resolution now live in the shared
// `bids-core` crate. Re-export them so `crate::{entities, filetree, inheritance}` and the public
// `bids_validator_rs::{entities, filetree, inheritance}` paths keep resolving unchanged.
pub use bids_core::{entities, filetree, inheritance};
// The expression evaluator now lives in the shared `bids-schema` crate; re-export it so
// `crate::expression::…` and the public `bids_validator_rs::expression` path keep resolving.
pub use bids_schema::expression;

pub use validator::validate;
