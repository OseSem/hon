use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;

use anyhow::{anyhow, bail};

use crate::cli::ChapterRange;
use crate::config::{Config, Format};
use crate::db::{Db, Progress};
use crate::download::{chapter_path, fetch_pages, is_downloaded, write_chapter};
use crate::model::{ChapterNo, ChapterRef, SourceId};
use crate::search::{
    SearchOutcome, SourceSummary, TitleGroup, missing_elsewhere, resolve, search_all,
};
use crate::sources::Source;
use crate::tui::{self, Start};

const SHOWN_GAPS: usize = 2;

pub struct Ctx {
    pub sources: Vec<Arc<dyn Source>>,
    pub db: Db,
    pub config: Config,
}

type Ask<'a> = &'a mut dyn FnMut(&str) -> anyhow::Result<bool>;

pub async fn search(ctx: &Ctx, query: &str, out: &mut impl Write) -> anyhow::Result<()> {
    let outcome = search_all(&ctx.sources, query).await;
    let now = jiff::Timestamp::now();
    for group in found(ctx, query, &outcome)? {
        writeln!(out, "{}", group.title)?;
        for summary in &group.sources {
            writeln!(out, "{}", source_line(summary, now))?;
        }
    }
    Ok(())
}

/// Prints each source failure once as a warning; errors when nothing was found.
fn found<'a>(
    ctx: &Ctx,
    query: &str,
    outcome: &'a SearchOutcome,
) -> anyhow::Result<&'a [TitleGroup]> {
    for line in distinct_failures(&outcome.failures) {
        eprintln!("warning: {line}");
    }
    if !outcome.groups.is_empty() {
        return Ok(&outcome.groups);
    }
    let all_failed = ctx
        .sources
        .iter()
        .all(|s| outcome.failures.iter().any(|(id, _)| *id == s.id()));
    if all_failed {
        bail!("every source failed");
    }
    bail!("no results for \"{query}\"")
}

/// `"<Source> failed: <reason>"`, once per distinct failure.
pub(crate) fn distinct_failures(failures: &[(SourceId, String)]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for (id, reason) in failures {
        let line = format!("{} failed: {reason}", id.name());
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    lines
}

pub(crate) fn source_name_width() -> usize {
    SourceId::ALL
        .iter()
        .map(|id| id.name().len())
        .max()
        .unwrap_or(0)
}

fn source_line(s: &SourceSummary, now: jiff::Timestamp) -> String {
    let name_width = source_name_width();
    let latest = s.latest.map_or("–".to_string(), |n| n.to_string());
    let mut line = format!(
        "  {:<name_width$}   {:>3} ch   latest {latest}",
        s.manga.source.name(),
        s.chapters.len()
    );
    if let Some(updated) = s.updated {
        line += "   ";
        line += &updated_text(updated, now);
    }
    if !s.gaps.is_empty() {
        line += "   gaps: ";
        line += &gaps_text(&s.gaps);
    }
    line
}

pub(crate) fn updated_text(updated: jiff::Timestamp, now: jiff::Timestamp) -> String {
    match now.duration_since(updated).as_secs() / 86_400 {
        ..1 => "updated today".to_string(),
        days => format!("updated {days}d ago"),
    }
}

pub(crate) fn gaps_text(gaps: &[(u32, u32)]) -> String {
    let mut parts: Vec<String> = gaps
        .iter()
        .take(SHOWN_GAPS)
        .map(|&(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}–{b}")
            }
        })
        .collect();
    if gaps.len() > SHOWN_GAPS {
        parts.push(format!("+{} more", gaps.len() - SHOWN_GAPS));
    }
    parts.join(", ")
}

pub async fn download(
    ctx: &Ctx,
    title: &str,
    range: &ChapterRange,
    source: Option<SourceId>,
    format: Option<Format>,
    interactive: bool,
) -> anyhow::Result<()> {
    let mut ask = |question: &str| -> anyhow::Result<bool> {
        let mut stdout = io::stdout();
        write!(stdout, "{question}")?;
        stdout.flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
    };
    let ask: Option<Ask> = if interactive { Some(&mut ask) } else { None };
    download_to(ctx, title, range, source, format, ask, &mut io::stdout()).await
}

async fn download_to(
    ctx: &Ctx,
    title: &str,
    range: &ChapterRange,
    source: Option<SourceId>,
    format: Option<Format>,
    mut ask: Option<Ask<'_>>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let outcome = search_all(&ctx.sources, title).await;
    let (group, on) = resolve_title(ctx, title, source, &outcome)?;
    let summary = group.source(on).expect("resolve_title checks the source");
    let in_range = |c: &&ChapterRef| range.contains(c.number);
    if !group
        .sources
        .iter()
        .any(|s| s.chapters.iter().any(|c| in_range(&c)))
    {
        bail!("\"{}\" has no chapters in that range", group.title);
    }
    // Files go under the series' first title, whichever source titles it today.
    let folder = ctx.db.series(&group.keys, &group.title)?.title;
    let fmt = format.unwrap_or(ctx.config.format);
    let chapters: Vec<&ChapterRef> = summary.chapters.iter().filter(in_range).collect();
    let mut failed = fetch_chapters(ctx, &folder, on, &chapters, fmt, out).await?;

    let dl = &ctx.config.download_dir;
    let mut wanted: Vec<ChapterNo> = group
        .sources
        .iter()
        .flat_map(|s| s.chapters.iter().filter(in_range))
        .map(|c| c.number)
        .filter(|&n| !is_downloaded(dl, &folder, n))
        .collect();
    wanted.sort();
    wanted.dedup();
    let missing = missing_elsewhere(group, on, &wanted);
    for other in group.sources.iter().filter(|s| s.manga.source != on) {
        let other_id = other.manga.source;
        let chapters: Vec<&ChapterRef> = other
            .chapters
            .iter()
            .filter(|c| missing.contains(&(c.number, other_id)))
            .collect();
        if chapters.is_empty() {
            continue;
        }
        let runs = runs(&other.chapters, &chapters);
        let Some(ask) = ask.as_mut() else {
            writeln!(
                out,
                "hint: hon download \"{}\" {} --source {}",
                group.title,
                runs_text(&runs, "-", ","),
                other_id.slug()
            )?;
            continue;
        };
        let (verb, pronoun) = if chapters.len() == 1 {
            ("isn't", "it")
        } else {
            ("aren't", "them")
        };
        let question = format!(
            "Ch {} {verb} on {}. Download {pronoun} from {}? [y/N] ",
            runs_text(&runs, "–", ", "),
            on.name(),
            other_id.name()
        );
        if ask(&question)? {
            failed += fetch_chapters(ctx, &folder, other_id, &chapters, fmt, out).await?;
        }
    }

    match failed {
        0 => Ok(()),
        1 => bail!("1 chapter failed"),
        n => bail!("{n} chapters failed"),
    }
}

/// The one title group `title` names, and the source to use: `source`, else the one with
/// the newest chapter.
fn resolve_title<'a>(
    ctx: &Ctx,
    title: &str,
    source: Option<SourceId>,
    outcome: &'a SearchOutcome,
) -> anyhow::Result<(&'a TitleGroup, SourceId)> {
    let groups = found(ctx, title, outcome)?;
    let group = resolve(title, groups).map_err(|_| ambiguous(title, groups))?;
    let on = source.unwrap_or_else(|| group.best_source());
    if group.source(on).is_none() {
        let names: Vec<_> = group
            .sources
            .iter()
            .map(|s| s.manga.source.name())
            .collect();
        bail!(
            "\"{}\" isn't on {}; it's on {}",
            group.title,
            on.name(),
            names.join(", ")
        );
    }
    Ok((group, on))
}

pub async fn read(
    ctx: Ctx,
    title: &str,
    chapter: Option<&str>,
    source: Option<SourceId>,
) -> anyhow::Result<()> {
    tui::ensure_terminal()?;
    let start = read_start(&ctx, title, chapter, source).await?;
    tui::run(ctx, start).await
}

/// Resolves `hon read`'s title, source and chapter (default: where reading left off).
async fn read_start(
    ctx: &Ctx,
    title: &str,
    chapter: Option<&str>,
    source: Option<SourceId>,
) -> anyhow::Result<Start> {
    let wanted = chapter
        .map(|c| ChapterNo::parse(c).ok_or_else(|| anyhow!("{c:?} isn't a chapter number")))
        .transpose()?;
    let outcome = search_all(&ctx.sources, title).await;
    let (group, on) = resolve_title(ctx, title, source, &outcome)?;
    let on_any_source = |n: ChapterNo| {
        group
            .sources
            .iter()
            .any(|s| s.chapters.iter().any(|c| c.number == n))
    };
    let chapter = match wanted {
        Some(n) if !on_any_source(n) => bail!("\"{}\" has no Ch {n}", group.title),
        Some(n) => n,
        None => {
            let series = ctx.db.series(&group.keys, &group.title)?;
            start_chapter(group, &ctx.db.progress(series.id)?)
                .ok_or_else(|| anyhow!("\"{}\" has no chapters", group.title))?
        }
    };
    Ok(Start::Read {
        group: group.clone(),
        source: on,
        chapter: Some(chapter),
    })
}

/// Of every source's chapters: the first in progress, else the first unread, else the last.
fn start_chapter(group: &TitleGroup, progress: &HashMap<ChapterNo, Progress>) -> Option<ChapterNo> {
    let mut numbers: Vec<ChapterNo> = group
        .sources
        .iter()
        .flat_map(|s| s.chapters.iter().map(|c| c.number))
        .collect();
    numbers.sort();
    numbers.dedup();
    let in_progress = |n: &ChapterNo| {
        progress
            .get(n)
            .is_some_and(|p| !p.read && p.last_page.is_some())
    };
    let unread = |n: &ChapterNo| !progress.get(n).is_some_and(|p| p.read);
    numbers
        .iter()
        .find(|n| in_progress(n))
        .or_else(|| numbers.iter().find(|n| unread(n)))
        .or(numbers.last())
        .copied()
}

fn ambiguous(query: &str, groups: &[TitleGroup]) -> anyhow::Error {
    let mut msg = format!("\"{query}\" matches several titles:");
    for g in groups {
        let names: Vec<_> = g.sources.iter().map(|s| s.manga.source.name()).collect();
        msg += &format!("\n  {} ({})", g.title, names.join(", "));
    }
    anyhow!(msg)
}

/// Downloads `chapters` from `source` into `folder`'s directory, skipping ones already on
/// disk. A failed chapter is reported and skipped; returns how many failed.
async fn fetch_chapters(
    ctx: &Ctx,
    folder: &str,
    source: SourceId,
    chapters: &[&ChapterRef],
    fmt: Format,
    out: &mut impl Write,
) -> anyhow::Result<usize> {
    let src = ctx
        .sources
        .iter()
        .find(|s| s.id() == source)
        .ok_or_else(|| anyhow!("{} isn't registered", source.name()))?;
    let dl = &ctx.config.download_dir;
    let mut failed = 0;
    for ch in chapters {
        if is_downloaded(dl, folder, ch.number) {
            writeln!(out, "Ch {} already downloaded", ch.number)?;
            continue;
        }
        let path = chapter_path(dl, folder, ch.number, fmt);
        let saved = fetch_pages(src.as_ref(), ch, |_, _| {})
            .await
            .and_then(|pages| write_chapter(&pages, fmt, &path).map(|()| pages.len()));
        match saved {
            Ok(1) => writeln!(out, "Ch {} (1 page) → {}", ch.number, path.display())?,
            Ok(n) => writeln!(out, "Ch {} ({n} pages) → {}", ch.number, path.display())?,
            Err(e) => {
                eprintln!("error: Ch {}: {e:#}", ch.number);
                failed += 1;
            }
        }
    }
    Ok(failed)
}

/// Groups `picked` into runs that are consecutive in `all` (a source's sorted chapter list).
fn runs(all: &[ChapterRef], picked: &[&ChapterRef]) -> Vec<(ChapterNo, ChapterNo)> {
    let mut runs: Vec<(ChapterNo, ChapterNo)> = Vec::new();
    let mut extending = false;
    for c in all {
        let is_picked = picked.iter().any(|p| p.number == c.number);
        match (is_picked, extending, runs.last_mut()) {
            (true, true, Some(run)) => run.1 = c.number,
            (true, _, _) => runs.push((c.number, c.number)),
            _ => {}
        }
        extending = is_picked;
    }
    runs
}

fn runs_text(runs: &[(ChapterNo, ChapterNo)], dash: &str, sep: &str) -> String {
    runs.iter()
        .map(|(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}{dash}{b}")
            }
        })
        .collect::<Vec<_>>()
        .join(sep)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::{anyhow, bail};

    use super::*;
    use crate::cli::parse_range;
    use crate::config::Paths;
    use crate::download::{chapter_path, test_images, write_chapter};
    use crate::model::{ChapterNo, ChapterRef, MangaRef, PageRef};

    fn no(s: &str) -> ChapterNo {
        ChapterNo::parse(s).unwrap()
    }

    fn nums(from: u32, to: u32) -> Vec<String> {
        (from..=to).map(|n| n.to_string()).collect()
    }

    struct Fake {
        id: SourceId,
        results: Result<Vec<&'static str>, String>,
        alt_titles: Vec<&'static str>,
        chapters: Vec<String>,
        published: Option<jiff::Timestamp>,
        failing_pages: Vec<ChapterNo>,
        pages_calls: Mutex<Vec<ChapterNo>>,
    }

    impl Fake {
        fn new(id: SourceId, titles: &[&'static str], chapters: Vec<String>) -> Fake {
            Fake {
                id,
                results: Ok(titles.to_vec()),
                alt_titles: Vec::new(),
                chapters,
                published: None,
                failing_pages: Vec::new(),
                pages_calls: Mutex::new(Vec::new()),
            }
        }

        fn failing(id: SourceId) -> Fake {
            Fake {
                results: Err("connection refused".into()),
                ..Fake::new(id, &[], Vec::new())
            }
        }
    }

    #[async_trait::async_trait]
    impl Source for Fake {
        fn id(&self) -> SourceId {
            self.id
        }

        async fn search(&self, _query: &str) -> anyhow::Result<Vec<MangaRef>> {
            let titles = self.results.clone().map_err(|e| anyhow!(e))?;
            Ok(titles
                .iter()
                .enumerate()
                .map(|(i, t)| MangaRef {
                    source: self.id,
                    id: format!("{}-{i}", self.id.slug()),
                    title: t.to_string(),
                    alt_titles: self.alt_titles.iter().map(|a| a.to_string()).collect(),
                    url: String::new(),
                })
                .collect())
        }

        async fn chapters(&self, _manga: &MangaRef) -> anyhow::Result<Vec<ChapterRef>> {
            Ok(self
                .chapters
                .iter()
                .map(|n| ChapterRef {
                    source: self.id,
                    id: n.clone(),
                    number: no(n),
                    title: None,
                    published: self.published,
                })
                .collect())
        }

        async fn pages(&self, ch: &ChapterRef) -> anyhow::Result<Vec<PageRef>> {
            self.pages_calls.lock().unwrap().push(ch.number);
            if self.failing_pages.contains(&ch.number) {
                bail!("HTTP 429 Too Many Requests");
            }
            Ok(vec![
                PageRef { url: "a".into() },
                PageRef { url: "b".into() },
            ])
        }

        async fn fetch_page(&self, _page: &PageRef) -> anyhow::Result<Vec<u8>> {
            Ok(test_images::jpeg_rgb())
        }
    }

    fn ctx(sources: Vec<Arc<dyn Source>>) -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&Paths::under(dir.path())).unwrap();
        let ctx = Ctx {
            sources,
            db: Db::open_in_memory().unwrap(),
            config,
        };
        (dir, ctx)
    }

    fn calls(source: &Fake) -> Vec<ChapterNo> {
        source.pages_calls.lock().unwrap().clone()
    }

    #[test]
    fn updated_text_in_days() {
        let now: jiff::Timestamp = "2026-11-01T12:00:00Z".parse().unwrap();
        let ago = |h: i64| now - jiff::SignedDuration::from_hours(h);
        assert_eq!(updated_text(ago(1), now), "updated today");
        assert_eq!(updated_text(ago(-5), now), "updated today");
        assert_eq!(updated_text(ago(49), now), "updated 2d ago");
    }

    #[test]
    fn failures_printed_once_each() {
        let failures = [
            (SourceId::MangaKatana, "boom".to_string()),
            (SourceId::MangaDex, "slow".to_string()),
            (SourceId::MangaKatana, "boom".to_string()),
        ];
        assert_eq!(
            distinct_failures(&failures),
            ["MangaKatana failed: boom", "MangaDex failed: slow"]
        );
    }

    #[tokio::test]
    async fn search_prints_grouped_lines() {
        let mut md_chapters = nums(1, 44);
        md_chapters.extend(nums(67, 200));
        let mut mk = Fake::new(SourceId::MangaKatana, &["Solo Leveling"], nums(1, 201));
        mk.published = Some(jiff::Timestamp::now() - jiff::SignedDuration::from_hours(50));
        let (_dir, ctx) = ctx(vec![
            Arc::new(Fake::new(
                SourceId::MangaDex,
                &["Solo Leveling"],
                md_chapters,
            )),
            Arc::new(mk),
        ]);
        let mut out = Vec::new();
        search(&ctx, "solo leveling", &mut out).await.unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "Solo Leveling\n\
             \x20 MangaKatana   201 ch   latest 201   updated 2d ago\n\
             \x20 MangaDex      178 ch   latest 200   gaps: 45–66\n"
        );
    }

    #[tokio::test]
    async fn search_all_failed_is_error() {
        let (_dir, ctx) = ctx(vec![
            Arc::new(Fake::failing(SourceId::MangaDex)),
            Arc::new(Fake::failing(SourceId::MangaKatana)),
        ]);
        let err = search(&ctx, "x", &mut Vec::new()).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("every source failed"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn search_without_results_is_error() {
        let (_dir, ctx) = ctx(vec![
            Arc::new(Fake::new(SourceId::MangaDex, &[], Vec::new())),
            Arc::new(Fake::failing(SourceId::MangaKatana)),
        ]);
        let err = search(&ctx, "x", &mut Vec::new()).await.unwrap_err();
        assert_eq!(format!("{err:#}"), "no results for \"x\"");
    }

    #[tokio::test]
    async fn download_ambiguous_lists_candidates() {
        let (_dir, ctx) = ctx(vec![
            Arc::new(Fake::new(
                SourceId::MangaDex,
                &["Solo Leveling", "Solo Leveling"],
                nums(1, 3),
            )),
            Arc::new(Fake::new(SourceId::MangaKatana, &[], Vec::new())),
        ]);
        let range = parse_range("1").unwrap();
        let err = download_to(
            &ctx,
            "solo leveling",
            &range,
            None,
            None,
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "\"solo leveling\" matches several titles:\n  \
             Solo Leveling (MangaDex)\n  \
             Solo Leveling (MangaDex)"
        );
    }

    #[tokio::test]
    async fn download_source_not_in_group_names_available() {
        let (_dir, ctx) = ctx(vec![
            Arc::new(Fake::new(
                SourceId::MangaDex,
                &["Solo Leveling"],
                nums(1, 3),
            )),
            Arc::new(Fake::new(SourceId::MangaKatana, &[], Vec::new())),
        ]);
        let range = parse_range("1").unwrap();
        let err = download_to(
            &ctx,
            "solo leveling",
            &range,
            Some(SourceId::MangaKatana),
            None,
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("isn't on MangaKatana"), "{err}");
        assert!(err.contains("MangaDex"), "{err}");
    }

    #[tokio::test]
    async fn download_skips_existing_and_hints_missing() {
        let md = Arc::new(Fake::new(
            SourceId::MangaDex,
            &["Solo Leveling"],
            vec!["1".into(), "2".into(), "4".into()],
        ));
        let mk = Arc::new(Fake::new(
            SourceId::MangaKatana,
            &["Solo Leveling"],
            nums(1, 3),
        ));
        let (_dir, ctx) = ctx(vec![md.clone(), mk.clone()]);
        let dl = ctx.config.download_dir.clone();
        let existing = chapter_path(&dl, "Solo Leveling", no("2"), Format::Cbz);
        write_chapter(&[test_images::jpeg_rgb()], Format::Cbz, &existing).unwrap();

        let range = parse_range("1-4").unwrap();
        let mut out = Vec::new();
        download_to(&ctx, "solo leveling", &range, None, None, None, &mut out)
            .await
            .unwrap();
        let out = String::from_utf8(out).unwrap();

        assert_eq!(calls(&md), [no("1"), no("4")]);
        assert!(calls(&mk).is_empty());
        for ch in ["1", "4"] {
            let path = chapter_path(&dl, "Solo Leveling", no(ch), Format::Pdf);
            assert!(path.is_file(), "{}", path.display());
            assert!(
                out.contains(&format!("Ch {ch} (2 pages) → {}\n", path.display())),
                "{out}"
            );
        }
        assert!(
            out.contains("hint: hon download \"Solo Leveling\" 3 --source mangakatana\n"),
            "{out}"
        );
        let series = ctx
            .db
            .series(&["solo leveling".into()], "Solo Leveling")
            .unwrap()
            .id;
        assert!(ctx.db.progress(series).unwrap().is_empty());
    }

    #[tokio::test]
    async fn series_found_again_when_sources_title_it_differently() {
        let md = Fake {
            alt_titles: vec!["Solo Leveling"],
            ..Fake::new(SourceId::MangaDex, &["Na Honjaman Level-Up"], nums(1, 3))
        };
        let mk = Arc::new(Fake::new(
            SourceId::MangaKatana,
            &["Solo Leveling"],
            nums(1, 3),
        ));
        let (_dir, mut ctx) = ctx(vec![Arc::new(md), mk.clone()]);
        let range = parse_range("1").unwrap();
        download_to(
            &ctx,
            "solo leveling",
            &range,
            None,
            None,
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        let dl = ctx.config.download_dir.clone();
        assert!(chapter_path(&dl, "Na Honjaman Level-Up", no("1"), Format::Pdf).is_file());
        let first = ctx
            .db
            .series(&["na honjaman level up".into()], "unused")
            .unwrap();
        ctx.db.set_read(first.id, no("1"), true).unwrap();

        // MangaDex is down: the group is MangaKatana's "Solo Leveling" alone.
        ctx.sources = vec![Arc::new(Fake::failing(SourceId::MangaDex)), mk.clone()];
        let mut out = Vec::new();
        download_to(&ctx, "solo leveling", &range, None, None, None, &mut out)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "Ch 1 already downloaded\n");
        assert!(calls(&mk).is_empty());
        let again = ctx
            .db
            .series(&["solo leveling".into()], "Solo Leveling")
            .unwrap();
        assert_eq!(again, first);
        assert_eq!(again.title, "Na Honjaman Level-Up");
        let start = read_start(&ctx, "solo leveling", None, None).await.unwrap();
        assert!(
            matches!(start, Start::Read { chapter: Some(c), .. } if c == no("2")),
            "Ch 1 is still read"
        );
    }

    #[tokio::test]
    async fn download_asks_before_using_other_source() {
        let md = Arc::new(Fake::new(
            SourceId::MangaDex,
            &["Solo Leveling"],
            vec!["1".into(), "5".into()],
        ));
        let mk = Arc::new(Fake::new(
            SourceId::MangaKatana,
            &["Solo Leveling"],
            nums(1, 4),
        ));
        let (_dir, ctx) = ctx(vec![md.clone(), mk.clone()]);
        let range = parse_range("1-5").unwrap();
        let mut questions = Vec::new();
        let mut ask = |q: &str| {
            questions.push(q.to_string());
            Ok(true)
        };
        download_to(
            &ctx,
            "solo leveling",
            &range,
            None,
            Some(Format::Cbz),
            Some(&mut ask),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            questions,
            ["Ch 2–4 aren't on MangaDex. Download them from MangaKatana? [y/N] "]
        );
        assert_eq!(calls(&md), [no("1"), no("5")]);
        assert_eq!(calls(&mk), [no("2"), no("3"), no("4")]);
        let dl = &ctx.config.download_dir;
        assert!(chapter_path(dl, "Solo Leveling", no("3"), Format::Cbz).is_file());
    }

    fn progress_of(entries: &[(&str, bool, Option<u32>)]) -> HashMap<ChapterNo, Progress> {
        entries
            .iter()
            .map(|&(ch, read, last_page)| {
                let p = Progress {
                    read,
                    last_page,
                    page_count: None,
                    source: Some(SourceId::MangaDex),
                };
                (no(ch), p)
            })
            .collect()
    }

    #[tokio::test]
    async fn start_chapter_in_progress_then_unread_then_last() {
        let (_dir, ctx) = ctx(vec![
            Arc::new(Fake::new(
                SourceId::MangaDex,
                &["X"],
                vec!["1".into(), "3".into()],
            )),
            Arc::new(Fake::new(SourceId::MangaKatana, &["X"], nums(1, 4))),
        ]);
        let outcome = search_all(&ctx.sources, "x").await;
        let group = &outcome.groups[0];
        let start = |entries| start_chapter(group, &progress_of(entries));
        assert_eq!(start(&[]), Some(no("1")));
        assert_eq!(
            start(&[("1", true, None), ("2", true, None)]),
            Some(no("3"))
        );
        assert_eq!(
            start(&[
                ("1", true, None),
                ("3", false, Some(4)),
                ("4", false, Some(0))
            ]),
            Some(no("3")),
            "first in-progress beats first unread"
        );
        assert_eq!(
            start(&[
                ("1", true, None),
                ("2", true, None),
                ("3", true, None),
                ("4", true, None)
            ]),
            Some(no("4")),
            "all read: the last one"
        );
    }

    #[tokio::test]
    async fn read_command_picks_first_unread() {
        let (_dir, ctx) = ctx(vec![
            Arc::new(Fake::new(
                SourceId::MangaDex,
                &["Solo Leveling"],
                nums(1, 3),
            )),
            Arc::new(Fake::new(
                SourceId::MangaKatana,
                &["Solo Leveling"],
                nums(1, 5),
            )),
        ]);
        let series = ctx
            .db
            .series(&["solo leveling".into()], "Solo Leveling")
            .unwrap()
            .id;
        ctx.db.set_read(series, no("1"), true).unwrap();
        ctx.db.set_read(series, no("2"), true).unwrap();

        let start = read_start(&ctx, "solo leveling", None, None).await.unwrap();
        let Start::Read {
            group,
            source,
            chapter,
        } = start
        else {
            panic!("not a read start");
        };
        assert_eq!(group.title, "Solo Leveling");
        assert_eq!(
            source,
            SourceId::MangaKatana,
            "the one with the newest chapter"
        );
        assert_eq!(chapter, Some(no("3")));

        let chapter_of = |start| match start {
            Start::Read {
                chapter, source, ..
            } => (chapter, source),
            Start::Search => panic!("not a read start"),
        };
        let start = read_start(&ctx, "solo leveling", Some("5"), Some(SourceId::MangaDex));
        assert_eq!(
            chapter_of(start.await.unwrap()),
            (Some(no("5")), SourceId::MangaDex),
            "the reader offers MangaKatana for it"
        );

        let err = read_start(&ctx, "solo leveling", Some("9"), None).await;
        assert_eq!(
            format!("{:#}", err.err().unwrap()),
            "\"Solo Leveling\" has no Ch 9"
        );
        let err = read_start(&ctx, "solo leveling", Some("x"), None).await;
        assert_eq!(
            format!("{:#}", err.err().unwrap()),
            "\"x\" isn't a chapter number"
        );
    }

    #[tokio::test]
    async fn download_continues_after_failed_chapter() {
        let mut md = Fake::new(SourceId::MangaDex, &["Solo Leveling"], nums(1, 3));
        md.failing_pages = vec![no("2")];
        let md = Arc::new(md);
        let (_dir, ctx) = ctx(vec![
            md.clone(),
            Arc::new(Fake::new(SourceId::MangaKatana, &[], Vec::new())),
        ]);
        let range = parse_range("1-3").unwrap();
        let err = download_to(
            &ctx,
            "solo leveling",
            &range,
            None,
            None,
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(calls(&md), [no("1"), no("2"), no("3")]);
        assert!(format!("{err:#}").contains("1 chapter failed"), "{err:#}");
        let dl = &ctx.config.download_dir;
        assert!(chapter_path(dl, "Solo Leveling", no("3"), Format::Pdf).is_file());
        assert!(!chapter_path(dl, "Solo Leveling", no("2"), Format::Pdf).exists());
    }
}
