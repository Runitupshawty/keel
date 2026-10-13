//! Checks a search box query against Keel's search language (keel-query).

/// The problem with `text` as a search query, if the parser rejects it.
pub fn problem(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    match keel_query::parse(text, &keel_query::Options::default()) {
        Ok(_) => None,
        Err(e) => {
            let mut snippet = text.get(e.span.clone()).unwrap_or("");
            if snippet.is_empty() {
                snippet = text.get(e.span.start..).unwrap_or("");
            }
            if snippet.is_empty() {
                snippet = "end of query";
            }
            Some(format!("{} at \"{}\"", e.message, snippet))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::problem;

    #[test]
    fn a_valid_query_has_no_problem() {
        assert!(problem("ext:pdf size:>1mb").is_none());
    }

    #[test]
    fn a_bad_value_is_reported() {
        let p = problem("size:big").expect("size:big should be rejected");
        assert!(p.contains("size:big") || p.contains("big"));
    }

    #[test]
    fn an_empty_query_has_no_problem() {
        assert!(problem("").is_none());
    }
}
