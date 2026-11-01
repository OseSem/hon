use clap::{CommandFactory, Parser, Subcommand};

use crate::config::Format;
use crate::model::{ChapterNo, SourceId};

#[derive(Parser)]
#[command(name = "hon", version, about = "Read manga in your terminal")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Search every source and print grouped results
    Search { query: String },
    /// Download chapters, e.g. `hon download "solo leveling" 1-10`
    Download {
        title: String,
        /// Chapters: `112`, `1-10`, `1-10,15,20.5` or `45-` (to the latest)
        #[arg(value_parser = parse_range)]
        chapters: ChapterRange,
        /// Source to download from (default: the one with the newest chapter)
        #[arg(long, value_parser = parse_source)]
        source: Option<SourceId>,
        /// File format (default: `format` from the config file, else pdf)
        #[arg(long)]
        format: Option<Format>,
    },
    /// Open the reader at a chapter (default: where you left off)
    Read {
        title: String,
        chapter: Option<String>,
        /// Source to read from (default: the one with the newest chapter)
        #[arg(long, value_parser = parse_source)]
        source: Option<SourceId>,
    },
    #[command(hide = true)]
    Man,
}

/// Inclusive chapter ranges; `None` as the end means open-ended.
#[derive(Clone, Debug)]
pub struct ChapterRange(Vec<(ChapterNo, Option<ChapterNo>)>);

impl ChapterRange {
    pub fn contains(&self, n: ChapterNo) -> bool {
        self.0
            .iter()
            .any(|&(from, to)| from <= n && to.is_none_or(|to| n <= to))
    }
}

pub fn parse_range(s: &str) -> Result<ChapterRange, String> {
    let chapter = |part: &str| {
        ChapterNo::parse(part).ok_or_else(|| format!("{part:?} isn't a chapter number"))
    };
    let mut ranges = Vec::new();
    for part in s.split(',') {
        let range = match part.split_once('-') {
            None => {
                let n = chapter(part)?;
                (n, Some(n))
            }
            Some((from, to)) if to.trim().is_empty() => (chapter(from)?, None),
            Some((from, to)) => {
                let (from, to) = (chapter(from)?, chapter(to)?);
                if from > to {
                    return Err(format!("{part:?} runs backwards"));
                }
                (from, Some(to))
            }
        };
        ranges.push(range);
    }
    Ok(ChapterRange(ranges))
}

pub fn parse_source(s: &str) -> Result<SourceId, String> {
    SourceId::from_slug(s).ok_or_else(|| {
        let valid: Vec<_> = SourceId::ALL.iter().map(|id| id.slug()).collect();
        format!("unknown source {s:?}; valid: {}", valid.join(", "))
    })
}

pub fn man_page() -> Vec<u8> {
    let mut out = Vec::new();
    clap_mangen::Man::new(Cli::command())
        .render(&mut out)
        .expect("writing to a Vec can't fail");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> ChapterNo {
        ChapterNo::parse(s).unwrap()
    }

    #[test]
    fn ranges() {
        let r = parse_range("1-10,15,20.5").unwrap();
        assert!(r.contains(n("10")) && r.contains(n("15")) && r.contains(n("20.5")));
        assert!(!r.contains(n("10.5")) && !r.contains(n("16")));
        assert!(parse_range("45-").unwrap().contains(n("9999")));
        assert!(!parse_range("45-").unwrap().contains(n("44.5")));
        assert!(parse_range(" 112 ").unwrap().contains(n("112")));
        for bad in ["", "a-b", "10-1", "1--2", "1,,2", "-5"] {
            assert!(parse_range(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sources_parse_and_list_valid() {
        assert_eq!(parse_source("MangaDex"), Ok(SourceId::MangaDex));
        let err = parse_source("foo").unwrap_err();
        assert!(
            err.contains("mangadex") && err.contains("mangakatana"),
            "{err}"
        );
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
