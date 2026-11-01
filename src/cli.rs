use clap::{CommandFactory, Parser, Subcommand};
use clap_mangen::Man;

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
    Search {
        /// Title or part of one
        query: String,
    },
    /// Download chapters, e.g. `hon download "solo leveling" 1-10`
    Download {
        /// Title; must match one series exactly, or be the only search result
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
        /// Title; must match one series exactly, or be the only search result
        title: String,
        /// Chapter number, e.g. `112` or `10.5`
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

/// One page for the whole CLI: the top-level command, then a COMMANDS section with each
/// subcommand's synopsis, description and options.
pub fn man_page() -> Vec<u8> {
    let mut cmd = Cli::command()
        .disable_help_subcommand(true)
        .subcommand_value_name("command")
        .mut_subcommands(|sub| sub.disable_help_flag(true));
    cmd.build();
    let top = Man::new(cmd.clone());
    let mut page = String::from(APOSTROPHE_PREAMBLE);
    page += &render(
        &top,
        &[
            Man::render_title,
            Man::render_name_section,
            Man::render_synopsis_section,
            Man::render_description_section,
            Man::render_options_section,
        ],
    );
    page += ".SH COMMANDS\n";
    for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        page += &format!(".SS \"{}\"\n", sub.get_bin_name().expect("built"));
        let man = Man::new(sub.clone());
        let body = render(
            &man,
            &[
                Man::render_synopsis_section,
                Man::render_description_section,
                Man::render_options_section,
            ],
        );
        // Each command is a subsection: drop its section headings, keeping the description
        // a paragraph of its own.
        for line in body.lines() {
            match line {
                ".SH DESCRIPTION" => page += ".PP",
                _ if line.starts_with(".SH ") => continue,
                _ => page += line,
            }
            page += "\n";
        }
    }
    page += &render(&top, &[Man::render_version_section]);
    page.into_bytes()
}

/// clap_mangen starts every render with this; the page needs it once.
const APOSTROPHE_PREAMBLE: &str = ".ie \\n(.g .ds Aq \\(aq\n.el .ds Aq '\n";

type Section = fn(&Man, &mut dyn std::io::Write) -> std::io::Result<()>;

fn render(man: &Man, sections: &[Section]) -> String {
    let mut out = String::new();
    for section in sections {
        let mut buf = Vec::new();
        section(man, &mut buf).expect("writing to a Vec can't fail");
        let text = String::from_utf8(buf).expect("roff output is UTF-8");
        out += text
            .strip_prefix(APOSTROPHE_PREAMBLE)
            .expect("clap_mangen output starts with the Aq preamble");
    }
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
    fn man_page_documents_commands_inline() {
        let page = String::from_utf8(man_page()).unwrap();
        for heading in [
            ".SH COMMANDS",
            ".SS \"hon search\"",
            ".SS \"hon download\"",
            ".SS \"hon read\"",
        ] {
            assert!(page.contains(heading), "{heading} missing:\n{page}");
        }
        for option in [
            "\\fB\\-\\-source\\fR",
            "\\fB\\-\\-format\\fR",
            "\\fICHAPTERS\\fR",
        ] {
            assert!(page.contains(option), "{option} missing:\n{page}");
        }
        assert!(page.contains("Possible values:"), "{page}");
        assert!(!page.contains("hon\\-search(1)"), "{page}");
        assert!(!page.contains("hon\\-help(1)"), "{page}");
        assert!(!page.contains("hon\\-man"), "{page}");
        assert_eq!(page.matches(".TH ").count(), 1, "{page}");
        assert_eq!(page.matches(".ds Aq").count(), 2, "{page}");
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
