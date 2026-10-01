//! Leal's CSV engine: everything that matters for correctness and performance.

// No `unwrap`/`expect` outside tests: see `[workspace.lints.clippy]` in the
// root Cargo.toml.
#![warn(clippy::unwrap_used, clippy::expect_used)]

pub mod attributes;
pub mod detect;
pub mod dialect;
pub mod index;
mod inspect;
pub mod rows;
pub mod source;

pub use inspect::{FIRST_LINE_MAX_BYTES, FileSummary, inspect_file};

/// Returns the version of `leal-core`, for example `"0.0.0"`.
///
/// ```
/// assert!(!leal_core::version().is_empty());
/// ```
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_the_package() {
        assert_eq!(version(), "0.0.0");
    }
}
