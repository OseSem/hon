use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceId {
    MangaDex,
    MangaKatana,
    WeebCentral,
}

impl SourceId {
    pub const ALL: [SourceId; 3] = [
        SourceId::MangaDex,
        SourceId::MangaKatana,
        SourceId::WeebCentral,
    ];

    pub fn name(self) -> &'static str {
        match self {
            SourceId::MangaDex => "MangaDex",
            SourceId::MangaKatana => "MangaKatana",
            SourceId::WeebCentral => "WeebCentral",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            SourceId::MangaDex => "mangadex",
            SourceId::MangaKatana => "mangakatana",
            SourceId::WeebCentral => "weebcentral",
        }
    }

    pub fn from_slug(slug: &str) -> Option<SourceId> {
        SourceId::ALL
            .into_iter()
            .find(|s| s.slug().eq_ignore_ascii_case(slug))
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn source_slugs_round_trip() {
        for s in SourceId::ALL {
            assert_eq!(SourceId::from_slug(&s.slug().to_uppercase()), Some(s));
        }
    }
}
