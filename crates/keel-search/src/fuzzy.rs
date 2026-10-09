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

    pub fn search(&mut self, pattern: &str, max: usize) -> Vec<(u32, String)> {
        self.matcher
            .pattern
            .reparse(0, pattern, CaseMatching::Smart, Normalization::Smart, false);
        self.matcher.tick(10);

        let snapshot = self.matcher.snapshot();
        let end = (max as u32).min(snapshot.matched_item_count());
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
        self.matcher.tick(10);
    }
}
