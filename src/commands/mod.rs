use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};

use crate::cli::ChapterRange;
use crate::config::{Config, Format, Reader};
use crate::db::{Db, Progress, Series, resume_chapter};
use crate::download::{chapter_of, chapter_path, existing_download, fetch_pages, write_chapter};
use crate::model::{ChapterNo, ChapterRef, SourceId};
use crate::search::{
    SearchOutcome, SourceSummary, TitleGroup, missing_elsewhere, resolve, search_all,
};
use crate::sources::Source;
use crate::tui::{self, Start};
use crate::viewer::{open_chapter, opencomic};

const SHOWN_GAPS: usize = 2;

pub struct Ctx {
    pub sources: Vec<Arc<dyn Source>>,
    pub db: Db,
    pub config: Config,
}

type Ask<'a> = &'a mut dyn FnMut(&str) -> anyhow::Result<bool>;

/// What `hon download` does for a person at a terminal.
#[derive(Default)]
struct Interaction<'a> {
    /// Asks before using another source; without it, hints are printed instead.
    ask: Option<Ask<'a>>,
    /// Shows `Ch N  12/38` while a chapter downloads.
    live: bool,
}

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
    let mut ask = ask_on_terminal;
    let term = Interaction {
        ask: if interactive { Some(&mut ask) } else { None },
        live: io::stdout().is_terminal(),
    };
    download_to(ctx, title, range, source, format, term, &mut io::stdout()).await
}

/// Asks `question` on stdout; true when the answer on stdin is y or yes.
fn ask_on_terminal(question: &str) -> anyhow::Result<bool> {
    let mut stdout = io::stdout();
    write!(stdout, "{question}")?;
    stdout.flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

async fn download_to(
    ctx: &Ctx,
    title: &str,
    range: &ChapterRange,
    source: Option<SourceId>,
    format: Option<Format>,
    mut term: Interaction<'_>,
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
    let live = term.live;
    let mut failed = fetch_chapters(ctx, &folder, on, &chapters, fmt, live, out).await?;

    let dl = &ctx.config.download_dir;
    let mut wanted: Vec<ChapterNo> = group
        .sources
        .iter()
        .flat_map(|s| s.chapters.iter().filter(in_range))
        .map(|c| c.number)
        .filter(|&n| existing_download(dl, &folder, n, fmt).is_none())
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
        let Some(ask) = term.ask.as_mut() else {
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
            failed += fetch_chapters(ctx, &folder, other_id, &chapters, fmt, live, out).await?;
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
    let group = resolve(title, groups, source).map_err(|_| ambiguous(title, groups))?;
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
    if ctx.config.reader == Reader::Viewer {
        let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
        let mut ask = ask_on_terminal;
        let ask: Option<Ask> = if interactive { Some(&mut ask) } else { None };
        let viewer = ctx.config.viewer.as_deref();
        let storage = ctx.config.opencomic.as_deref();
        let mut open = |path: &Path| open_chapter(path, viewer, storage);
        let out = &mut io::stdout();
        let opened = read_in_viewer(&ctx, title, chapter, source, ask, &mut open, out).await?;
        let Some(opened) = opened.filter(|_| opencomic::is_opencomic(viewer) && interactive) else {
            return Ok(());
        };
        let Some(storage) = &ctx.config.opencomic else {
            writeln!(
                out,
                "can't find OpenComic's progress, so hon won't follow along this time"
            )?;
            return Ok(());
        };
        return follow(&ctx, &opened, storage, out).await;
    }
    tui::ensure_terminal()?;
    let start = read_start(&ctx, title, chapter, source).await?;
    tui::run(ctx, start).await
}

/// A chapter `hon read` handed to the viewer.
#[derive(Debug)]
struct Opened {
    group: TitleGroup,
    /// The session's source: where chapters ahead come from.
    on: SourceId,
    series: Series,
    chapter: ChapterNo,
    /// When it was opened, in Unix milliseconds.
    opened_at_ms: i64,
    /// The note about chapters ahead already printed when it was opened.
    note: Option<String>,
}

/// `hon read` with `reader = "viewer"`: downloads the chapter unless it's on disk, opens it
/// with `open` and marks it read, since hon can't follow the pages in another app. A
/// chapter the source lacks comes from another source only if `ask` says yes.
///
/// For OpenComic, the chapter opens only once the next chapter is on disk too: OpenComic
/// only goes on to a next chapter that existed when the current one opened. Nothing is
/// marked read; OpenComic's own progress says what was read. `None` when the user
/// declined the other source.
async fn read_in_viewer(
    ctx: &Ctx,
    title: &str,
    chapter: Option<&str>,
    source: Option<SourceId>,
    ask: Option<Ask<'_>>,
    open: &mut dyn FnMut(&Path) -> anyhow::Result<()>,
    out: &mut impl Write,
) -> anyhow::Result<Option<Opened>> {
    let Start::Read {
        group,
        source: on,
        chapter: Some(n),
    } = read_start(ctx, title, chapter, source).await?
    else {
        unreachable!("read_start always picks a chapter");
    };
    let on_source = |id: SourceId| {
        group
            .source(id)
            .and_then(|s| s.chapters.iter().find(|c| c.number == n))
    };
    let ch = match on_source(on) {
        Some(ch) => ch,
        None => {
            let (_, other) = *missing_elsewhere(&group, on, &[n])
                .first()
                .expect("read_start checks the chapter exists");
            let Some(ask) = ask else {
                bail!(
                    "Ch {n} isn't on {}; it's on {} (use --source {})",
                    on.name(),
                    other.name(),
                    other.slug()
                );
            };
            let question = format!(
                "Ch {n} isn't on {}. Open it from {}? [y/N] ",
                on.name(),
                other.name()
            );
            if !ask(&question)? {
                return Ok(None);
            }
            on_source(other).expect("missing_elsewhere found it there")
        }
    };
    let series = ctx.db.series(&group.keys, &group.title)?;
    let (dl, fmt) = (&ctx.config.download_dir, ctx.config.format);
    let path = match existing_download(dl, &series.title, n, fmt) {
        Some(path) => path,
        None => {
            let src = ctx
                .sources
                .iter()
                .find(|s| s.id() == ch.source)
                .ok_or_else(|| anyhow!("{} isn't registered", ch.source.name()))?;
            let path = chapter_path(dl, &series.title, n, fmt);
            fetch_pages(src.as_ref(), ch, |_, _| {})
                .await
                .and_then(|pages| write_chapter(&pages, fmt, &path))
                .with_context(|| format!("Ch {n}"))?;
            path
        }
    };
    let mut note = None;
    if !opencomic::is_opencomic(ctx.config.viewer.as_deref()) {
        open(&path)?;
        ctx.db.set_read(series.id, n, true)?;
        writeln!(out, "Ch {n} → {}", path.display())?;
    } else {
        let next = ahead(&group, on, n, 1);
        fetch_missing(ctx, &series.title, on, &next.fetch, out).await?;
        if let Some(text) = &next.note {
            writeln!(out, "{text}")?;
        }
        note = next.note;
        open(&path)?;
        let config = opencomic::read_config(ctx.config.opencomic.as_deref());
        if config.is_some_and(|json| opencomic::opens_chapters(&json)) {
            writeln!(out, "Ch {n} → {}", path.display())?;
        } else {
            writeln!(out, "Ch {n} is ready — pick it in OpenComic")?;
        }
        if half_chapters_misordered(ctx, &group) {
            writeln!(
                out,
                "tip: set OpenComic's sort to \"Numeric\" so half chapters (9.5) come after \
                 their chapter (9)"
            )?;
        }
    }
    Ok(Some(Opened {
        on,
        series,
        chapter: n,
        group,
        opened_at_ms: jiff::Timestamp::now().as_millisecond(),
        note,
    }))
}

/// How often `follow` checks OpenComic's progress file.
const FOLLOW_INTERVAL: Duration = Duration::from_secs(2);
/// Chapters kept on disk after the one OpenComic has open.
const FOLLOW_AHEAD: usize = 2;

/// Follows OpenComic until Ctrl-C: whenever its progress file changes, takes in what was
/// read and keeps the next chapters downloaded.
async fn follow(
    ctx: &Ctx,
    opened: &Opened,
    storage: &Path,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    writeln!(out, "following OpenComic · Ctrl-C to stop")?;
    out.flush()?;
    let file = storage.join(opencomic::PROGRESS_FILE);
    let mut state = Following::new(opened);
    // The progress file's mtime at the last good look; `None` until there is one.
    let mut seen: Option<Option<std::time::SystemTime>> = None;
    let mut last_warning = None;
    loop {
        let modified = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
        if seen != Some(modified) {
            match follow_step(ctx, opened, storage, &mut state, out).await {
                // OpenComic may be halfway through writing it; try again next time.
                Err(e) => {
                    let warning = format!("{e:#}");
                    if last_warning.as_ref() != Some(&warning) {
                        eprintln!("warning: {warning}");
                    }
                    last_warning = Some(warning);
                    seen = None;
                }
                Ok(()) => {
                    last_warning = None;
                    seen = Some(modified);
                }
            }
            out.flush()?;
        }
        tokio::time::sleep(FOLLOW_INTERVAL).await;
    }
}

/// What `follow` has seen so far.
struct Following {
    /// The chapter OpenComic has open, as far as hon can tell.
    current: ChapterNo,
    /// The last note about chapters ahead, so it's printed once.
    note: Option<String>,
}

impl Following {
    fn new(opened: &Opened) -> Following {
        Following {
            current: opened.chapter,
            note: opened.note.clone(),
        }
    }
}

/// One look at OpenComic's progress: takes in what was read, works out the chapter
/// that's open now (ignoring progress from before `hon read` opened it) and downloads
/// the next `FOLLOW_AHEAD` chapters.
async fn follow_step(
    ctx: &Ctx,
    opened: &Opened,
    storage: &Path,
    state: &mut Following,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let dl = &ctx.config.download_dir;
    let entries = import_opencomic(&ctx.db, storage, dl, &opened.series, opened.on)?;
    let recent: Vec<opencomic::Entry> = entries
        .into_iter()
        .filter(|e| e.last_reading_ms >= opened.opened_at_ms)
        .collect();
    let chapters = all_chapters(&opened.group);
    let current = opencomic::current_chapter(&recent, &chapters).unwrap_or(state.current);
    if current != state.current {
        writeln!(out, "reading Ch {current}")?;
        state.current = current;
    }
    let next = ahead(&opened.group, opened.on, current, FOLLOW_AHEAD);
    fetch_missing(ctx, &opened.series.title, opened.on, &next.fetch, out).await?;
    if next.note != state.note {
        if let Some(note) = &next.note {
            writeln!(out, "{note}")?;
        }
        state.note = next.note;
    }
    Ok(())
}

/// Whether the series has half chapters that OpenComic's chapter order puts first.
fn half_chapters_misordered(ctx: &Ctx, group: &TitleGroup) -> bool {
    let has_half = group
        .sources
        .iter()
        .flat_map(|s| &s.chapters)
        .any(|c| c.number.value().fract() != 0.0);
    let config = opencomic::read_config(ctx.config.opencomic.as_deref());
    has_half && config.is_some_and(|json| opencomic::sorts_half_chapters_wrong(&json))
}

/// The chapters to have on disk after the one being read.
struct Ahead<'a> {
    /// The next ones, all on the session's source.
    fetch: Vec<&'a ChapterRef>,
    /// Why there are fewer than asked for, if there are.
    note: Option<String>,
}

/// The next `count` chapters after `current` on any source, as long as `on` has them:
/// downloading ahead stops at the first one only another source has.
fn ahead(group: &TitleGroup, on: SourceId, current: ChapterNo, count: usize) -> Ahead<'_> {
    let mut later: Vec<ChapterNo> = group
        .sources
        .iter()
        .flat_map(|s| s.chapters.iter().map(|c| c.number))
        .filter(|&n| n > current)
        .collect();
    later.sort();
    later.dedup();
    if later.is_empty() {
        let note = format!("no chapters after Ch {current} yet");
        return Ahead {
            fetch: Vec::new(),
            note: Some(note),
        };
    }
    let mut fetch = Vec::new();
    for n in later.into_iter().take(count) {
        let on_source = group
            .source(on)
            .and_then(|s| s.chapters.iter().find(|c| c.number == n));
        let Some(ch) = on_source else {
            let (_, other) = *missing_elsewhere(group, on, &[n])
                .first()
                .expect("another source has it");
            let note = format!(
                "Ch {n} isn't on {}; it's on {} (hon download \"{}\" {n} --source {})",
                on.name(),
                other.name(),
                group.title,
                other.slug()
            );
            return Ahead {
                fetch,
                note: Some(note),
            };
        };
        fetch.push(ch);
    }
    Ahead { fetch, note: None }
}

/// Downloads the ones of `chapters` that aren't on disk yet; returns how many failed.
async fn fetch_missing(
    ctx: &Ctx,
    folder: &str,
    source: SourceId,
    chapters: &[&ChapterRef],
    out: &mut impl Write,
) -> anyhow::Result<usize> {
    let (dl, fmt) = (&ctx.config.download_dir, ctx.config.format);
    let missing: Vec<&ChapterRef> = chapters
        .iter()
        .copied()
        .filter(|c| existing_download(dl, folder, c.number, fmt).is_none())
        .collect();
    fetch_chapters(ctx, folder, source, &missing, fmt, false, out).await
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
            if let Some(storage) = &ctx.config.opencomic {
                let dl = &ctx.config.download_dir;
                if let Err(e) = import_opencomic(&ctx.db, storage, dl, &series, on) {
                    eprintln!("warning: {e:#}");
                }
            }
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

/// The resume chapter among every source's chapters.
fn start_chapter(group: &TitleGroup, progress: &HashMap<ChapterNo, Progress>) -> Option<ChapterNo> {
    resume_chapter(&all_chapters(group), progress)
}

/// Every source's chapter numbers, ascending and distinct.
fn all_chapters(group: &TitleGroup) -> Vec<ChapterNo> {
    let mut numbers: Vec<ChapterNo> = group
        .sources
        .iter()
        .flat_map(|s| s.chapters.iter().map(|c| c.number))
        .collect();
    numbers.sort();
    numbers.dedup();
    numbers
}

/// Applies OpenComic's progress for `series` that is newer than hon's own: a finished
/// chapter is marked read, any other saves its page (on `source`, the one hon resumes
/// from). Returns all of the series' entries; none while OpenComic has saved nothing.
pub(crate) fn import_opencomic(
    db: &Db,
    storage: &Path,
    download_dir: &Path,
    series: &Series,
    source: SourceId,
) -> anyhow::Result<Vec<opencomic::Entry>> {
    let json = match std::fs::read_to_string(storage.join(opencomic::PROGRESS_FILE)) {
        Ok(json) => json,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("reading OpenComic's progress"),
    };
    let mut entries =
        opencomic::entries(&json, |path| chapter_of(download_dir, &series.title, path))?;
    entries.sort_by_key(|e| e.last_reading_ms);
    let known = db.progress(series.id)?;
    for e in &entries {
        if known
            .get(&e.chapter)
            .is_some_and(|p| p.updated_at * 1000 >= e.last_reading_ms)
        {
            continue;
        }
        if e.completed {
            db.set_read(series.id, e.chapter, true)?;
        } else {
            db.save_page(
                series.id,
                e.chapter,
                source,
                e.page.saturating_sub(1),
                e.pages,
            )?;
        }
    }
    Ok(entries)
}

fn ambiguous(query: &str, groups: &[TitleGroup]) -> anyhow::Error {
    let mut msg = format!("\"{query}\" matches several titles:");
    for (i, g) in groups.iter().enumerate() {
        let names: Vec<_> = g.sources.iter().map(|s| s.manga.source.name()).collect();
        msg += &format!("\n  {} ({})", g.title, names.join(", "));
        // Titles alike tell nothing apart; a source only this one of them has does.
        let others: Vec<&TitleGroup> = groups
            .iter()
            .enumerate()
            .filter(|&(j, other)| j != i && other.title == g.title)
            .map(|(_, other)| other)
            .collect();
        if others.is_empty() {
            continue;
        }
        let own = g
            .sources
            .iter()
            .map(|s| s.manga.source)
            .find(|&id| others.iter().all(|o| o.source(id).is_none()));
        if let Some(id) = own {
            msg += &format!("   --source {}", id.slug());
        }
    }
    anyhow!(msg)
}

/// Downloads `chapters` from `source` into `folder`'s directory, skipping ones already on
/// disk. A failed chapter is reported and skipped; returns how many failed. `live` shows
/// `\rCh N  12/38` while each chapter downloads.
async fn fetch_chapters(
    ctx: &Ctx,
    folder: &str,
    source: SourceId,
    chapters: &[&ChapterRef],
    fmt: Format,
    live: bool,
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
        if existing_download(dl, folder, ch.number, fmt).is_some() {
            writeln!(out, "Ch {} already downloaded", ch.number)?;
            continue;
        }
        let path = chapter_path(dl, folder, ch.number, fmt);
        let shown = Cell::new(0);
        let progress = RefCell::new(&mut *out);
        let pages = fetch_pages(src.as_ref(), ch, |done, total| {
            if live {
                let line = format!("Ch {}  {done}/{total}", ch.number);
                let mut out = progress.borrow_mut();
                let _ = write!(out, "\r{line}").and_then(|()| out.flush());
                shown.set(line.chars().count());
            }
        })
        .await;
        let saved = pages.and_then(|pages| write_chapter(&pages, fmt, &path).map(|()| pages.len()));
        if shown.get() > 0 {
            write!(out, "\r{:1$}\r", "", shown.get())?;
            out.flush()?;
        }
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
    use crate::db;
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
            Interaction::default(),
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

    #[test]
    fn same_title_twice_says_which_source_picks_which() {
        let summary = |source: SourceId, id: &str| SourceSummary {
            manga: MangaRef {
                source,
                id: id.into(),
                title: "Kaoru".into(),
                alt_titles: Vec::new(),
                url: String::new(),
            },
            chapters: Vec::new(),
            latest: None,
            gaps: Vec::new(),
            updated: None,
        };
        let group = |sources| TitleGroup {
            keys: vec!["kaoru".into()],
            title: "Kaoru".into(),
            sources,
        };
        let groups = [
            group(vec![
                summary(SourceId::WeebCentral, "wc"),
                summary(SourceId::MangaDex, "a"),
            ]),
            group(vec![
                summary(SourceId::MangaKatana, "mk"),
                summary(SourceId::MangaDex, "b"),
            ]),
        ];
        assert_eq!(
            format!("{:#}", ambiguous("kaoru", &groups)),
            "\"kaoru\" matches several titles:\n  \
             Kaoru (WeebCentral, MangaDex)   --source weebcentral\n  \
             Kaoru (MangaKatana, MangaDex)   --source mangakatana"
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
            Interaction::default(),
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
        download_to(
            &ctx,
            "solo leveling",
            &range,
            None,
            None,
            Interaction::default(),
            &mut out,
        )
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
            Interaction::default(),
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
        download_to(
            &ctx,
            "solo leveling",
            &range,
            None,
            None,
            Interaction::default(),
            &mut out,
        )
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
            Interaction {
                ask: Some(&mut ask),
                live: false,
            },
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

    /// `(chapter, read, updated_at)`; unread ones are in progress at page 4.
    fn progress_of(entries: &[(&str, bool, i64)]) -> HashMap<ChapterNo, Progress> {
        entries
            .iter()
            .map(|&(ch, read, updated_at)| {
                let p = Progress {
                    read,
                    last_page: (!read).then_some(4),
                    page_count: None,
                    source: Some(SourceId::MangaDex),
                    updated_at,
                };
                (no(ch), p)
            })
            .collect()
    }

    fn read_at(chapters: impl IntoIterator<Item = u32>, t: i64) -> Vec<(String, bool, i64)> {
        chapters
            .into_iter()
            .map(|v| (v.to_string(), true, t))
            .collect()
    }

    #[tokio::test]
    async fn start_chapter_resumes_where_reading_stopped() {
        let (_dir, ctx) = ctx(vec![
            Arc::new(Fake::new(
                SourceId::MangaDex,
                &["X"],
                vec!["0".into(), "1".into(), "3".into()],
            )),
            Arc::new(Fake::new(SourceId::MangaKatana, &["X"], nums(1, 130))),
        ]);
        let outcome = search_all(&ctx.sources, "x").await;
        let group = &outcome.groups[0];
        let start = |entries: &[(String, bool, i64)]| {
            let entries: Vec<(&str, bool, i64)> = entries
                .iter()
                .map(|(c, r, t)| (c.as_str(), *r, *t))
                .collect();
            start_chapter(group, &progress_of(&entries))
        };
        assert_eq!(start(&[]), Some(no("0")), "nothing read: the first");

        let mut stray = read_at((1..=120).filter(|&v| v != 3), 10);
        stray.push(("3".into(), false, 1));
        assert_eq!(start(&stray), Some(no("121")), "not the stray page of Ch 3");

        let started_late = [("50".to_string(), false, 5)];
        assert_eq!(start(&started_late), Some(no("50")), "not Ch 0 or Ch 1");

        let oneshot_unread = read_at(1..=5, 10);
        assert_eq!(
            start(&oneshot_unread),
            Some(no("6")),
            "not the oneshot Ch 0"
        );

        let next_elsewhere = read_at([1], 10);
        assert_eq!(
            start(&next_elsewhere),
            Some(no("2")),
            "on any source: MangaKatana has it"
        );
        assert_eq!(start(&read_at([130], 10)), Some(no("130")), "all read");
    }

    #[tokio::test]
    async fn read_command_resumes_where_left_off() {
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

    /// Writes OpenComic's progress for "Solo Leveling" chapters into `storage`:
    /// `(chapter, page, pages, completed, lastReading ms)`.
    fn write_progress(ctx: &Ctx, storage: &Path, rows: &[(&str, u32, u32, bool, i64)]) {
        let mut all = serde_json::Map::new();
        for &(n, page, pages, completed, at) in rows {
            let file = chapter_path(
                &ctx.config.download_dir,
                "Solo Leveling",
                no(n),
                Format::Cbz,
            );
            all.insert(
                file.display().to_string(),
                serde_json::json!({
                    "page": page, "pages": pages, "completed": completed, "lastReading": at
                }),
            );
        }
        std::fs::create_dir_all(storage).unwrap();
        std::fs::write(
            storage.join(opencomic::PROGRESS_FILE),
            serde_json::Value::Object(all).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn import_marks_completed_and_saves_pages() {
        let (dir, ctx) = ctx(Vec::new());
        let storage = dir.path().join("opencomic");
        let series = ctx
            .db
            .series(&["solo leveling".into()], "Solo Leveling")
            .unwrap();
        let later = (db::now() + 60) * 1000;
        write_progress(
            &ctx,
            &storage,
            &[("1", 24, 24, true, later), ("2", 5, 19, false, later + 1)],
        );

        let entries = import_opencomic(
            &ctx.db,
            &storage,
            &ctx.config.download_dir,
            &series,
            SourceId::MangaKatana,
        )
        .unwrap();

        assert_eq!(entries.len(), 2);
        let progress = ctx.db.progress(series.id).unwrap();
        assert!(progress[&no("1")].read);
        let two = &progress[&no("2")];
        assert!(!two.read);
        assert_eq!(
            two.last_page,
            Some(4),
            "OpenComic's page 5 is hon's 0-based 4"
        );
        assert_eq!(two.page_count, Some(19));
        assert_eq!(two.source, Some(SourceId::MangaKatana));
    }

    #[test]
    fn import_skips_entries_older_than_hon() {
        let (dir, ctx) = ctx(Vec::new());
        let storage = dir.path().join("opencomic");
        let series = ctx
            .db
            .series(&["solo leveling".into()], "Solo Leveling")
            .unwrap();
        let import = || {
            import_opencomic(
                &ctx.db,
                &storage,
                &ctx.config.download_dir,
                &series,
                SourceId::MangaKatana,
            )
        };
        assert_eq!(import().unwrap(), [], "no progress file yet");

        ctx.db.set_read(series.id, no("2"), false).unwrap();
        write_progress(&ctx, &storage, &[("2", 5, 19, true, 1000)]);
        import().unwrap();
        let two = &ctx.db.progress(series.id).unwrap()[&no("2")];
        assert!(!two.read, "hon's own change is newer");
        assert_eq!(two.last_page, None);
    }

    fn numbers(chapters: &[&ChapterRef]) -> Vec<ChapterNo> {
        chapters.iter().map(|c| c.number).collect()
    }

    #[tokio::test]
    async fn ahead_stops_at_gap_and_end() {
        let mk = Fake::new(SourceId::MangaKatana, &["Solo Leveling"], nums(1, 2));
        let mk = Fake {
            chapters: vec!["1".into(), "2".into(), "4".into()],
            ..mk
        };
        let md = Fake::new(SourceId::MangaDex, &["Solo Leveling"], vec!["3".into()]);
        let (_dir, ctx) = ctx(vec![Arc::new(mk), Arc::new(md)]);
        let outcome = search_all(&ctx.sources, "solo leveling").await;
        let group = &outcome.groups[0];

        let two = ahead(group, SourceId::MangaKatana, no("1"), 2);
        assert_eq!(numbers(&two.fetch), [no("2")]);
        assert_eq!(
            two.note.as_deref(),
            Some(
                "Ch 3 isn't on MangaKatana; it's on MangaDex \
                 (hon download \"Solo Leveling\" 3 --source mangadex)"
            )
        );
        let end = ahead(group, SourceId::MangaKatana, no("4"), 2);
        assert!(end.fetch.is_empty());
        assert_eq!(end.note.as_deref(), Some("no chapters after Ch 4 yet"));
        let both = ahead(group, SourceId::MangaDex, no("1"), 1);
        assert!(both.fetch.is_empty(), "Ch 2 is only on MangaKatana");
    }

    fn opencomic_ctx(sources: Vec<Arc<dyn Source>>) -> (tempfile::TempDir, Ctx) {
        let (dir, mut ctx) = ctx(sources);
        ctx.config.viewer = Some("flatpak run app.opencomic.OpenComic".into());
        let storage = dir.path().join("opencomic");
        std::fs::create_dir_all(&storage).unwrap();
        ctx.config.opencomic = Some(storage);
        (dir, ctx)
    }

    #[tokio::test]
    async fn opencomic_gets_the_chapter_once_the_next_one_is_ready() {
        let mk = Arc::new(Fake::new(
            SourceId::MangaKatana,
            &["Solo Leveling"],
            nums(1, 5),
        ));
        let (_dir, ctx) = opencomic_ctx(vec![mk.clone()]);
        let dl = &ctx.config.download_dir;
        let next = chapter_path(dl, "Solo Leveling", no("3"), Format::Pdf);
        let mut opened = Vec::new();
        let mut open = |p: &Path| {
            assert!(
                next.exists(),
                "the next chapter is on disk before OpenComic opens"
            );
            opened.push(p.to_path_buf());
            Ok(())
        };
        let mut out = Vec::new();

        let read = read_in_viewer(
            &ctx,
            "solo leveling",
            Some("2"),
            None,
            None,
            &mut open,
            &mut out,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            opened,
            [chapter_path(dl, "Solo Leveling", no("2"), Format::Pdf)]
        );
        assert_eq!(read.chapter, no("2"));
        assert_eq!(read.on, SourceId::MangaKatana);
        assert_eq!(calls(&mk), [no("2"), no("3")]);
        let progress = ctx.db.progress(read.series.id).unwrap();
        assert!(
            progress.get(&no("2")).is_none_or(|p| !p.read),
            "not read on open"
        );
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("Ch 2 is ready — pick it in OpenComic"),
            "{out}"
        );
        assert!(!out.contains("tip:"), "no half chapters: {out}");
    }

    #[tokio::test]
    async fn opencomic_1_6_6_needs_no_click() {
        let mk = Fake::new(SourceId::MangaKatana, &["Solo Leveling"], nums(1, 3));
        let (_dir, ctx) = opencomic_ctx(vec![Arc::new(mk)]);
        let storage = ctx.config.opencomic.clone().unwrap();
        std::fs::write(
            storage.join(opencomic::CONFIG_FILE),
            r#"{"appVersion":"1.6.6"}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        read_in_viewer(
            &ctx,
            "solo leveling",
            Some("1"),
            None,
            None,
            &mut |_| Ok(()),
            &mut out,
        )
        .await
        .unwrap();
        let out = String::from_utf8(out).unwrap();
        let path = chapter_path(
            &ctx.config.download_dir,
            "Solo Leveling",
            no("1"),
            Format::Pdf,
        );
        assert!(out.contains(&format!("Ch 1 → {}", path.display())), "{out}");
        assert!(!out.contains("pick it"), "{out}");
    }

    #[tokio::test]
    async fn opencomic_tip_for_half_chapters_with_default_sort() {
        let chapters = vec!["1".into(), "1.5".into(), "2".into()];
        let mk = Fake::new(SourceId::MangaKatana, &["Solo Leveling"], chapters);
        let (_dir, ctx) = opencomic_ctx(vec![Arc::new(mk)]);
        let storage = ctx.config.opencomic.clone().unwrap();
        std::fs::write(storage.join(opencomic::CONFIG_FILE), "{}").unwrap();
        let mut out = Vec::new();
        read_in_viewer(
            &ctx,
            "solo leveling",
            Some("1"),
            None,
            None,
            &mut |_| Ok(()),
            &mut out,
        )
        .await
        .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("tip: set OpenComic's sort to \"Numeric\""),
            "{out}"
        );

        std::fs::write(
            storage.join(opencomic::CONFIG_FILE),
            r#"{"sortReading":"numeric"}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        read_in_viewer(
            &ctx,
            "solo leveling",
            Some("1"),
            None,
            None,
            &mut |_| Ok(()),
            &mut out,
        )
        .await
        .unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("tip:"));
    }

    #[tokio::test]
    async fn read_resumes_from_opencomic_progress() {
        let mk = Arc::new(Fake::new(
            SourceId::MangaKatana,
            &["Solo Leveling"],
            nums(1, 5),
        ));
        let (_dir, ctx) = opencomic_ctx(vec![mk]);
        let storage = ctx.config.opencomic.clone().unwrap();
        let later = (db::now() + 60) * 1000;
        write_progress(
            &ctx,
            &storage,
            &[("1", 24, 24, true, later), ("2", 7, 20, false, later + 1)],
        );
        let Start::Read { chapter, .. } =
            read_start(&ctx, "solo leveling", None, None).await.unwrap()
        else {
            panic!("read_start always picks a chapter");
        };
        assert_eq!(
            chapter,
            Some(no("2")),
            "Ch 1 finished in OpenComic, Ch 2 started"
        );
    }

    /// Opens Ch `n` of "Solo Leveling" in OpenComic, as `hon read` does.
    async fn open_in_opencomic(ctx: &Ctx, n: &str) -> Opened {
        read_in_viewer(
            ctx,
            "solo leveling",
            Some(n),
            None,
            None,
            &mut |_| Ok(()),
            &mut Vec::new(),
        )
        .await
        .unwrap()
        .unwrap()
    }

    #[tokio::test]
    async fn follow_step_downloads_two_ahead_after_switch() {
        let mk = Arc::new(Fake::new(
            SourceId::MangaKatana,
            &["Solo Leveling"],
            nums(1, 6),
        ));
        let (_dir, ctx) = opencomic_ctx(vec![mk.clone()]);
        let storage = ctx.config.opencomic.clone().unwrap();
        let opened = open_in_opencomic(&ctx, "1").await;
        assert_eq!(calls(&mk), [no("1"), no("2")]);
        let mut state = Following::new(&opened);
        let mut out = Vec::new();

        follow_step(&ctx, &opened, &storage, &mut state, &mut out)
            .await
            .unwrap();
        assert_eq!(state.current, no("1"), "nothing saved yet");
        assert_eq!(calls(&mk), [no("1"), no("2"), no("3")], "two ahead of Ch 1");

        let later = opened.opened_at_ms + 1000;
        write_progress(&ctx, &storage, &[("1", 24, 24, true, later)]);
        follow_step(&ctx, &opened, &storage, &mut state, &mut out)
            .await
            .unwrap();
        assert_eq!(
            state.current,
            no("2"),
            "Ch 1 finished: OpenComic moved on to Ch 2"
        );
        assert_eq!(calls(&mk)[3..], [no("4")]);
        let dl = &ctx.config.download_dir;
        let on_disk = |n: &str| existing_download(dl, "Solo Leveling", no(n), Format::Pdf);
        assert!(on_disk("3").is_some() && on_disk("4").is_some());
        assert!(ctx.db.progress(opened.series.id).unwrap()[&no("1")].read);
        assert!(String::from_utf8(out).unwrap().contains("reading Ch 2"));

        follow_step(&ctx, &opened, &storage, &mut state, &mut Vec::new())
            .await
            .unwrap();
        assert_eq!(calls(&mk).len(), 4, "nothing new to download");
    }

    #[tokio::test]
    async fn follow_step_ignores_progress_from_before_opening() {
        let mk = Arc::new(Fake::new(
            SourceId::MangaKatana,
            &["Solo Leveling"],
            nums(1, 6),
        ));
        let (_dir, ctx) = opencomic_ctx(vec![mk]);
        let storage = ctx.config.opencomic.clone().unwrap();
        let opened = open_in_opencomic(&ctx, "4").await;
        let earlier = opened.opened_at_ms - 60_000;
        write_progress(&ctx, &storage, &[("1", 24, 24, true, earlier)]);
        let mut state = Following::new(&opened);
        follow_step(&ctx, &opened, &storage, &mut state, &mut Vec::new())
            .await
            .unwrap();
        assert_eq!(
            state.current,
            no("4"),
            "last week's Ch 1 isn't where you are now"
        );
    }

    #[tokio::test]
    async fn follow_step_notes_the_end_once() {
        let mk = Arc::new(Fake::new(
            SourceId::MangaKatana,
            &["Solo Leveling"],
            nums(1, 3),
        ));
        let (_dir, ctx) = opencomic_ctx(vec![mk]);
        let storage = ctx.config.opencomic.clone().unwrap();
        let mut out = Vec::new();
        let opened = read_in_viewer(
            &ctx,
            "solo leveling",
            Some("3"),
            None,
            None,
            &mut |_| Ok(()),
            &mut out,
        )
        .await
        .unwrap()
        .unwrap();
        let mut state = Following::new(&opened);
        for _ in 0..2 {
            follow_step(&ctx, &opened, &storage, &mut state, &mut out)
                .await
                .unwrap();
        }
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out.matches("no chapters after Ch 3 yet").count(),
            1,
            "{out}"
        );
    }

    #[tokio::test]
    async fn read_in_viewer_downloads_opens_and_marks_read() {
        let md = Arc::new(Fake::new(
            SourceId::MangaDex,
            &["Solo Leveling"],
            nums(1, 3),
        ));
        let mk = Arc::new(Fake::new(
            SourceId::MangaKatana,
            &["Solo Leveling"],
            nums(1, 5),
        ));
        let (_dir, ctx) = ctx(vec![md.clone(), mk.clone()]);
        let series = ctx
            .db
            .series(&["solo leveling".into()], "Solo Leveling")
            .unwrap()
            .id;
        ctx.db.set_read(series, no("1"), true).unwrap();
        ctx.db.set_read(series, no("2"), true).unwrap();
        let path = |n: &str| {
            chapter_path(
                &ctx.config.download_dir,
                "Solo Leveling",
                no(n),
                Format::Pdf,
            )
        };
        let mut opened = Vec::new();
        let mut open = |p: &Path| {
            opened.push(p.to_path_buf());
            Ok(())
        };

        let mut out = Vec::new();
        read_in_viewer(&ctx, "solo leveling", None, None, None, &mut open, &mut out)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("Ch 3 → {}\n", path("3").display())
        );
        assert!(path("3").is_file());
        assert!(ctx.db.progress(series).unwrap()[&no("3")].read);
        assert_eq!(calls(&mk), [no("3")], "the source with the newest chapter");

        // Ch 5 isn't on MangaDex: never switch source silently.
        let on_md = Some(SourceId::MangaDex);
        let mut out = Vec::new();
        let err = read_in_viewer(
            &ctx,
            "solo leveling",
            Some("5"),
            on_md,
            None,
            &mut open,
            &mut out,
        )
        .await
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "Ch 5 isn't on MangaDex; it's on MangaKatana (use --source mangakatana)"
        );
        let mut asked = Vec::new();
        let mut decline = |q: &str| {
            asked.push(q.to_string());
            Ok(false)
        };
        read_in_viewer(
            &ctx,
            "solo leveling",
            Some("5"),
            on_md,
            Some(&mut decline),
            &mut open,
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(
            asked,
            ["Ch 5 isn't on MangaDex. Open it from MangaKatana? [y/N] "]
        );
        assert!(out.is_empty(), "declined: nothing opened");
        let mut accept = |_: &str| Ok(true);
        read_in_viewer(
            &ctx,
            "solo leveling",
            Some("5"),
            on_md,
            Some(&mut accept),
            &mut open,
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(calls(&mk), [no("3"), no("5")]);
        assert!(calls(&md).is_empty());

        read_in_viewer(
            &ctx,
            "solo leveling",
            Some("3"),
            None,
            None,
            &mut open,
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(calls(&mk).len(), 2, "already downloaded");
        assert_eq!(opened, [path("3"), path("5"), path("3")]);

        let mut failing = |_: &Path| Err(anyhow!("no such app"));
        let err = read_in_viewer(
            &ctx,
            "solo leveling",
            Some("4"),
            None,
            None,
            &mut failing,
            &mut out,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("no such app"), "{err:#}");
        let progress = ctx.db.progress(series).unwrap();
        assert!(!progress.get(&no("4")).is_some_and(|p| p.read));
    }

    #[tokio::test]
    async fn download_shows_live_page_counts() {
        let (_dir, ctx) = ctx(vec![
            Arc::new(Fake::new(
                SourceId::MangaDex,
                &["Solo Leveling"],
                nums(1, 3),
            )),
            Arc::new(Fake::new(SourceId::MangaKatana, &[], Vec::new())),
        ]);
        let range = parse_range("2").unwrap();
        let live = Interaction {
            ask: None,
            live: true,
        };
        let mut out = Vec::new();
        download_to(&ctx, "solo leveling", &range, None, None, live, &mut out)
            .await
            .unwrap();
        let path = chapter_path(
            &ctx.config.download_dir,
            "Solo Leveling",
            no("2"),
            Format::Pdf,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "\rCh 2  1/2\rCh 2  2/2\r{}\rCh 2 (2 pages) → {}\n",
                " ".repeat(9),
                path.display()
            )
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
            Interaction::default(),
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
