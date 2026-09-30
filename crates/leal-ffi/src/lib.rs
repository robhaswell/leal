//! Thin wrapper that exposes `leal-core` to Swift. It holds no logic.

/// Returns the version of `leal-core` this library was built with.
#[must_use]
pub fn core_version() -> &'static str {
    leal_core::version()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_version_comes_from_core() {
        assert_eq!(core_version(), leal_core::version());
    }
}
