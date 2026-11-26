use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;

/// A source's stable id (`slug`, stored in the database and taken by `--source`) and its
/// display name. Ids compare by slug only.
#[derive(Clone, Debug)]
pub struct SourceId {
    slug: Arc<str>,
    name: Arc<str>,
}

impl SourceId {
    pub fn new(slug: &str, name: &str) -> SourceId {
        SourceId {
            slug: slug.into(),
            name: name.into(),
        }
    }

    pub fn slug(&self) -> &str {
        &self.slug
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl PartialEq for SourceId {
    fn eq(&self, other: &Self) -> bool {
        self.slug == other.slug
    }
}

impl Eq for SourceId {}

impl PartialOrd for SourceId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SourceId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.slug.cmp(&other.slug)
    }
}

impl Hash for SourceId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.slug.hash(state);
    }
}

/// A finite, non-negative chapter number; may be fractional (e.g. 10.5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChapterNo(f64);

impl ChapterNo {
    /// Digits with an optional fractional part (`12`, `10.5`), ignoring surrounding space.
    pub fn parse(s: &str) -> Option<ChapterNo> {
        let s = s.trim();
        let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        let valid = match s.split_once('.') {
            Some((whole, fraction)) => digits(whole) && digits(fraction),
            None => digits(s),
        };
        if !valid {
            return None;
        }
        let v: f64 = s.parse().ok()?;
        v.is_finite().then_some(ChapterNo(v))
    }

    pub fn value(self) -> f64 {
        self.0
    }
}

impl fmt::Display for ChapterNo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Eq for ChapterNo {}

impl PartialOrd for ChapterNo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ChapterNo {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl Hash for ChapterNo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MangaRef {
    pub source: SourceId,
    pub id: String,
    pub title: String,
    pub alt_titles: Vec<String>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChapterRef {
    pub source: SourceId,
    pub id: String,
    pub number: ChapterNo,
    pub title: Option<String>,
    pub published: Option<jiff::Timestamp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageRef {
    pub url: String,
}

/// Lowercase, replace every non-alphanumeric char with a space, collapse whitespace.
pub fn normalize_title(s: &str) -> String {
    let normalized: String = s.nfc().collect();
    let spaced: String = normalized
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(any(test, feature = "test-util"))]
pub mod test_ids {
    use super::SourceId;

    pub fn alpha() -> SourceId {
        SourceId::new("alpha", "Alpha")
    }

    pub fn beta() -> SourceId {
        SourceId::new("beta", "Beta")
    }

    pub fn gamma() -> SourceId {
        SourceId::new("gamma", "Gamma")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_ids_compare_by_slug_only() {
        assert_eq!(SourceId::new("a", "A"), SourceId::new("a", "Other"));
        assert_ne!(SourceId::new("a", "A"), SourceId::new("b", "A"));
    }

    #[test]
    fn chapter_no_parses_and_displays() {
        assert_eq!(ChapterNo::parse("112").unwrap().to_string(), "112");
        assert_eq!(ChapterNo::parse("112.0").unwrap().to_string(), "112");
        assert_eq!(ChapterNo::parse("10.5").unwrap().to_string(), "10.5");
        assert_eq!(ChapterNo::parse(" 7 ").unwrap().to_string(), "7");
        let too_big = "9".repeat(400);
        for bad in [
            "", "abc", "-1", "NaN", "inf", "1e3", "+5", ".5", "5.", "1.2.3", "1 2", "١", &too_big,
        ] {
            assert!(ChapterNo::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn chapter_no_orders_fractions() {
        let mut v: Vec<_> = ["2", "10.5", "10", "1"]
            .iter()
            .map(|s| ChapterNo::parse(s).unwrap())
            .collect();
        v.sort();
        assert_eq!(
            v.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            ["1", "2", "10", "10.5"]
        );
    }

    #[test]
    fn normalize_title_strips_punctuation() {
        assert_eq!(
            normalize_title("Solo Leveling: Ragnarok"),
            "solo leveling ragnarok"
        );
        assert_eq!(
            normalize_title("  Re:Zero  -Starting Life- "),
            "re zero starting life"
        );
        assert_eq!(normalize_title("Ōkami"), "ōkami");
    }

    #[test]
    fn normalize_title_matches_composed_and_decomposed_unicode() {
        let composed = normalize_title("Ōkami");
        let decomposed = normalize_title("O\u{0304}kami");

        assert_eq!(composed, "ōkami");
        assert_eq!(decomposed, composed);
    }
}
