//! Dependency-free, case-insensitive search for media metadata.

/// A normalized searchable field. Reuse it across queries when the data is unchanged.
pub(crate) struct SearchField {
    chars: Vec<char>,
}

impl SearchField {
    pub(crate) fn new(text: &str) -> Self {
        Self { chars: text.to_lowercase().chars().collect() }
    }
}

/// A query prepared once for matching against multiple media items.
pub(crate) struct SearchQuery {
    tokens: Vec<Vec<char>>,
}

impl SearchQuery {
    pub(crate) fn new(text: &str) -> Self {
        Self {
            tokens: text.split_whitespace().map(|token| token.to_lowercase().chars().collect()).collect(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Every token must match at least one field. Higher scores are better.
    /// Field order does not affect the score; callers choose their result order.
    pub(crate) fn score(&self, fields: &[SearchField]) -> Option<u64> {
        let mut total = 0u64;
        for token in &self.tokens {
            let best = fields.iter().filter_map(|field| token_score(token, &field.chars)).max()?;
            total = total.saturating_add(best);
        }
        Some(total)
    }
}

fn token_score(token: &[char], text: &[char]) -> Option<u64> {
    if token.len() > text.len() {
        return None;
    }
    if token == text {
        return Some(600);
    }
    if text.starts_with(token) {
        return Some(500);
    }
    let mut substring = false;
    for (start, window) in text.windows(token.len()).enumerate() {
        if window == token {
            if start > 0 && !text[start - 1].is_alphanumeric() {
                return Some(400);
            }
            substring = true;
        }
    }
    if substring {
        return Some(300);
    }

    // Greedy subsequence matching, with a bounded penalty for gaps and late starts.
    // This is linear in the field length and does not allocate while matching.
    let mut matched = 0;
    let mut first = 0;
    for (i, &ch) in text.iter().enumerate() {
        if ch == token[matched] {
            if matched == 0 {
                first = i;
            }
            matched += 1;
            if matched == token.len() {
                let gaps = i - first + 1 - matched;
                let penalty = gaps.saturating_mul(3).saturating_add(first).min(99);
                return Some(199 - penalty as u64);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, fields: &[&str]) -> Option<u64> {
        let fields: Vec<_> = fields.iter().map(|field| SearchField::new(field)).collect();
        SearchQuery::new(query).score(&fields)
    }

    #[test]
    fn empty_query_matches_everything() {
        assert!(SearchQuery::new(" \t\n ").is_empty());
        assert_eq!(score("  ", &[]), Some(0));
        assert_eq!(score("", &["Alien"]), Some(0));
        assert_eq!(score("alien", &[]), None);
    }

    #[test]
    fn ranks_match_types() {
        let fields = ["alien", "aliens", "the alien", "xalien", "a-l-i-e-n"];
        let scores: Vec<_> = fields.iter().map(|field| score("alien", &[*field]).unwrap()).collect();
        assert!(scores.windows(2).all(|pair| pair[0] > pair[1]));
    }

    #[test]
    fn matches_abbreviations_in_order() {
        assert!(score("mtn lnd", &["Mountain Landscape.mp4"]).is_some());
        assert_eq!(score("lien a", &["Alien"]), Some(800));
        assert_eq!(score("neila", &["Alien"]), None);
        assert_eq!(score("zz", &["Alien"]), None);
        assert_eq!(score("aa", &["Alien"]), None);
    }

    #[test]
    fn compact_subsequences_rank_higher() {
        assert!(score("abc", &["a-b-c"]) > score("abc", &["a----b----c"]));
    }

    #[test]
    fn tokens_can_match_different_fields() {
        assert!(score("scifi alien", &["Science Fiction", "Alien (1979)"]).is_some());
        assert_eq!(score("alien missing", &["Alien", "Science Fiction"]), None);
        assert_eq!(score("alien scifi", &["Alien", "Science Fiction"]), score("scifi alien", &["Science Fiction", "Alien"]));
    }

    #[test]
    fn unicode_case_is_preserved() {
        assert_eq!(score("ČĆŽŠĐ", &["čćžšđ"]), Some(600));
        assert!(score("žš", &["Život Šume"]).is_some());
        assert_eq!(score("c", &["č"]), None);
        assert!(score("東京", &["映画 東京"]).is_some());
    }
}
