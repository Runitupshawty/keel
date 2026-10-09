//! Search backends and fuzzy matching for Keel.

/// Returns this crate's package name.
pub fn crate_name() -> &'static str {
    "keel-search"
}

#[cfg(test)]
mod tests {
    use super::crate_name;

    #[test]
    fn reports_crate_name() {
        assert_eq!(crate_name(), "keel-search");
    }
}
