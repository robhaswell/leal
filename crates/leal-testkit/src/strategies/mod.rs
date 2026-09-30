//! Proptest strategies (DESIGN §5, test layer 3).
//!
//! - [`bytes`]: arbitrary bytes, biased towards CSV-significant ones. Use it
//!   to check that code never panics and that F1 holds for any input.
//! - [`csv`]: a model-based generator of CSV-like files that also returns
//!   the structure a parser must find. Use it as the oracle for the parser.
//!
//! Neither strategy sets a case count. Tests use proptest's default config,
//! which reads `PROPTEST_CASES` from the environment (default 256), so
//! `just test-deep` can run many more cases without code changes.

pub mod bytes;
pub mod csv;
