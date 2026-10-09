use std::sync::Arc;

use nucleo::{pattern::CaseMatching, pattern::Normalization, Config, Matcher, Nucleo, Utf32String};

/// Incremental fuzzy matcher used by folder jump and command filtering.
pub struct Fuzzy {
    matcher: Nucleo<String>,
}

impl Fuzzy {
    pub fn new(items: Vec<String>) -> Self {
        let mut fuzzy = Self {
            matcher: Nucleo::new(Config::DEFAULT.match_paths(), Arc::new(|| {}), None, 1),
        };
        fuzzy.inject(items);
        fuzzy
    }

    pub fn set_items(&mut self, items: Vec<String>) {
        *self = Self::new(items);
    }

    /// Blocks until nucleo has matched every item against `pattern`; call it
    /// off the UI thread for large item sets.
    pub fn search(&mut self, pattern: &str, max: usize) -> Vec<(u32, String)> {
        self.matcher
            .pattern
            .reparse(0, pattern, CaseMatching::Smart, Normalization::Smart, false);
        self.drain();

        let snapshot = self.matcher.snapshot();
        let max = u32::try_from(max).unwrap_or(u32::MAX);
        let end = max.min(snapshot.matched_item_count());
        let mut scorer = Matcher::new(Config::DEFAULT.match_paths());
        snapshot
            .matched_items(..end)
            .filter_map(|item| {
                snapshot
                    .pattern()
                    .score(item.matcher_columns, &mut scorer)
                    .map(|score| (score, item.data.clone()))
            })
            .collect()
    }

    fn inject(&mut self, items: Vec<String>) {
        let injector = self.matcher.injector();
        for item in items {
            injector.push(item, |value, columns| {
                columns[0] = Utf32String::from(value.as_str());
            });
        }
        self.drain();
    }

    /// A single `tick` may return while the worker is still matching, leaving
    /// the previous pattern's snapshot in place.
    fn drain(&mut self) {
        while self.matcher.tick(10).running {}
    }
}

#[cfg(test)]
mod tests {
    use super::Fuzzy;

    #[test]
    fn search_waits_for_the_current_pattern_on_large_sets() {
        let mut items: Vec<String> = (0..200_000)
            .map(|i| format!(r"C:\data\dir{}\sub{}\file{i}.txt", i % 1000, i % 97))
            .collect();
        items.push(r"E:\archive\qzxvunique\notes".into());
        let mut fuzzy = Fuzzy::new(items);

        assert!(!fuzzy.search("file1", 10).is_empty());
        let hits = fuzzy.search("qzxvunique", 10);
        assert_eq!(hits[0].1, r"E:\archive\qzxvunique\notes");
    }

    #[test]
    fn huge_max_is_clamped_to_the_match_count() {
        let mut fuzzy = Fuzzy::new(vec!["a".into(), "b".into()]);
        assert_eq!(fuzzy.search("", usize::MAX).len(), 2);
    }
}
