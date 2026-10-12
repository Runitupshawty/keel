//! Search modifiers (`case:`, `regex:`, `ww:`, ...): every modifier on Everything's search
//! modifiers page, with its `no-` form and aliases, plus Everything 1.4's spellings
//! (`nocase:`, `wholeword:`, `nowfn:`, ...).

/// A search modifier. Modifiers prefix a term or a function (`case:regex:^a`,
/// `nopath:ww:x`, `metric:size:>1mb`); the ones marked global change the whole query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Modifier {
    Case(bool),
    Diacritics(bool),
    WholeWords(bool),
    Path(bool),
    Prefix(bool),
    Suffix(bool),
    StartWith,
    EndWith,
    Whole(bool),
    Regex(bool),
    Wildcards(bool),
    File,
    Folder,
    IgnorePunctuation(bool),
    IgnoreWhitespace(bool),
    Len,
    Metric(bool),
    Hex,
    HexToNumber,
    Binary,
    ToInteger,
    ToNumber,
    ToString,
    MultiString(bool),
    AllStrings,
    EachLine,
    MultiLine,
    DotAll,
    Ungreedy,
    Global,
    StarAll,
    Soundex,
    Pinyin(bool),
    PinyinJian,
    PinyinQuan,
    NoSubstitute,
    NoFastRegex,
    NoHighlight,
    FromDisk,
    /// Global: keep the terms in the order typed.
    NoReorder,
    /// Global: search within the current results (`true`) or force a new search.
    SearchCurrentResults(bool),
    /// Global: combine the selected filters with AND.
    AndFilter,
    /// Global: combine the selected filters with OR.
    OrFilter,
    /// Global: result omissions on or off.
    ResultOmissions(bool),
    /// Global: temporary result omissions on or off.
    TempResultOmissions(bool),
    /// `no-!:`: `!` in this term is text, not NOT.
    NoNot,
    /// Global: AND binds before OR.
    AndPrecedence,
    /// Global: OR binds before AND (Everything's default).
    OrPrecedence,
    /// Global, for filter and bookmark menus (Phase 16): start a new menu column.
    MenuBreak,
    /// Global, for filter and bookmark menus (Phase 16): show as a separator.
    MenuSeparator,
}

/// One row of the modifier table.
pub struct ModifierInfo {
    /// Canonical name first, then aliases (all lower case, without the `:`).
    pub names: &'static [&'static str],
    pub modifier: Modifier,
    /// What it does in Keel.
    pub summary: &'static str,
}

macro_rules! m {
    ($names:expr, $m:expr, $s:expr) => {
        ModifierInfo {
            names: $names,
            modifier: $m,
            summary: $s,
        }
    };
}

use Modifier as M;

/// Every modifier Keel reads, in Everything's order.
pub static MODIFIERS: &[ModifierInfo] = &[
    m!(&["all-strings"], M::AllStrings, "every `;`-separated value of the property must match"),
    m!(&["and-filter"], M::AndFilter, "global: combine the selected filters with AND"),
    m!(&["and-precedence"], M::AndPrecedence, "global: AND binds before OR"),
    m!(&["binary"], M::Binary, "compare bytes exactly (no case or accent folding)"),
    m!(&["case"], M::Case(true), "match case"),
    m!(&["no-case", "nocase"], M::Case(false), "ignore case (the default)"),
    m!(&["diacritics", "diac"], M::Diacritics(true), "match accents (é is not e)"),
    m!(&["no-diacritics", "nodiacritics", "no-diac", "nodiac"], M::Diacritics(false), "ignore accents (the default)"),
    m!(&["dot-all"], M::DotAll, "regex: `.` also matches a line break"),
    m!(&["each-line"], M::EachLine, "match each line of the value on its own"),
    m!(&["end-with", "endwith", "$"], M::EndWith, "the name (or value) ends with the text"),
    m!(&["no-fast-regex"], M::NoFastRegex, "accepted; Keel's regex engine has no separate fast path"),
    m!(&["file", "files"], M::File, "files only"),
    m!(&["folder", "folders"], M::Folder, "folders only"),
    m!(&["global"], M::Global, "accepted; affects highlighting of every regex match (Phase 16)"),
    m!(&["hex"], M::Hex, "the text is two-digit hex codes (`hex:4b65656c`)"),
    m!(&["hex-to-number"], M::HexToNumber, "read the name or value as a hex number and compare numerically"),
    m!(&["no-highlight"], M::NoHighlight, "accepted; the term is not highlighted (Phase 16)"),
    m!(&["from-disk", "fromdisk"], M::FromDisk, "accepted; read the property from disk rather than the index (the caller's `Props` decides)"),
    m!(&["len"], M::Len, "compare the length of the name (or value) in UTF-16 units: `len:>20`"),
    m!(&["menu-break"], M::MenuBreak, "global: start a new filter-menu column (Phase 16)"),
    m!(&["menu-separator"], M::MenuSeparator, "global: show the filter as a menu separator (Phase 16)"),
    m!(&["metric"], M::Metric(true), "1 KB = 1,000 bytes"),
    m!(&["no-metric", "nometric"], M::Metric(false), "1 KB = 1,024 bytes (the default)"),
    m!(&["multi-line", "multiline"], M::MultiLine, "regex: `^` and `$` match at every line"),
    m!(&["multi-string", "multistring"], M::MultiString(true), "split the value on `;` and match any part"),
    m!(&["no-multi-string", "nomultistring"], M::MultiString(false), "match the value as one string"),
    m!(&["no-!"], M::NoNot, "`!` in this term is text"),
    m!(&["or-filter"], M::OrFilter, "global: combine the selected filters with OR"),
    m!(&["or-precedence"], M::OrPrecedence, "global: OR binds before AND (the default)"),
    m!(&["path", "\\"], M::Path(true), "match the full path"),
    m!(&["no-path", "nopath"], M::Path(false), "match the name only"),
    m!(&["pinyin", "py"], M::Pinyin(true), "Chinese characters by full Pinyin"),
    m!(&["no-pinyin", "nopinyin"], M::Pinyin(false), "no Pinyin matching (the default)"),
    m!(&["pinyin-jian", "pyj"], M::PinyinJian, "Chinese characters by Pinyin initials"),
    m!(&["pinyin-quan", "pyq"], M::PinyinQuan, "Chinese characters by full Pinyin"),
    m!(&["prefix"], M::Prefix(true), "the text starts a word"),
    m!(&["no-prefix", "noprefix"], M::Prefix(false), "match anywhere (the default)"),
    m!(&["ignore-punctuation", "nopunc"], M::IgnorePunctuation(true), "ignore punctuation on both sides"),
    m!(&["no-ignore-punctuation"], M::IgnorePunctuation(false), "punctuation counts (the default)"),
    m!(&["regex"], M::Regex(true), "the text is a regular expression"),
    m!(&["no-regex", "noregex"], M::Regex(false), "the text is not a regular expression"),
    m!(&["enable-result-omissions"], M::ResultOmissions(true), "global: result omissions on"),
    m!(&["disable-result-omissions"], M::ResultOmissions(false), "global: result omissions off"),
    m!(&["no-reorder"], M::NoReorder, "global: evaluate terms in the order typed"),
    m!(&["search-current-results"], M::SearchCurrentResults(true), "global: search within the current results"),
    m!(&["no-search-current-results"], M::SearchCurrentResults(false), "global: always search everything"),
    m!(&["soundex"], M::Soundex, "the name sounds like the text (American Soundex)"),
    m!(&["star-all"], M::StarAll, "`*` also matches `\\`"),
    m!(&["start-with", "startwith", "^"], M::StartWith, "the name (or value) starts with the text"),
    m!(&["no-substitute"], M::NoSubstitute, "`$property:` is text"),
    m!(&["suffix"], M::Suffix(true), "the text ends a word"),
    m!(&["no-suffix", "nosuffix"], M::Suffix(false), "match anywhere (the default)"),
    m!(&["enable-temp-result-omissions"], M::TempResultOmissions(true), "global: temporary result omissions on"),
    m!(&["disable-temp-result-omissions"], M::TempResultOmissions(false), "global: temporary result omissions off"),
    m!(&["to-integer"], M::ToInteger, "read the name (or value) as an integer and compare numerically"),
    m!(&["to-number", "#"], M::ToNumber, "read the name (or value) as a number and compare numerically"),
    m!(&["to-string"], M::ToString, "compare a number property as text"),
    m!(&["ungreedy"], M::Ungreedy, "regex: quantifiers are lazy"),
    m!(&["whole", "whole-filename", "wholefilename", "wfn", "entire"], M::Whole(true), "the whole name (or value) equals the text"),
    m!(&["no-whole", "no-whole-filename", "nowholefilename", "nowfn"], M::Whole(false), "match part of the name (the default)"),
    m!(&["ignore-whitespace", "nows"], M::IgnoreWhitespace(true), "ignore spaces on both sides"),
    m!(&["no-ignore-whitespace"], M::IgnoreWhitespace(false), "spaces count (the default)"),
    m!(&["whole-words", "ww", "wholeword", "wholewords", "whole-word"], M::WholeWords(true), "match whole words"),
    m!(&["no-whole-words", "noww", "no-ww", "nowholeword", "nowholewords"], M::WholeWords(false), "match inside words (the default)"),
    m!(&["wildcards"], M::Wildcards(true), "`*` and `?` are wildcards (the default)"),
    m!(&["no-wildcards", "nowildcards"], M::Wildcards(false), "`*` and `?` are text"),
];

/// The modifier called `name` (lower case, without the `:`).
pub fn lookup(name: &str) -> Option<Modifier> {
    MODIFIERS
        .iter()
        .find(|m| m.names.contains(&name))
        .map(|m| m.modifier)
}

impl Modifier {
    /// Modifiers that change the whole query rather than one term.
    pub fn is_global(self) -> bool {
        matches!(
            self,
            M::NoReorder
                | M::SearchCurrentResults(_)
                | M::AndFilter
                | M::OrFilter
                | M::ResultOmissions(_)
                | M::TempResultOmissions(_)
                | M::AndPrecedence
                | M::OrPrecedence
                | M::MenuBreak
                | M::MenuSeparator
        )
    }

    /// The modifier's canonical name.
    pub fn name(self) -> &'static str {
        MODIFIERS
            .iter()
            .find(|m| m.modifier == self)
            .map_or("?", |m| m.names[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_lower_case() {
        let mut seen = std::collections::HashSet::new();
        for m in MODIFIERS {
            for n in m.names {
                assert_eq!(*n, n.to_ascii_lowercase());
                assert!(seen.insert(*n), "{n} twice");
            }
        }
    }
}
