//! The `leal` command: opens files in Leal.app and checks CSV files.

// No `unwrap`/`expect` outside tests: see `[workspace.lints.clippy]` in the
// root Cargo.toml.
#![warn(clippy::unwrap_used, clippy::expect_used)]

fn main() {
    println!("{}", version_line());
}

/// The line printed by `leal`, for example `leal 0.0.0`.
fn version_line() -> String {
    format!("leal {}", leal_core::version())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_the_command() {
        assert_eq!(version_line(), "leal 0.0.0");
    }
}
