use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::Start;
use crate::commands::distinct_failures;
use crate::db::Progress;
use crate::model::{ChapterNo, ChapterRef, PageRef, SourceId};
use crate::search::{SearchOutcome, TitleGroup};

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
    Progress(HashMap<ChapterNo, Progress>),
    PagesListed(ChapterNo, anyhow::Result<Vec<PageRef>>),
    PageLoaded(usize, anyhow::Result<Vec<u8>>),
    Downloaded(ChapterNo, anyhow::Result<PathBuf>),
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Search(String),
    LoadProgress {
        key: String,
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
    pub(super) search_scroll: usize,
    pub(super) chapter_scroll: usize,
}

const PAGE_STEP: usize = 10;

impl App {
    pub fn new(start: Start) -> (App, Vec<Effect>) {
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
            search_scroll: 0,
            chapter_scroll: 0,
        };
        let effects = match start {
            Start::Search => Vec::new(),
            Start::Read { group, source, .. } => app.open(group, source),
        };
        (app, effects)
    }

    pub fn update(&mut self, ev: AppEvent) -> Vec<Effect> {
        match ev {
            AppEvent::Key(key) if key.kind == KeyEventKind::Press => return self.key(key),
            AppEvent::SearchDone(outcome) => self.search_done(outcome),
            AppEvent::Progress(progress) => {
                self.progress = progress;
                self.cursor = self.first_unread();
                self.chapter_scroll = self.cursor.saturating_sub(2);
            }
            AppEvent::Downloaded(n, Ok(path)) => {
                self.downloaded.insert(n);
                match self.downloading.remove(&n) {
                    Some(true) => self.status = Some(format!("opened Ch {n}")),
                    Some(false) => self.status = Some(format!("Ch {n} → {}", path.display())),
                    None => {}
                }
            }
            AppEvent::Downloaded(n, Err(e)) => {
                self.downloading.remove(&n);
                self.status = Some(format!("couldn't download Ch {n}: {e:#}"));
            }
            AppEvent::Error(msg) => self.status = Some(msg),
            AppEvent::Key(_)
            | AppEvent::Resize(..)
            | AppEvent::PagesListed(..)
            | AppEvent::PageLoaded(..) => {}
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

    fn key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return vec![Effect::Quit];
        }
        match self.screen {
            Screen::Search if self.editing => self.query_key(key),
            Screen::Search => self.results_key(key),
            Screen::Chapters => self.chapters_key(key),
            Screen::Reader | Screen::EndOfChapter | Screen::SwitchSource => Vec::new(),
        }
    }

    fn query_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Enter => return self.run_search(),
            KeyCode::Esc => self.editing = false,
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Up => self.move_group(-1),
            KeyCode::Down => self.move_group(1),
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c)
            }
            _ => {}
        }
        Vec::new()
    }

    fn results_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('q') => return vec![Effect::Quit],
            KeyCode::Enter => return self.open_selected(),
            KeyCode::Char('/') => {
                self.editing = true;
                self.query.clear();
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
            key: group.key.clone(),
            title: group.title.clone(),
        };
        self.screen = Screen::Chapters;
        self.chosen = Some((group, source));
        self.progress.clear();
        self.downloaded.clear();
        self.downloading.clear();
        self.cursor = 0;
        self.chapter_scroll = 0;
        self.status = None;
        vec![effect]
    }

    fn first_unread(&self) -> usize {
        let chapters = self.chapters();
        chapters
            .iter()
            .position(|c| !self.is_read(c.number))
            .unwrap_or(chapters.len().saturating_sub(1))
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
            KeyCode::Esc => {
                self.screen = Screen::Search;
                self.status = None;
            }
            KeyCode::Char('s') => self.switch_source(),
            KeyCode::Char('m') => return self.toggle_read(),
            KeyCode::Char('d') => return self.download(false),
            KeyCode::Char('o') => return self.download(true),
            KeyCode::Enter => self.status = Some("reader coming soon".into()),
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
            .unwrap_or_else(|| self.first_unread());
    }

    fn toggle_read(&mut self) -> Vec<Effect> {
        let Some(n) = self.chapters().get(self.cursor).map(|c| c.number) else {
            return Vec::new();
        };
        let p = self.progress.entry(n).or_insert(Progress {
            read: false,
            last_page: None,
            page_count: None,
            source: None,
        });
        p.read = !p.read;
        if !p.read {
            p.last_page = None;
        }
        vec![Effect::SetRead(n, p.read)]
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
            key: crate::model::normalize_title(title),
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
        let (mut app, _) = App::new(Start::Search);
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
        let (mut app, effects) = App::new(Start::Search);
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
                key: "solo leveling".into(),
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
    fn cursor_starts_at_first_unread() {
        let app = chapters(read(1..=3));
        assert_eq!(current(&app), n(4));

        let app = chapters(read(1..=201));
        assert_eq!(current(&app), n(201), "all read: last chapter");
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
        assert_eq!(current(&app), n(0), "falls back to first unread");
    }

    #[test]
    fn q_in_query_box_types_q() {
        let (mut app, _) = App::new(Start::Search);
        assert!(app.update(key(KeyCode::Char('q'))).is_empty());
        assert_eq!(app.query, "q");
        app.update(key(KeyCode::Esc));
        assert_eq!(app.update(key(KeyCode::Char('q'))), [Effect::Quit]);
    }

    #[test]
    fn ctrl_c_quits_while_typing() {
        let (mut app, _) = App::new(Start::Search);
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
        let (mut app, _) = App::new(Start::Search);
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
    fn read_start_opens_chapter_list() {
        let group = outcome().groups.remove(0);
        let (app, effects) = App::new(Start::Read {
            group,
            source: SourceId::MangaDex,
            chapter: None,
        });
        assert_eq!(app.screen, Screen::Chapters);
        assert_eq!(app.chosen.as_ref().unwrap().1, SourceId::MangaDex);
        assert!(matches!(effects[..], [Effect::LoadProgress { .. }]));
    }
}
