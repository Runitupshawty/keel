//! Checks a search box query against Keel's search language (keel-query).

fn describe(text: &str, e: &keel_query::Error) -> String {
    let mut snippet = text.get(e.span.clone()).unwrap_or("");
    if snippet.is_empty() {
        snippet = text.get(e.span.start..).unwrap_or("");
    }
    if snippet.is_empty() {
        snippet = "end of query";
    }
    format!("{} at \"{}\"", e.message, snippet)
}

/// The problem with `text` as a search query, if the parser rejects it.
pub fn problem(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    keel_query::parse(text, &keel_query::Options::default())
        .err()
        .map(|e| describe(text, &e))
}

/// The problem with `query` for Keel's own backends, which read it with the search tab's
/// options (and a regex that does not compile is a problem too).
pub fn native_problem(query: &keel_search::Query) -> Option<String> {
    if query.text.trim().is_empty() {
        return None;
    }
    let error = keel_search::compile_query(query).err()?;
    Some(match error.downcast_ref::<keel_query::Error>() {
        Some(e) => describe(&query.text, e),
        None => format!("{error:#}"),
    })
}

#[cfg(test)]
mod tests {
    use super::{native_problem, problem};

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

    #[test]
    fn keels_own_backends_check_the_query_with_the_tabs_options() {
        let query = |text: &str, regex| keel_search::Query {
            text: text.into(),
            regex,
            ..keel_search::Query::default()
        };
        assert!(native_problem(&query("report | ext:pdf !draft", false)).is_none());
        assert!(
            native_problem(&query("report notes OR", false)).is_none(),
            "OR is a word"
        );
        let p = native_problem(&query("report <notes", false)).unwrap();
        assert!(p.contains('<'), "{p}");
        assert!(
            native_problem(&query("(", true)).is_some(),
            "a regex mode query"
        );
        assert!(native_problem(&query("", false)).is_none());
    }
}
