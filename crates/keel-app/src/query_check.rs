//! Checks a search box query against Keel's search language (keel-query).

/// Map a search tab's Everything toggles onto keel-query's options.
pub fn options(t: crate::search_tab::Toggles) -> keel_query::Options {
    keel_query::Options {
        match_case: t.match_case,
        match_whole_words: t.whole_word,
        match_path: t.match_path,
        match_diacritics: t.match_diacritics,
        regex: t.regex,
        ..Default::default()
    }
}

/// The problem with `text` as a search query, if the parser rejects it.
#[allow(dead_code)] // pub API for the search box; not yet called outside tests
pub fn problem(text: &str) -> Option<String> {
    problem_with(text, &keel_query::Options::default())
}

/// The problem with `text` as a search query under `opts`, if the parser rejects it.
pub fn problem_with(text: &str, opts: &keel_query::Options) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    match keel_query::parse(text, opts) {
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
    use super::{options, problem, problem_with};

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
    fn toggles_map_to_query_options() {
        let t = crate::search_tab::Toggles {
            match_case: true,
            whole_word: true,
            match_path: true,
            match_diacritics: true,
            regex: true,
        };
        let o = options(t);
        assert!(o.match_case);
        assert!(o.match_whole_words);
        assert!(o.match_path);
        assert!(o.match_diacritics);
        assert!(o.regex);
    }

    #[test]
    fn regex_mode_reports_a_bad_regex() {
        let t = crate::search_tab::Toggles {
            regex: true,
            ..Default::default()
        };
        assert!(problem_with("a(", &options(t)).is_some());
    }
}
