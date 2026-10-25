//! Checks a search box query against Keel's search language (keel-query).

/// Map a search tab's Everything toggles onto keel-query's options.
// shortcut: no caller outside tests since the merge dropped the app-side refine layer; use it
// where the search box checks a query under the tab's toggles, or remove it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn options(t: crate::search_tab::Toggles) -> keel_query::Options {
    keel_query::Options {
        match_case: t.match_case,
        match_whole_words: t.whole_word,
        match_path: t.match_path,
        match_diacritics: t.match_diacritics,
        regex: t.regex,
        match_prefix: t.prefix,
        match_suffix: t.suffix,
        ignore_punctuation: t.ignore_punctuation,
        ignore_whitespace: t.ignore_whitespace,
        ..Default::default()
    }
}

fn describe(text: &str, e: &keel_query::Error) -> String {
    let mut snippet = text.get(e.span.clone()).unwrap_or("");
    if snippet.is_empty() {
        snippet = text.get(e.span.start..).unwrap_or("");
    }
    if snippet.is_empty() {
        snippet = crate::i18n::tr("search_tab.end_of_query");
    }
    crate::i18n::tr("search_tab.problem_at")
        .replace("{problem}", &e.message)
        .replace("{part}", snippet)
}

/// The problem with `text` as a search query, if the parser rejects it.
pub fn problem(text: &str) -> Option<String> {
    problem_with(text, &keel_query::Options::default())
}

/// The problem with `text` as a search query under `opts`, if the parser rejects it.
pub fn problem_with(text: &str, opts: &keel_query::Options) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    keel_query::parse(text, opts)
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
    use super::{native_problem, options, problem, problem_with};

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
    fn the_problem_is_worded_in_the_interface_language() {
        crate::i18n::set("es");
        let p = problem("size:big").expect("size:big should be rejected");
        let end = problem("<report").expect("an unclosed group should be rejected");
        crate::i18n::set("en");
        assert!(p.contains(" en \""), "{p}");
        assert!(problem("size:big").unwrap().contains(" at \""));
        assert!(!end.is_empty());
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
            prefix: true,
            suffix: true,
            ignore_punctuation: true,
            ignore_whitespace: true,
        };
        let o = options(t);
        assert!(o.match_case);
        assert!(o.match_whole_words);
        assert!(o.match_path);
        assert!(o.match_diacritics);
        assert!(o.regex);
        assert!(o.match_prefix);
        assert!(o.match_suffix);
        assert!(o.ignore_punctuation);
        assert!(o.ignore_whitespace);
    }

    #[test]
    fn regex_mode_reports_a_bad_regex() {
        let t = crate::search_tab::Toggles {
            regex: true,
            ..Default::default()
        };
        assert!(problem_with("a(", &options(t)).is_some());
    }

    #[test]
    fn problem_wording_is_translated() {
        crate::i18n::set("es");
        let es = problem("size:big").expect("size:big should be rejected");
        let es_end = crate::i18n::tr("search_tab.end_of_query");
        crate::i18n::set("en");
        let en = problem("size:big").expect("size:big should be rejected");
        let en_end = crate::i18n::tr("search_tab.end_of_query");
        assert!(es.contains("en \"big\""));
        assert!(!es.contains("at \"big\""));
        assert!(en.contains("at \"big\""));
        assert_ne!(es_end, en_end);
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
