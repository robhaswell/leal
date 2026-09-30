//! Test-only helpers for Leal. Use this crate only as a dev-dependency:
//!
//! ```toml
//! [dev-dependencies]
//! leal-testkit.workspace = true
//! proptest.workspace = true
//! ```
//!
//! - [`fidelity`]: `assert_identical` (F1) and `assert_only_changed`
//!   (F2, F3, F6) with short, useful failure reports.
//! - [`strategies`]: proptest strategies for arbitrary bytes and for
//!   model-generated CSV-like files with their expected structure.
//! - [`corpus`]: loads `tests/corpus/` and its `*.expected.toml` sidecars.
//! - [`layout`], [`diagnostics`], [`dialect`]: the vocabulary those share.
//!   These are the testkit's own, independent definitions; `leal-core` has
//!   its own types, and tests convert between them.

pub mod corpus;
pub mod diagnostics;
pub mod dialect;
pub mod fidelity;
pub mod layout;
pub mod strategies;
