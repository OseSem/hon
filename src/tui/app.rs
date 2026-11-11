use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use image::DynamicImage;

use super::Start;
use crate::commands::distinct_failures;
use crate::config::Reader as ReaderMode;
use crate::db::{self, Progress, resume_chapter, resume_page};
use crate::model::{ChapterNo, ChapterRef, PageRef, SourceId};
use crate::search::{SearchOutcome, TitleGroup, missing_elsewhere};
use crate::suggest::Suggestion;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Search,
    Chapters,
    Reader,
    EndOfChapter,
    SwitchSource,
}

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Resize(u16, u16),
    SearchDone(SearchOutcome),
    /// AniList's titles for a typed prefix (trimmed, lowercased).
    Suggested(String, anyhow::Result<Vec<Suggestion>>),
    Progress(HashMap<ChapterNo, Progress>),
    PagesListed(ChapterNo, anyhow::Result<Vec<PageRef>>),
    PageLoaded(usize, anyhow::Result<Arc<DynamicImage>>),
    Downloaded(ChapterNo, anyhow::Result<PathBuf>),
    /// Follows `Downloaded` when the chapter was to be opened in the viewer.
    Opened(ChapterNo, anyhow::Result<()>),
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Search(String),
    /// Ask AniList for titles starting like this, once typing pauses.
    Suggest(String),
    LoadProgress {
        keys: Vec<String>,
        title: String,
    },
    ListPages(SourceId, ChapterRef),
    FetchPage(SourceId, usize, PageRef),
    SavePage {
        ch: ChapterNo,
        source: SourceId,
        page: u32,
        page_count: u32,
    },
    SetRead(ChapterNo, bool),
    Download {
        source: SourceId,
        ch: ChapterRef,
        then_open: bool,
    },
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    Height,
    Width,
}

/// Pages at least this many times taller than wide are long strips (webtoons).
const STRIP_RATIO: u32 = 2;

pub fn is_strip(image: &DynamicImage) -> bool {
    image.height() >= image.width().saturating_mul(STRIP_RATIO).max(1)
}

#[derive(Debug)]
pub enum PageImage {
    Loading,
    Ready(Arc<DynamicImage>),
    Failed(String),
}

/// The chapter being read.
#[derive(Debug)]
pub struct Reader {
    pub ch: ChapterRef,
    /// `None` while the page list loads.
    pub pages: Option<Result<Vec<PageRef>, String>>,
    /// 0-based.
    pub page: usize,
    pub images: HashMap<usize, PageImage>,
    /// Rows scrolled down the page when it's fitted to width.
    pub scroll: u16,
    /// Furthest `scroll` goes on the page as last drawn.
    pub max_scroll: u16,
    /// Rows the page was last drawn in.
    pub view_rows: u16,
    /// Listing the pages again after a failed page: stay on `page`.
    relisting: bool,
}

impl Reader {
    /// How the current page is fitted: as `chosen`, or else long strips to width and
    /// other pages to height.
    pub fn fit(&self, chosen: Option<Fit>) -> Fit {
        if let Some(fit) = chosen {
            return fit;
        }
        match self.images.get(&self.page) {
            Some(PageImage::Ready(image)) if is_strip(image) => Fit::Width,
            _ => Fit::Height,
        }
    }
}

/// Rows Space scrolls: a screen, less a little overlap to keep your place.
fn screen_step(view_rows: u16) -> u16 {
    view_rows.saturating_sub(view_rows / 8).max(1)
}

pub struct App {
    pub screen: Screen,
    pub query: String,
    /// The query of the latest submitted search.
    pub submitted: String,
    pub editing: bool,
    pub searching: bool,
    pub groups: Vec<TitleGroup>,
    pub selected: usize,
    pub picked_source: usize,
    pub chosen: Option<(TitleGroup, SourceId)>,
    pub progress: HashMap<ChapterNo, Progress>,
    pub downloaded: HashSet<ChapterNo>,
    pub downloading: HashMap<ChapterNo, bool>,
    pub cursor: usize,
    pub status: Option<String>,
    pub reader: Option<Reader>,
    /// The fit `f` chose; until then each page is fitted by its shape.
    pub fit: Option<Fit>,
    /// What Enter on a chapter opens it in.
    pub reader_mode: ReaderMode,
    /// The chapter on another source the `SwitchSource` prompt offers.
    pub switch_to: Option<ChapterRef>,
    /// The chapter `hon read` opens once progress is loaded.
    pending_read: Option<ChapterNo>,
    /// A chapter list opened by `hon read` has no search screen to return to.
    quit_on_chapter_back: bool,
    /// Chapters Enter is opening in the viewer, marked read once they're open.
    read_once_open: HashSet<ChapterNo>,
    /// Whether typing suggests titles (config `suggestions`).
    pub suggest: bool,
    /// Titles for what's typed in the search box.
    pub suggestions: Vec<Suggestion>,
    /// The suggestion ↑↓ picked; `None` is the typed text.
    pub suggestion: Option<usize>,
    /// Why there are no suggestions, when asking failed.
    pub suggest_error: Option<String>,
    suggest_cache: HashMap<String, Vec<Suggestion>>,
    pub(super) search_scroll: usize,
    pub(super) chapter_scroll: usize,
}

const PAGE_STEP: usize = 10;
/// Characters typed before titles are suggested.
const SUGGEST_FROM: usize = 3;
/// Pages fetched ahead of the one shown.
const PREFETCH: usize = 3;
const MAX_DECODED: usize = 8;
const SCROLL_STEP: u16 = 4;

impl App {
    pub fn new(start: Start, reader_mode: ReaderMode) -> (App, Vec<Effect>) {
        let mut app = App {
            screen: Screen::Search,
            query: String::new(),
            submitted: String::new(),
            editing: true,
            searching: false,
            groups: Vec::new(),
            selected: 0,
            picked_source: 0,
            chosen: None,
            progress: HashMap::new(),
            downloaded: HashSet::new(),
            downloading: HashMap::new(),
            cursor: 0,
            status: None,
            reader: None,
            fit: None,
            reader_mode,
            switch_to: None,
            pending_read: None,
            quit_on_chapter_back: false,
            read_once_open: HashSet::new(),
            suggest: false,
            suggestions: Vec::new(),
            suggestion: None,
            suggest_error: None,
            suggest_cache: HashMap::new(),
            search_scroll: 0,
            chapter_scroll: 0,
        };
        let effects = match start {
            Start::Search => Vec::new(),
            Start::Read {
                group,
                source,
                chapter,
            } => {
                let effects = app.open(group, source);
                app.quit_on_chapter_back = true;
                app.pending_read = chapter;
                effects
            }
        };
        (app, effects)
    }

    pub fn update(&mut self, ev: AppEvent) -> Vec<Effect> {
        match ev {
            AppEvent::Key(key) if key.kind == KeyEventKind::Press => return self.key(key),
            AppEvent::SearchDone(outcome) => self.search_done(outcome),
            AppEvent::Suggested(prefix, found) => self.suggested(prefix, found),
            AppEvent::Progress(progress) => {
                self.progress = progress;
                self.cursor = self.resume_cursor();
                self.chapter_scroll = self.cursor.saturating_sub(2);
                if let Some(n) = self.pending_read.take() {
                    return self.read_number(n);
                }
            }
            AppEvent::PagesListed(n, pages) => return self.pages_listed(n, pages),
            AppEvent::PageLoaded(i, image) => self.page_loaded(i, image),
            AppEvent::Downloaded(n, Ok(path)) => {
                self.downloaded.insert(n);
                if self.downloading.remove(&n) == Some(false) {
                    self.status = Some(format!("Ch {n} → {}", path.display()));
                }
            }
            AppEvent::Downloaded(n, Err(e)) => {
                self.downloading.remove(&n);
                self.read_once_open.remove(&n);
                self.status = Some(format!("couldn't download Ch {n}: {e:#}"));
            }
            AppEvent::Opened(n, Ok(())) => {
                self.status = Some(format!("opened Ch {n}"));
                if self.read_once_open.remove(&n) {
                    self.touch(n).read = true;
                    return vec![Effect::SetRead(n, true)];
                }
            }
            AppEvent::Opened(n, Err(e)) => {
                self.read_once_open.remove(&n);
                self.status = Some(format!("couldn't open Ch {n}: {e:#}"));
            }
            AppEvent::Error(msg) => self.status = Some(msg),
            AppEvent::Key(_) | AppEvent::Resize(..) => {}
        }
        Vec::new()
    }

    /// Chapters of the chosen source, ascending.
    pub fn chapters(&self) -> &[ChapterRef] {
        self.chosen
            .as_ref()
            .and_then(|(group, source)| group.source(*source))
            .map_or(&[], |s| &s.chapters)
    }

    pub fn is_read(&self, n: ChapterNo) -> bool {
        self.progress.get(&n).is_some_and(|p| p.read)
    }

    /// The chapter after the one being read, on any source: from the session's source
    /// when it has it, else from the first other source that does.
    pub fn next_chapter(&self) -> Option<&ChapterRef> {
        let (group, source) = self.chosen.as_ref()?;
        let current = self.reader.as_ref()?.ch.number;
        let next = group
            .sources
            .iter()
            .flat_map(|s| &s.chapters)
            .map(|c| c.number)
            .filter(|&n| n > current)
            .min()?;
        let here = group
            .source(*source)
            .is_some_and(|s| s.chapters.iter().any(|c| c.number == next));
        let on = if here {
            *source
        } else {
            missing_elsewhere(group, *source, &[next]).first()?.1
        };
        group.source(on)?.chapters.iter().find(|c| c.number == next)
    }

    fn key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return vec![Effect::Quit];
        }
        match self.screen {
            Screen::Search if self.editing => self.query_key(key),
            Screen::Search => self.results_key(key),
            Screen::Chapters => self.chapters_key(key),
            Screen::Reader => self.reader_key(key),
            Screen::EndOfChapter => self.end_key(key),
            Screen::SwitchSource => self.switch_key(key),
        }
    }

    fn query_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Enter => {
                if let Some(s) = self.suggestion.and_then(|i| self.suggestions.get(i)) {
                    self.query.clone_from(&s.title);
                }
                return self.run_search();
            }
            KeyCode::Esc => {
                self.editing = false;
                self.clear_suggestions();
            }
            KeyCode::Backspace => {
                self.query.pop();
                return self.query_changed();
            }
            KeyCode::Tab => {
                let picked = self.suggestion.unwrap_or(0);
                if let Some(s) = self.suggestions.get(picked) {
                    self.query.clone_from(&s.title);
                    return self.query_changed();
                }
            }
            KeyCode::Up if !self.suggestions.is_empty() => {
                self.suggestion = self.suggestion.and_then(|i| i.checked_sub(1));
            }
            KeyCode::Down if !self.suggestions.is_empty() => {
                let last = self.suggestions.len() - 1;
                self.suggestion = Some(self.suggestion.map_or(0, |i| (i + 1).min(last)));
            }
            KeyCode::Up => self.move_group(-1),
            KeyCode::Down => self.move_group(1),
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c);
                return self.query_changed();
            }
            _ => {}
        }
        Vec::new()
    }

    fn suggest_key(&self) -> String {
        self.query.trim().to_lowercase()
    }

    /// Shows titles for the new text: from earlier answers when there are some, else asks.
    /// Until the answer comes the old titles stay, so the list doesn't flicker.
    fn query_changed(&mut self) -> Vec<Effect> {
        self.suggestion = None;
        let key = self.suggest_key();
        if !self.suggest || key.chars().count() < SUGGEST_FROM {
            self.clear_suggestions();
            return Vec::new();
        }
        if let Some(found) = self.suggest_cache.get(&key) {
            self.suggestions = found.clone();
            self.suggest_error = None;
            return Vec::new();
        }
        vec![Effect::Suggest(key)]
    }

    fn suggested(&mut self, prefix: String, found: anyhow::Result<Vec<Suggestion>>) {
        let current = self.editing && prefix == self.suggest_key();
        match found {
            Ok(found) => {
                if current {
                    self.suggestions = found.clone();
                    self.suggestion = None;
                    self.suggest_error = None;
                }
                self.suggest_cache.insert(prefix, found);
            }
            Err(e) if current => {
                self.suggestions.clear();
                self.suggestion = None;
                self.suggest_error = Some(format!("{e:#}"));
            }
            Err(_) => {}
        }
    }

    fn clear_suggestions(&mut self) {
        self.suggestions.clear();
        self.suggestion = None;
        self.suggest_error = None;
    }

    fn results_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('q') => return vec![Effect::Quit],
            KeyCode::Enter => return self.open_selected(),
            KeyCode::Char('/') => {
                self.editing = true;
                self.query.clear();
                self.clear_suggestions();
            }
            KeyCode::Up => self.move_group(-1),
            KeyCode::Down => self.move_group(1),
            KeyCode::Tab => {
                if let Some(group) = self.groups.get(self.selected) {
                    self.picked_source = (self.picked_source + 1) % group.sources.len();
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn run_search(&mut self) -> Vec<Effect> {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            return Vec::new();
        }
        self.submitted.clone_from(&query);
        self.editing = false;
        self.clear_suggestions();
        self.searching = true;
        self.groups.clear();
        self.selected = 0;
        self.picked_source = 0;
        self.search_scroll = 0;
        self.status = None;
        vec![Effect::Search(query)]
    }

    fn search_done(&mut self, outcome: SearchOutcome) {
        self.searching = false;
        self.groups = outcome.groups;
        self.selected = 0;
        self.picked_source = 0;
        let failures = distinct_failures(&outcome.failures);
        self.status = if !failures.is_empty() {
            Some(failures.join("; "))
        } else if self.groups.is_empty() {
            Some(format!("no results for \"{}\"", self.submitted))
        } else {
            None
        };
    }

    fn move_group(&mut self, by: isize) {
        let last = self.groups.len().saturating_sub(1);
        let next = self.selected.saturating_add_signed(by).min(last);
        if next != self.selected {
            self.selected = next;
            self.picked_source = 0;
        }
    }

    fn open_selected(&mut self) -> Vec<Effect> {
        let Some(group) = self.groups.get(self.selected) else {
            return Vec::new();
        };
        let source = group.sources[self.picked_source].manga.source;
        self.open(group.clone(), source)
    }

    fn open(&mut self, group: TitleGroup, source: SourceId) -> Vec<Effect> {
        let effect = Effect::LoadProgress {
            keys: group.keys.clone(),
            title: group.title.clone(),
        };
        self.screen = Screen::Chapters;
        self.chosen = Some((group, source));
        self.reader = None;
        self.switch_to = None;
        self.pending_read = None;
        self.read_once_open.clear();
        self.progress.clear();
        self.downloaded.clear();
        self.downloading.clear();
        self.cursor = 0;
        self.chapter_scroll = 0;
        self.status = None;
        vec![effect]
    }

    /// The resume chapter's row on the session's source.
    fn resume_cursor(&self) -> usize {
        let numbers: Vec<ChapterNo> = self.chapters().iter().map(|c| c.number).collect();
        resume_chapter(&numbers, &self.progress)
            .and_then(|n| numbers.iter().position(|&c| c == n))
            .unwrap_or(0)
    }

    /// The chapter's progress, marked as just changed.
    fn touch(&mut self, n: ChapterNo) -> &mut Progress {
        let p = self.progress.entry(n).or_insert_with(no_progress);
        p.updated_at = db::now();
        p
    }

    fn chapters_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let last = self.chapters().len().saturating_sub(1);
        match key.code {
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor = (self.cursor + 1).min(last),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(PAGE_STEP),
            KeyCode::PageDown => self.cursor = (self.cursor + PAGE_STEP).min(last),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = last,
            KeyCode::Esc | KeyCode::Char('q') if self.quit_on_chapter_back => {
                return vec![Effect::Quit];
            }
            KeyCode::Esc => {
                self.screen = Screen::Search;
                self.status = None;
            }
            KeyCode::Char('s') => self.switch_source(),
            KeyCode::Char('m') => return self.toggle_read(),
            KeyCode::Char('d') => return self.download(false),
            KeyCode::Char('o') => return self.download(true),
            KeyCode::Enter if self.reader_mode == ReaderMode::Viewer => {
                return self.open_in_viewer();
            }
            KeyCode::Enter => {
                if let Some(ch) = self.chapters().get(self.cursor).cloned() {
                    return self.open_chapter(ch);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Opens chapter `n` on the session's source, or asks to read it from another one.
    fn read_number(&mut self, n: ChapterNo) -> Vec<Effect> {
        if let Some(ch) = self.chapters().iter().find(|c| c.number == n).cloned() {
            return self.open_chapter(ch);
        }
        let Some((group, source)) = &self.chosen else {
            return Vec::new();
        };
        let elsewhere = missing_elsewhere(group, *source, &[n])
            .first()
            .and_then(|&(_, other)| group.source(other))
            .and_then(|s| s.chapters.iter().find(|c| c.number == n))
            .cloned();
        match elsewhere {
            Some(ch) => {
                self.switch_to = Some(ch);
                self.screen = Screen::SwitchSource;
            }
            None => self.status = Some(format!("there's no Ch {n}")),
        }
        Vec::new()
    }

    fn open_chapter(&mut self, ch: ChapterRef) -> Vec<Effect> {
        if let Some(i) = self.chapters().iter().position(|c| c.number == ch.number) {
            self.cursor = i;
        }
        self.screen = Screen::Reader;
        self.status = None;
        self.switch_to = None;
        let effect = Effect::ListPages(ch.source, ch.clone());
        self.reader = Some(Reader {
            ch,
            pages: None,
            page: 0,
            images: HashMap::new(),
            scroll: 0,
            max_scroll: 0,
            view_rows: 0,
            relisting: false,
        });
        vec![effect]
    }

    fn back_to_list(&mut self) {
        self.reader = None;
        self.screen = Screen::Chapters;
    }

    fn pages_listed(&mut self, n: ChapterNo, pages: anyhow::Result<Vec<PageRef>>) -> Vec<Effect> {
        let Some(reader) = &mut self.reader else {
            return Vec::new();
        };
        if reader.ch.number != n || reader.pages.is_some() {
            return Vec::new();
        }
        reader.pages = Some(match pages {
            Ok(pages) if pages.is_empty() => Err("it has no pages".into()),
            Ok(pages) => {
                if !reader.relisting {
                    reader.page = resume_page(self.progress.get(&n), reader.ch.source) as usize;
                }
                reader.relisting = false;
                reader.page = reader.page.min(pages.len() - 1);
                Ok(pages)
            }
            Err(e) => Err(format!("{e:#}")),
        });
        self.fetch_ahead()
    }

    /// Fetches the current page and the next few that aren't loaded or loading.
    fn fetch_ahead(&mut self) -> Vec<Effect> {
        let Some(reader) = &mut self.reader else {
            return Vec::new();
        };
        let Some(Ok(pages)) = &reader.pages else {
            return Vec::new();
        };
        let mut effects = Vec::new();
        for (i, page) in pages
            .iter()
            .enumerate()
            .skip(reader.page)
            .take(PREFETCH + 1)
        {
            if let Entry::Vacant(slot) = reader.images.entry(i) {
                slot.insert(PageImage::Loading);
                effects.push(Effect::FetchPage(reader.ch.source, i, page.clone()));
            }
        }
        effects
    }

    fn page_loaded(&mut self, i: usize, image: anyhow::Result<Arc<DynamicImage>>) {
        let Some(reader) = &mut self.reader else {
            return;
        };
        let Some(slot) = reader.images.get_mut(&i) else {
            return;
        };
        if !matches!(slot, PageImage::Loading) {
            return;
        }
        *slot = match image {
            Ok(image) => PageImage::Ready(image),
            Err(e) => PageImage::Failed(format!("{e:#}")),
        };
        // Drop the decoded pages farthest from the current one.
        loop {
            let ready = reader
                .images
                .iter()
                .filter(|(_, image)| matches!(image, PageImage::Ready(_)))
                .map(|(i, _)| *i);
            if ready.clone().count() <= MAX_DECODED {
                break;
            }
            if let Some(far) = ready.max_by_key(|i| i.abs_diff(reader.page)) {
                reader.images.remove(&far);
            }
        }
    }

    fn reader_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(reader) = &mut self.reader else {
            return Vec::new();
        };
        match key.code {
            // Forward and back scroll through a page taller than the screen before
            // turning it, so a long strip is read in full.
            KeyCode::Right | KeyCode::Char('l' | ' ') | KeyCode::PageDown
                if reader.scroll < reader.max_scroll =>
            {
                reader.scroll = reader
                    .scroll
                    .saturating_add(screen_step(reader.view_rows))
                    .min(reader.max_scroll);
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::PageUp
                if reader.scroll.min(reader.max_scroll) > 0 =>
            {
                reader.scroll = reader
                    .scroll
                    .min(reader.max_scroll)
                    .saturating_sub(screen_step(reader.view_rows));
            }
            KeyCode::Right | KeyCode::Char('l' | ' ') | KeyCode::PageDown => {
                return self.next_page();
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::PageUp if reader.page > 0 => {
                let previous = reader.page - 1;
                let effects = self.go_to(previous);
                if let Some(reader) = &mut self.reader {
                    // Clamped to the bottom of the page when it's drawn.
                    reader.scroll = u16::MAX;
                }
                return effects;
            }
            KeyCode::Down => reader.scroll = reader.scroll.saturating_add(SCROLL_STEP),
            KeyCode::Up => reader.scroll = reader.scroll.saturating_sub(SCROLL_STEP),
            KeyCode::Char('f') => {
                self.fit = Some(match reader.fit(self.fit) {
                    Fit::Height => Fit::Width,
                    Fit::Width => Fit::Height,
                });
                reader.scroll = 0;
            }
            KeyCode::Char('r') => return self.retry(),
            KeyCode::Char('o') => return self.download(true),
            KeyCode::Char('q') | KeyCode::Esc => self.back_to_list(),
            _ => {}
        }
        Vec::new()
    }

    fn next_page(&mut self) -> Vec<Effect> {
        let Some(reader) = &self.reader else {
            return Vec::new();
        };
        let Some(Ok(pages)) = &reader.pages else {
            return Vec::new();
        };
        if reader.page + 1 < pages.len() {
            return self.go_to(reader.page + 1);
        }
        let n = reader.ch.number;
        self.touch(n).read = true;
        self.screen = Screen::EndOfChapter;
        vec![Effect::SetRead(n, true)]
    }

    fn go_to(&mut self, page: usize) -> Vec<Effect> {
        let Some(reader) = &mut self.reader else {
            return Vec::new();
        };
        let Some(Ok(pages)) = &reader.pages else {
            return Vec::new();
        };
        reader.page = page;
        reader.scroll = 0;
        reader.max_scroll = 0;
        let (ch, source) = (reader.ch.number, reader.ch.source);
        let (page, page_count) = (page as u32, pages.len() as u32);
        let p = self.touch(ch);
        p.last_page = Some(page);
        p.page_count = Some(page_count);
        p.source = Some(source);
        let mut effects = vec![Effect::SavePage {
            ch,
            source,
            page,
            page_count,
        }];
        effects.extend(self.fetch_ahead());
        effects
    }

    fn retry(&mut self) -> Vec<Effect> {
        let Some(reader) = &mut self.reader else {
            return Vec::new();
        };
        match &reader.pages {
            Some(Err(_)) => {
                reader.pages = None;
                vec![Effect::ListPages(reader.ch.source, reader.ch.clone())]
            }
            // Page URLs can expire (MangaDex's last ~15 minutes), so list them again.
            Some(Ok(_))
                if matches!(reader.images.get(&reader.page), Some(PageImage::Failed(_))) =>
            {
                reader.pages = None;
                reader.relisting = true;
                // Fetches in flight belong to the old listing; their results are dropped.
                reader
                    .images
                    .retain(|_, image| matches!(image, PageImage::Ready(_)));
                vec![Effect::ListPages(reader.ch.source, reader.ch.clone())]
            }
            _ => Vec::new(),
        }
    }

    fn end_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Enter => {
                if let Some(n) = self.next_chapter().map(|c| c.number) {
                    return self.read_number(n);
                }
            }
            KeyCode::Left | KeyCode::Char('h') => self.screen = Screen::Reader,
            KeyCode::Char('l') | KeyCode::Esc => self.back_to_list(),
            KeyCode::Char('q') => return vec![Effect::Quit],
            _ => {}
        }
        Vec::new()
    }

    fn switch_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('y') => {
                let Some(ch) = self.switch_to.take() else {
                    return Vec::new();
                };
                if let Some((_, source)) = &mut self.chosen {
                    *source = ch.source;
                }
                return self.open_chapter(ch);
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.switch_to = None;
                self.back_to_list();
            }
            _ => {}
        }
        Vec::new()
    }

    fn switch_source(&mut self) {
        let current = self.chapters().get(self.cursor).map(|c| c.number);
        let Some((group, source)) = &mut self.chosen else {
            return;
        };
        if group.sources.len() < 2 {
            self.status = Some(format!("{} is only on {}", group.title, source.name()));
            return;
        }
        let i = group
            .sources
            .iter()
            .position(|s| s.manga.source == *source)
            .unwrap_or(0);
        *source = group.sources[(i + 1) % group.sources.len()].manga.source;
        self.cursor = current
            .and_then(|n| self.chapters().iter().position(|c| c.number == n))
            .unwrap_or_else(|| self.resume_cursor());
    }

    fn toggle_read(&mut self) -> Vec<Effect> {
        let Some(n) = self.chapters().get(self.cursor).map(|c| c.number) else {
            return Vec::new();
        };
        let p = self.touch(n);
        p.read = !p.read;
        if !p.read {
            p.last_page = None;
        }
        vec![Effect::SetRead(n, p.read)]
    }

    /// `o`, and marks the chapter read once it's open: hon can't follow the pages in
    /// another app.
    fn open_in_viewer(&mut self) -> Vec<Effect> {
        let effects = self.download(true);
        if let Some(Effect::Download { ch, .. }) = effects.first() {
            self.read_once_open.insert(ch.number);
        }
        effects
    }

    fn download(&mut self, then_open: bool) -> Vec<Effect> {
        let (Some(ch), Some((_, source))) = (self.chapters().get(self.cursor), &self.chosen) else {
            return Vec::new();
        };
        let (ch, source, n) = (ch.clone(), *source, ch.number);
        let downloaded = self.downloaded.contains(&n);
        if self.downloading.contains_key(&n) {
            self.status = Some(format!("Ch {n} is already on its way"));
            return Vec::new();
        }
        if downloaded && !then_open {
            self.status = Some(format!("Ch {n} is already downloaded"));
            return Vec::new();
        }
        self.downloading.insert(n, then_open);
        self.status = Some(if downloaded {
            format!("opening Ch {n}…")
        } else {
            format!("downloading Ch {n}…")
        });
        vec![Effect::Download {
            source,
            ch,
            then_open,
        }]
    }
}

fn no_progress() -> Progress {
    Progress {
        read: false,
        last_page: None,
        page_count: None,
        source: None,
        updated_at: 0,
    }
}

#[cfg(test)]
pub(crate) mod test_data {

    use super::*;
    use crate::model::MangaRef;
    use crate::search::{SourceSummary, find_gaps};

    pub fn n(v: u32) -> ChapterNo {
        ChapterNo::parse(&v.to_string()).unwrap()
    }

    fn title_of(source: SourceId, n: u32) -> Option<String> {
        let t = match (source, n) {
            (SourceId::MangaKatana, 110) => "The Hunter Association",
            (SourceId::MangaKatana, 111) => "Raid",
            (SourceId::MangaKatana, 112) => "Shadow Monarch",
            (SourceId::MangaKatana, 113) => "Return",
            (SourceId::MangaKatana, 114) => "Jeju Island",
            (SourceId::MangaKatana, 115) => "Ants",
            _ => return None,
        };
        Some(t.to_string())
    }

    fn summary(
        source: SourceId,
        title: &str,
        nums: impl IntoIterator<Item = u32>,
        days_ago: Option<i64>,
    ) -> SourceSummary {
        let published =
            days_ago.map(|d| jiff::Timestamp::now() - jiff::SignedDuration::from_hours(d * 24 + 1));
        let chapters: Vec<ChapterRef> = nums
            .into_iter()
            .map(|v| ChapterRef {
                source,
                id: format!("{}-{v}", source.slug()),
                number: n(v),
                title: title_of(source, v),
                published,
            })
            .collect();
        SourceSummary {
            manga: MangaRef {
                source,
                id: title.to_lowercase(),
                title: title.to_string(),
                alt_titles: Vec::new(),
                url: String::new(),
            },
            latest: chapters.iter().map(|c| c.number).max(),
            gaps: find_gaps(&chapters),
            updated: published,
            chapters,
        }
    }

    fn group(title: &str, sources: Vec<SourceSummary>) -> TitleGroup {
        TitleGroup {
            keys: vec![crate::model::normalize_title(title)],
            title: title.to_string(),
            sources,
        }
    }

    /// The search results from the approved mockup (screen 1).
    pub fn outcome() -> SearchOutcome {
        let (mk, md) = (SourceId::MangaKatana, SourceId::MangaDex);
        SearchOutcome {
            groups: vec![
                group(
                    "Solo Leveling",
                    vec![
                        summary(mk, "Solo Leveling", 1..=201, Some(2)),
                        summary(md, "Solo Leveling", (0..=44).chain(67..=200), None),
                    ],
                ),
                group(
                    "Solo Leveling: Ragnarok",
                    vec![
                        summary(mk, "Solo Leveling: Ragnarok", 1..=58, None),
                        summary(md, "Solo Leveling: Ragnarok", 1..=54, None),
                    ],
                ),
                group(
                    "Solo Leveling: Side Story",
                    vec![summary(md, "Solo Leveling: Side Story", 1..=21, None)],
                ),
            ],
            failures: Vec::new(),
        }
    }

    pub fn key(code: KeyCode) -> AppEvent {
        AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    pub fn typed(app: &mut App, text: &str) -> Vec<Effect> {
        text.chars()
            .flat_map(|c| app.update(key(KeyCode::Char(c))))
            .collect()
    }

    /// Searched for "solo leveling" and got the mockup's results.
    pub fn searched() -> App {
        searched_with(ReaderMode::Terminal)
    }

    pub fn searched_with(reader: ReaderMode) -> App {
        let (mut app, _) = App::new(Start::Search, reader);
        typed(&mut app, "solo leveling");
        app.update(key(KeyCode::Enter));
        app.update(AppEvent::SearchDone(outcome()));
        app
    }

    pub fn read(nums: impl IntoIterator<Item = u32>) -> HashMap<ChapterNo, Progress> {
        nums.into_iter()
            .map(|v| {
                let p = Progress {
                    read: true,
                    last_page: None,
                    page_count: None,
                    source: None,
                    updated_at: 0,
                };
                (n(v), p)
            })
            .collect()
    }

    /// Opened "Solo Leveling" on MangaKatana with `progress` loaded.
    pub fn chapters(progress: HashMap<ChapterNo, Progress>) -> App {
        let mut app = searched();
        app.update(key(KeyCode::Enter));
        app.update(AppEvent::Progress(progress));
        app
    }

    /// `chapters`, with `reader = "viewer"`.
    pub fn chapters_in_viewer(progress: HashMap<ChapterNo, Progress>) -> App {
        let mut app = searched_with(ReaderMode::Viewer);
        app.update(key(KeyCode::Enter));
        app.update(AppEvent::Progress(progress));
        app
    }

    /// Opened "Solo Leveling" on MangaDex (no Ch 45–66) with `progress` loaded.
    pub fn chapters_on_mangadex(progress: HashMap<ChapterNo, Progress>) -> App {
        let mut app = searched();
        app.update(key(KeyCode::Tab));
        app.update(key(KeyCode::Enter));
        app.update(AppEvent::Progress(progress));
        app
    }

    pub fn pages(count: usize) -> Vec<PageRef> {
        (0..count)
            .map(|i| PageRef {
                url: format!("p{i}"),
            })
            .collect()
    }

    pub fn page_image() -> Arc<DynamicImage> {
        Arc::new(DynamicImage::new_rgb8(40, 60))
    }

    /// Touched after the chapters `read` marks.
    pub fn in_progress(page: u32, count: u32, source: SourceId) -> Progress {
        Progress {
            read: false,
            last_page: Some(page),
            page_count: Some(count),
            source: Some(source),
            updated_at: 1,
        }
    }

    /// Opens chapter `v` from the chapter list and lists `count` pages for it.
    pub fn reading(app: &mut App, v: u32, count: usize) -> Vec<Effect> {
        app.cursor = app
            .chapters()
            .iter()
            .position(|c| c.number == n(v))
            .unwrap();
        app.update(key(KeyCode::Enter));
        app.update(AppEvent::PagesListed(n(v), Ok(pages(count))))
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::test_data::*;
    use super::*;

    fn current(app: &App) -> ChapterNo {
        app.chapters()[app.cursor].number
    }

    #[test]
    fn enter_runs_search() {
        let (mut app, effects) = App::new(Start::Search, ReaderMode::Terminal);
        assert!(effects.is_empty());
        assert!(typed(&mut app, "solo").is_empty());
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Effect::Search("solo".into())]
        );
        assert!(app.searching);
        assert!(!app.editing);
    }

    #[test]
    fn tab_cycles_source_enter_opens_chapters() {
        let mut app = searched();
        assert!(app.update(key(KeyCode::Tab)).is_empty());
        let effects = app.update(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Chapters);
        let (group, source) = app.chosen.as_ref().unwrap();
        assert_eq!(group.title, "Solo Leveling");
        assert_eq!(*source, group.sources[1].manga.source);
        assert_eq!(*source, SourceId::MangaDex);
        assert_eq!(
            effects,
            [Effect::LoadProgress {
                keys: vec!["solo leveling".into()],
                title: "Solo Leveling".into()
            }]
        );

        let mut app = searched();
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Tab));
        app.update(key(KeyCode::Tab));
        app.update(key(KeyCode::Enter));
        let (group, source) = app.chosen.as_ref().unwrap();
        assert_eq!(group.title, "Solo Leveling: Ragnarok");
        assert_eq!(*source, SourceId::MangaKatana, "tab wraps around");
    }

    #[test]
    fn cursor_starts_at_resume_chapter() {
        let app = chapters(read([]));
        assert_eq!(current(&app), n(1));

        let app = chapters(read(1..=3));
        assert_eq!(current(&app), n(4));

        let app = chapters(read(1..=201));
        assert_eq!(current(&app), n(201), "all read: last chapter");

        let mut progress = read((1..=120).filter(|&v| v != 3));
        for p in progress.values_mut() {
            p.updated_at = 10;
        }
        progress.insert(n(3), in_progress(0, 20, SourceId::MangaKatana));
        let app = chapters(progress);
        assert_eq!(current(&app), n(121), "not the old stray page of Ch 3");

        let progress = HashMap::from([(n(50), in_progress(4, 20, SourceId::MangaKatana))]);
        assert_eq!(current(&chapters(progress)), n(50), "started at Ch 50");
    }

    #[test]
    fn m_toggles_read() {
        let mut app = chapters(read(1..=3));
        assert_eq!(
            app.update(key(KeyCode::Char('m'))),
            [Effect::SetRead(n(4), true)]
        );
        assert!(app.progress[&n(4)].read);
        assert_eq!(
            app.update(key(KeyCode::Char('m'))),
            [Effect::SetRead(n(4), false)]
        );
        assert!(!app.progress[&n(4)].read);
    }

    #[test]
    fn s_switches_source_keeps_chapter() {
        let mut app = chapters(read([]));
        app.cursor = 99;
        assert_eq!(current(&app), n(100));
        assert!(app.update(key(KeyCode::Char('s'))).is_empty());
        assert_eq!(app.chosen.as_ref().unwrap().1, SourceId::MangaDex);
        assert_eq!(current(&app), n(100));

        // Ch 50 isn't on MangaDex; back on MangaKatana it is.
        app.update(key(KeyCode::Char('s')));
        app.cursor = 49;
        app.update(key(KeyCode::Char('s')));
        assert_eq!(app.chosen.as_ref().unwrap().1, SourceId::MangaDex);
        assert_eq!(current(&app), n(0), "falls back to the resume chapter");
    }

    #[test]
    fn q_in_query_box_types_q() {
        let (mut app, _) = App::new(Start::Search, ReaderMode::Terminal);
        assert!(app.update(key(KeyCode::Char('q'))).is_empty());
        assert_eq!(app.query, "q");
        app.update(key(KeyCode::Esc));
        assert_eq!(app.update(key(KeyCode::Char('q'))), [Effect::Quit]);
    }

    fn suggesting() -> App {
        let (mut app, _) = App::new(Start::Search, ReaderMode::Terminal);
        app.suggest = true;
        app
    }

    fn titles(names: &[&str]) -> Vec<Suggestion> {
        names
            .iter()
            .map(|t| Suggestion {
                title: (*t).into(),
                kind: "manhwa",
                year: None,
            })
            .collect()
    }

    #[test]
    fn typing_asks_for_suggestions_from_three_characters() {
        let mut app = suggesting();
        assert!(typed(&mut app, "so").is_empty());
        assert_eq!(typed(&mut app, "L"), [Effect::Suggest("sol".into())]);
        app.update(AppEvent::Suggested(
            "sol".into(),
            Ok(titles(&["Solo Leveling"])),
        ));
        assert_eq!(app.suggestions, titles(&["Solo Leveling"]));

        assert!(app.update(key(KeyCode::Backspace)).is_empty());
        assert!(app.suggestions.is_empty(), "too short again");
    }

    #[test]
    fn suggestions_off_asks_nothing() {
        let (mut app, _) = App::new(Start::Search, ReaderMode::Terminal);
        assert!(typed(&mut app, "solo").is_empty());
    }

    #[test]
    fn answers_for_an_older_prefix_are_cached_not_shown() {
        let mut app = suggesting();
        typed(&mut app, "solo");
        app.update(AppEvent::Suggested("sol".into(), Ok(titles(&["Sol"]))));
        assert!(app.suggestions.is_empty());
        // Back to "sol": answered from the cache, no request.
        assert!(app.update(key(KeyCode::Backspace)).is_empty());
        assert_eq!(app.suggestions, titles(&["Sol"]));
    }

    #[test]
    fn picked_suggestion_is_searched() {
        let mut app = suggesting();
        typed(&mut app, "solo");
        app.update(AppEvent::Suggested(
            "solo".into(),
            Ok(titles(&["Solo Leveling", "Solo Max-Level Newbie"])),
        ));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        assert_eq!(app.suggestion, Some(1), "stops at the last");
        app.update(key(KeyCode::Up));
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Effect::Search("Solo Leveling".into())]
        );
        assert!(app.suggestions.is_empty());
    }

    #[test]
    fn enter_without_a_pick_searches_the_typed_text() {
        let mut app = suggesting();
        typed(&mut app, "solo");
        app.update(AppEvent::Suggested(
            "solo".into(),
            Ok(titles(&["Solo Leveling"])),
        ));
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Effect::Search("solo".into())]
        );
    }

    #[test]
    fn tab_completes_the_first_suggestion() {
        let mut app = suggesting();
        typed(&mut app, "solo");
        app.update(AppEvent::Suggested(
            "solo".into(),
            Ok(titles(&["Solo Leveling"])),
        ));
        assert_eq!(
            app.update(key(KeyCode::Tab)),
            [Effect::Suggest("solo leveling".into())]
        );
        assert_eq!(app.query, "Solo Leveling");
        assert!(app.editing);
    }

    #[test]
    fn failed_suggestions_say_why_until_typing_on() {
        let mut app = suggesting();
        typed(&mut app, "solo");
        app.update(AppEvent::Suggested(
            "solo".into(),
            Err(anyhow::anyhow!("offline")),
        ));
        assert_eq!(app.suggest_error.as_deref(), Some("offline"));
        app.update(key(KeyCode::Esc));
        assert_eq!(app.suggest_error, None);
    }

    #[test]
    fn ctrl_c_quits_while_typing() {
        let (mut app, _) = App::new(Start::Search, ReaderMode::Terminal);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.update(AppEvent::Key(ctrl_c)), [Effect::Quit]);
        assert_eq!(app.query, "");
    }

    #[test]
    fn slash_starts_new_search_esc_goes_back() {
        let mut app = chapters(read([]));
        app.update(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Search);
        assert_eq!(app.groups.len(), 3, "results kept");
        app.update(key(KeyCode::Char('/')));
        assert!(app.editing);
        assert_eq!(app.query, "");
    }

    #[test]
    fn search_failures_in_status() {
        let (mut app, _) = App::new(Start::Search, ReaderMode::Terminal);
        typed(&mut app, "x");
        app.update(key(KeyCode::Enter));
        let mut outcome = outcome();
        outcome.failures = vec![
            (SourceId::MangaKatana, "timed out after 20s".into()),
            (SourceId::MangaKatana, "timed out after 20s".into()),
        ];
        app.update(AppEvent::SearchDone(outcome));
        assert!(!app.searching);
        assert_eq!(
            app.status.as_deref(),
            Some("MangaKatana failed: timed out after 20s")
        );

        app.update(key(KeyCode::Char('/')));
        typed(&mut app, "zzz");
        app.update(key(KeyCode::Enter));
        app.update(key(KeyCode::Char('/')));
        typed(&mut app, "next");
        app.update(AppEvent::SearchDone(SearchOutcome {
            groups: Vec::new(),
            failures: Vec::new(),
        }));
        assert_eq!(
            app.status.as_deref(),
            Some("no results for \"zzz\""),
            "names the submitted query, not the text box"
        );
    }

    #[test]
    fn opening_a_series_forgets_pending_downloads() {
        let mut app = chapters(read(1..=4));
        assert_eq!(current(&app), n(5));
        assert_eq!(app.update(key(KeyCode::Char('d'))).len(), 1);
        app.update(key(KeyCode::Esc));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Enter));
        assert_eq!(
            app.chosen.as_ref().unwrap().0.title,
            "Solo Leveling: Ragnarok"
        );
        assert!(app.downloading.is_empty());
        app.update(AppEvent::Progress(read(1..=4)));
        assert!(
            matches!(
                app.update(key(KeyCode::Char('d')))[..],
                [Effect::Download { .. }]
            ),
            "Ch 5 of the new series isn't 'already on its way'"
        );
    }

    #[test]
    fn d_downloads_o_opens() {
        let mut app = chapters(read(1..=111));
        let ch = app.chapters()[app.cursor].clone();
        assert_eq!(ch.number, n(112));
        let d = app.update(key(KeyCode::Char('d')));
        assert_eq!(
            d,
            [Effect::Download {
                source: SourceId::MangaKatana,
                ch: ch.clone(),
                then_open: false
            }]
        );
        assert!(
            app.update(key(KeyCode::Char('d'))).is_empty(),
            "no second download while one runs"
        );
        app.update(AppEvent::Downloaded(n(112), Ok("/x/c.pdf".into())));
        assert!(app.downloaded.contains(&n(112)));
        assert!(
            app.update(key(KeyCode::Char('d'))).is_empty(),
            "already downloaded"
        );
        assert_eq!(
            app.update(key(KeyCode::Char('o'))),
            [Effect::Download {
                source: SourceId::MangaKatana,
                ch,
                then_open: true
            }]
        );
    }

    #[test]
    fn enter_in_viewer_mode_opens_in_viewer_and_marks_read_once_opened() {
        let mut app = chapters_in_viewer(read(1..=111));
        let ch = app.chapters()[app.cursor].clone();
        assert_eq!(ch.number, n(112));
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Effect::Download {
                source: SourceId::MangaKatana,
                ch,
                then_open: true
            }]
        );
        assert_eq!(app.screen, Screen::Chapters, "no in-terminal reader");
        assert!(app.reader.is_none());
        assert_eq!(app.status.as_deref(), Some("downloading Ch 112…"));
        assert!(!app.is_read(n(112)), "not until it's open");
        assert!(
            app.update(AppEvent::Downloaded(n(112), Ok("/x/c.pdf".into())))
                .is_empty()
        );
        assert!(!app.is_read(n(112)));
        assert_eq!(
            app.update(AppEvent::Opened(n(112), Ok(()))),
            [Effect::SetRead(n(112), true)]
        );
        assert!(app.is_read(n(112)));
        assert_eq!(app.status.as_deref(), Some("opened Ch 112"));

        assert_eq!(
            app.update(key(KeyCode::Char('m'))),
            [Effect::SetRead(n(112), false)],
            "m still toggles it back"
        );
        assert!(!app.is_read(n(112)));
    }

    #[test]
    fn enter_in_viewer_mode_failing_leaves_chapter_unread() {
        let mut app = chapters_in_viewer(read([]));
        app.update(key(KeyCode::Enter));
        let failed = app.update(AppEvent::Downloaded(n(1), Err(anyhow::anyhow!("HTTP 500"))));
        assert!(failed.is_empty());
        assert!(!app.is_read(n(1)));
        assert_eq!(
            app.status.as_deref(),
            Some("couldn't download Ch 1: HTTP 500")
        );

        app.update(key(KeyCode::Enter));
        app.update(AppEvent::Downloaded(n(1), Ok("/x/1.pdf".into())));
        let failed = app.update(AppEvent::Opened(n(1), Err(anyhow::anyhow!("no such app"))));
        assert!(failed.is_empty());
        assert!(!app.is_read(n(1)));
        assert_eq!(
            app.status.as_deref(),
            Some("couldn't open Ch 1: no such app")
        );
        assert!(
            app.update(AppEvent::Opened(n(1), Ok(()))).is_empty(),
            "a later open of the same chapter isn't from Enter"
        );
    }

    #[test]
    fn o_opens_without_marking_read() {
        for mut app in [chapters(read([])), chapters_in_viewer(read([]))] {
            app.update(key(KeyCode::Char('o')));
            app.update(AppEvent::Downloaded(n(1), Ok("/x/1.pdf".into())));
            assert!(app.update(AppEvent::Opened(n(1), Ok(()))).is_empty());
            assert!(!app.is_read(n(1)));
            assert_eq!(app.status.as_deref(), Some("opened Ch 1"));
        }
    }

    #[test]
    fn enter_in_viewer_mode_while_downloading_does_nothing() {
        let mut app = chapters_in_viewer(read([]));
        app.update(key(KeyCode::Char('d')));
        assert!(app.update(key(KeyCode::Enter)).is_empty());
        assert!(!app.is_read(n(1)));
    }

    fn fetched(effects: &[Effect]) -> Vec<usize> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::FetchPage(_, i, page) => {
                    assert_eq!(page.url, format!("p{i}"));
                    Some(*i)
                }
                _ => None,
            })
            .collect()
    }

    fn reader(app: &App) -> &Reader {
        app.reader.as_ref().expect("reading")
    }

    fn chapter_on(app: &App, source: SourceId, v: u32) -> ChapterRef {
        let (group, _) = app.chosen.as_ref().unwrap();
        let chapters = &group.source(source).unwrap().chapters;
        chapters.iter().find(|c| c.number == n(v)).unwrap().clone()
    }

    #[test]
    fn opens_at_resume_page_same_source_only() {
        let mut progress = read(1..=111);
        progress.insert(n(112), in_progress(13, 38, SourceId::MangaKatana));

        let mut app = chapters(progress.clone());
        assert_eq!(current(&app), n(112));
        let ch = chapter_on(&app, SourceId::MangaKatana, 112);
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Effect::ListPages(SourceId::MangaKatana, ch)]
        );
        assert_eq!(app.screen, Screen::Reader);
        let effects = app.update(AppEvent::PagesListed(n(112), Ok(pages(38))));
        assert_eq!(reader(&app).page, 13);
        assert_eq!(fetched(&effects), [13, 14, 15, 16]);

        let mut app = chapters_on_mangadex(progress);
        let effects = reading(&mut app, 112, 38);
        assert_eq!(reader(&app).page, 0, "progress was on MangaKatana");
        assert_eq!(fetched(&effects), [0, 1, 2, 3]);
    }

    #[test]
    fn resume_page_past_the_end_opens_last_page() {
        let progress = HashMap::from([(n(5), in_progress(40, 41, SourceId::MangaKatana))]);
        let mut app = chapters(progress);
        reading(&mut app, 5, 20);
        assert_eq!(reader(&app).page, 19);
    }

    #[test]
    fn page_turn_saves_progress_and_prefetches() {
        let mut app = chapters(read([]));
        assert_eq!(fetched(&reading(&mut app, 112, 38)), [0, 1, 2, 3]);
        let effects = app.update(key(KeyCode::Right));
        assert_eq!(
            effects[0],
            Effect::SavePage {
                ch: n(112),
                source: SourceId::MangaKatana,
                page: 1,
                page_count: 38
            }
        );
        assert_eq!(fetched(&effects), [4]);
        assert_eq!(
            app.progress[&n(112)],
            Progress {
                updated_at: app.progress[&n(112)].updated_at,
                ..in_progress(1, 38, SourceId::MangaKatana)
            },
            "the chapter list shows the new resume page"
        );

        for code in [KeyCode::Char('l'), KeyCode::Char(' '), KeyCode::PageDown] {
            app.update(key(code));
        }
        assert_eq!(reader(&app).page, 4);
        for code in [KeyCode::Left, KeyCode::Char('h'), KeyCode::PageUp] {
            assert!(fetched(&app.update(key(code))).is_empty(), "already loaded");
        }
        assert_eq!(reader(&app).page, 1);
        app.update(key(KeyCode::Left));
        assert!(
            app.update(key(KeyCode::Left)).is_empty(),
            "nothing before page 1"
        );
    }

    #[test]
    fn keeps_at_most_8_decoded_pages() {
        let mut app = chapters(read([]));
        reading(&mut app, 1, 30);
        for i in 0..20 {
            app.update(AppEvent::PageLoaded(i, Ok(page_image())));
            app.update(key(KeyCode::Right));
        }
        let r = reader(&app);
        assert_eq!(r.page, 20);
        let ready: Vec<usize> = (0..30)
            .filter(|i| matches!(r.images.get(i), Some(PageImage::Ready(_))))
            .collect();
        assert_eq!(ready, (12..20).collect::<Vec<_>>(), "nearest pages kept");
    }

    #[test]
    fn late_page_for_another_chapter_is_ignored() {
        let mut app = chapters(read([]));
        reading(&mut app, 1, 3);
        app.update(key(KeyCode::Char('q')));
        assert_eq!(app.screen, Screen::Chapters);
        assert!(app.reader.is_none());
        app.update(AppEvent::PageLoaded(0, Ok(page_image())));
        app.update(AppEvent::PagesListed(n(1), Ok(pages(3))));
        assert!(app.reader.is_none());

        reading(&mut app, 2, 3);
        assert!(
            app.update(AppEvent::PagesListed(n(1), Ok(pages(9))))
                .is_empty(),
            "pages of Ch 1 don't replace Ch 2's"
        );
        assert_eq!(
            reader(&app).pages.as_ref().unwrap().as_ref().unwrap().len(),
            3
        );
    }

    #[test]
    fn past_last_page_marks_read_and_prompts() {
        let mut app = chapters(read(1..=111));
        reading(&mut app, 112, 2);
        app.update(key(KeyCode::Right));
        let effects = app.update(key(KeyCode::Right));
        assert!(
            effects.contains(&Effect::SetRead(n(112), true)),
            "{effects:?}"
        );
        assert_eq!(app.screen, Screen::EndOfChapter);
        assert!(app.is_read(n(112)));
        assert_eq!(app.next_chapter().map(|c| c.number), Some(n(113)));

        app.update(key(KeyCode::Left));
        assert_eq!(app.screen, Screen::Reader, "back to the last page");
        assert_eq!(reader(&app).page, 1);
        app.update(key(KeyCode::Right));
        app.update(key(KeyCode::Char('l')));
        assert_eq!(app.screen, Screen::Chapters);
        assert_eq!(current(&app), n(112));
        assert!(app.reader.is_none());
    }

    #[test]
    fn q_on_end_prompt_quits() {
        let mut app = chapters(read([]));
        reading(&mut app, 3, 1);
        app.update(key(KeyCode::Right));
        assert_eq!(app.update(key(KeyCode::Char('q'))), [Effect::Quit]);
    }

    #[test]
    fn enter_on_prompt_opens_next() {
        let mut app = chapters(read(1..=111));
        reading(&mut app, 112, 1);
        app.update(key(KeyCode::Right));
        let next = chapter_on(&app, SourceId::MangaKatana, 113);
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Effect::ListPages(SourceId::MangaKatana, next)]
        );
        assert_eq!(app.screen, Screen::Reader);
        assert_eq!(reader(&app).ch.number, n(113));
        assert_eq!(reader(&app).pages, None, "listing");
        assert_eq!(current(&app), n(113));
    }

    #[test]
    fn next_missing_offers_other_source() {
        let mut app = chapters_on_mangadex(read([]));
        reading(&mut app, 44, 1);
        app.update(key(KeyCode::Right));
        assert_eq!(app.screen, Screen::EndOfChapter);
        assert_eq!(
            app.next_chapter().map(|c| (c.number, c.source)),
            Some((n(45), SourceId::MangaKatana))
        );
        assert!(app.update(key(KeyCode::Enter)).is_empty(), "never silently");
        assert_eq!(app.screen, Screen::SwitchSource);
        let ch45 = chapter_on(&app, SourceId::MangaKatana, 45);
        assert_eq!(app.switch_to.as_ref(), Some(&ch45));
        assert_eq!(
            app.update(key(KeyCode::Char('y'))),
            [Effect::ListPages(SourceId::MangaKatana, ch45)]
        );
        assert_eq!(app.screen, Screen::Reader);
        assert_eq!(app.chosen.as_ref().unwrap().1, SourceId::MangaKatana);
        assert_eq!(current(&app), n(45));

        let mut app = chapters_on_mangadex(read([]));
        reading(&mut app, 44, 1);
        app.update(key(KeyCode::Right));
        app.update(key(KeyCode::Enter));
        assert!(app.update(key(KeyCode::Char('n'))).is_empty());
        assert_eq!(app.screen, Screen::Chapters);
        assert_eq!(app.chosen.as_ref().unwrap().1, SourceId::MangaDex);
        assert_eq!(current(&app), n(44));
    }

    #[test]
    fn no_next_chapter_disables_enter() {
        let mut app = chapters(read([]));
        reading(&mut app, 201, 1);
        app.update(key(KeyCode::Right));
        assert_eq!(app.screen, Screen::EndOfChapter);
        assert!(app.next_chapter().is_none());
        assert!(app.update(key(KeyCode::Enter)).is_empty());
        assert_eq!(app.screen, Screen::EndOfChapter);
    }

    #[test]
    fn r_retries_failed_page_and_listing() {
        let mut app = chapters(read([]));
        reading(&mut app, 1, 5);
        app.update(key(KeyCode::Right));
        app.update(key(KeyCode::Right));
        app.update(AppEvent::PageLoaded(2, Err(anyhow::anyhow!("HTTP 403"))));
        app.update(AppEvent::PageLoaded(3, Ok(page_image())));
        assert!(matches!(
            reader(&app).images.get(&2),
            Some(PageImage::Failed(e)) if e == "HTTP 403"
        ));
        let ch1 = app.chapters()[0].clone();
        assert_eq!(
            app.update(key(KeyCode::Char('r'))),
            [Effect::ListPages(SourceId::MangaKatana, ch1)],
            "page URLs may have expired: list them again"
        );
        assert_eq!(reader(&app).pages, None);
        assert!(app.update(key(KeyCode::Char('r'))).is_empty(), "listing");
        let effects = app.update(AppEvent::PagesListed(n(1), Ok(pages(5))));
        assert_eq!(reader(&app).page, 2, "stays on the page");
        assert_eq!(fetched(&effects), [2, 4], "page 3 is still decoded");
        assert!(app.update(key(KeyCode::Char('r'))).is_empty(), "loading");

        app.update(key(KeyCode::Char('q')));
        app.cursor = 1;
        let ch = app.chapters()[1].clone();
        app.update(key(KeyCode::Enter));
        app.update(AppEvent::PagesListed(
            n(2),
            Err(anyhow::anyhow!("timed out")),
        ));
        assert_eq!(reader(&app).pages, Some(Err("timed out".to_string())));
        assert!(app.update(key(KeyCode::Right)).is_empty());
        assert_eq!(
            app.update(key(KeyCode::Char('r'))),
            [Effect::ListPages(SourceId::MangaKatana, ch)]
        );
    }

    #[test]
    fn f_toggles_fit_and_arrows_scroll() {
        let mut app = chapters(read([]));
        reading(&mut app, 1, 5);
        assert_eq!(app.fit, None);
        assert_eq!(reader(&app).fit(app.fit), Fit::Height);
        app.update(key(KeyCode::Char('f')));
        assert_eq!(app.fit, Some(Fit::Width));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        let scrolled = reader(&app).scroll;
        assert!(scrolled > 0);
        app.update(key(KeyCode::Up));
        assert!(reader(&app).scroll < scrolled);
        app.update(key(KeyCode::Right));
        assert_eq!(reader(&app).scroll, 0, "a new page starts at the top");
        app.update(key(KeyCode::Char('f')));
        assert_eq!(app.fit, Some(Fit::Height));
    }

    fn sized_page(width: u32, height: u32) -> Arc<DynamicImage> {
        Arc::new(DynamicImage::new_rgb8(width, height))
    }

    #[test]
    fn long_strips_fit_to_width_until_f_picks_a_fit() {
        let mut app = chapters(read([]));
        reading(&mut app, 1, 5);
        app.update(AppEvent::PageLoaded(0, Ok(sized_page(800, 1200))));
        assert_eq!(reader(&app).fit(app.fit), Fit::Height);
        app.update(AppEvent::PageLoaded(1, Ok(sized_page(800, 8000))));
        app.update(key(KeyCode::Right));
        assert_eq!(reader(&app).fit(app.fit), Fit::Width);
        app.update(key(KeyCode::Char('f')));
        assert_eq!(app.fit, Some(Fit::Height), "f shows the whole strip");
    }

    #[test]
    fn every_page_key_scrolls_a_strip_before_turning_the_page() {
        for (next, back) in [
            (KeyCode::Char(' '), KeyCode::PageUp),
            (KeyCode::PageDown, KeyCode::PageUp),
            (KeyCode::Right, KeyCode::Left),
            (KeyCode::Char('l'), KeyCode::Char('h')),
        ] {
            let mut app = chapters(read([]));
            reading(&mut app, 1, 5);
            let r = app.reader.as_mut().unwrap();
            (r.view_rows, r.max_scroll) = (40, 50);
            app.update(key(next));
            assert_eq!(
                (reader(&app).page, reader(&app).scroll),
                (0, 35),
                "{next:?}"
            );
            app.update(key(next));
            assert_eq!(
                (reader(&app).page, reader(&app).scroll),
                (0, 50),
                "{next:?}"
            );
            app.update(key(back));
            assert_eq!(
                (reader(&app).page, reader(&app).scroll),
                (0, 15),
                "{back:?}"
            );
            app.update(key(next));
            app.update(key(next));
            assert_eq!((reader(&app).page, reader(&app).scroll), (1, 0), "{next:?}");
            app.update(key(back));
            assert_eq!(reader(&app).page, 0, "{back:?}");
            assert_eq!(reader(&app).scroll, u16::MAX, "back at the bottom");
        }
    }

    #[test]
    fn o_in_reader_opens_chapter_in_viewer() {
        let mut app = chapters(read([]));
        reading(&mut app, 7, 5);
        let ch = chapter_on(&app, SourceId::MangaKatana, 7);
        assert_eq!(
            app.update(key(KeyCode::Char('o'))),
            [Effect::Download {
                source: SourceId::MangaKatana,
                ch,
                then_open: true
            }]
        );
        assert_eq!(app.screen, Screen::Reader);
    }

    #[test]
    fn read_start_opens_requested_chapter() {
        let group = outcome().groups.remove(0);
        let (mut app, _) = App::new(
            Start::Read {
                group: group.clone(),
                source: SourceId::MangaKatana,
                chapter: Some(n(112)),
            },
            ReaderMode::Terminal,
        );
        let mut progress = read([]);
        progress.insert(n(112), in_progress(13, 38, SourceId::MangaKatana));
        let effects = app.update(AppEvent::Progress(progress));
        assert!(
            matches!(&effects[..], [Effect::ListPages(SourceId::MangaKatana, ch)] if ch.number == n(112))
        );
        app.update(AppEvent::PagesListed(n(112), Ok(pages(38))));
        assert_eq!(reader(&app).page, 13);

        // A chapter the source lacks: same prompt as at the end of a chapter.
        let (mut app, _) = App::new(
            Start::Read {
                group,
                source: SourceId::MangaDex,
                chapter: Some(n(50)),
            },
            ReaderMode::Terminal,
        );
        assert!(app.update(AppEvent::Progress(read([]))).is_empty());
        assert_eq!(app.screen, Screen::SwitchSource);
        assert_eq!(
            app.switch_to.as_ref().map(|c| (c.number, c.source)),
            Some((n(50), SourceId::MangaKatana))
        );
    }

    #[test]
    fn read_start_opens_chapter_list() {
        let group = outcome().groups.remove(0);
        let (app, effects) = App::new(
            Start::Read {
                group,
                source: SourceId::MangaDex,
                chapter: None,
            },
            ReaderMode::Terminal,
        );
        assert_eq!(app.screen, Screen::Chapters);
        assert_eq!(app.chosen.as_ref().unwrap().1, SourceId::MangaDex);
        assert!(matches!(effects[..], [Effect::LoadProgress { .. }]));
    }

    #[test]
    fn read_start_quits_from_chapter_list_after_leaving_reader() {
        for exit_key in [KeyCode::Esc, KeyCode::Char('q')] {
            let group = outcome().groups.remove(0);
            let (mut app, _) = App::new(
                Start::Read {
                    group,
                    source: SourceId::MangaKatana,
                    chapter: Some(n(1)),
                },
                ReaderMode::Terminal,
            );

            assert!(matches!(
                app.update(AppEvent::Progress(read([]))).as_slice(),
                [Effect::ListPages(SourceId::MangaKatana, ch)] if ch.number == n(1)
            ));
            app.update(AppEvent::PagesListed(n(1), Ok(pages(2))));
            assert_eq!(app.screen, Screen::Reader);

            assert!(app.update(key(KeyCode::Esc)).is_empty());
            assert_eq!(app.screen, Screen::Chapters);
            assert_eq!(app.update(key(exit_key)), [Effect::Quit]);
        }
    }
}
