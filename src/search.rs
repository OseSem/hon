use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use futures::future::join_all;

use crate::model::{ChapterNo, ChapterRef, MangaRef, SourceId, normalize_title};
use crate::sources::Source;

const SOURCE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_GROUPS: usize = 8;

#[derive(Clone, Debug)]
pub struct SourceSummary {
    pub manga: MangaRef,
    pub chapters: Vec<ChapterRef>,
    pub latest: Option<ChapterNo>,
    pub gaps: Vec<(u32, u32)>,
    pub updated: Option<jiff::Timestamp>,
}

impl SourceSummary {
    fn new(manga: MangaRef, chapters: Vec<ChapterRef>) -> SourceSummary {
        SourceSummary {
            latest: chapters.iter().map(|c| c.number).max(),
            gaps: find_gaps(&chapters),
            updated: chapters.iter().filter_map(|c| c.published).max(),
            manga,
            chapters,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TitleGroup {
    pub key: String,
    pub title: String,
    /// Sorted by `latest` descending.
    pub sources: Vec<SourceSummary>,
}

impl TitleGroup {
    pub fn source(&self, id: SourceId) -> Option<&SourceSummary> {
        self.sources.iter().find(|s| s.manga.source == id)
    }

    pub fn best_source(&self) -> SourceId {
        self.sources[0].manga.source
    }
}

#[derive(Debug)]
pub struct SearchOutcome {
    pub groups: Vec<TitleGroup>,
    pub failures: Vec<(SourceId, String)>,
}

async fn timed<T>(fut: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(SOURCE_TIMEOUT, fut)
        .await
        .unwrap_or_else(|_| Err(anyhow!("timed out after {}s", SOURCE_TIMEOUT.as_secs())))
}

pub async fn search_all(sources: &[Arc<dyn Source>], query: &str) -> SearchOutcome {
    let mut failures = Vec::new();
    let mut results = Vec::new();
    let searches = join_all(
        sources
            .iter()
            .map(|s| async move { (s.id(), timed(s.search(query)).await) }),
    )
    .await;
    for (id, res) in searches {
        match res {
            Ok(found) => results.extend(found),
            Err(e) => failures.push((id, format!("{e:#}"))),
        }
    }

    let mut grouped = group(results);
    rank_by_query(&mut grouped, query);
    grouped.truncate(MAX_GROUPS);

    let fetches = grouped
        .iter()
        .enumerate()
        .flat_map(|(gi, (_, _, members))| {
            members.iter().filter_map(move |m| {
                let src = sources.iter().find(|s| s.id() == m.source)?;
                Some(async move { (gi, m, timed(src.chapters(m)).await) })
            })
        });
    let mut summaries: Vec<Vec<SourceSummary>> = vec![Vec::new(); grouped.len()];
    for (gi, manga, res) in join_all(fetches).await {
        match res {
            Ok(chapters) => summaries[gi].push(SourceSummary::new(manga.clone(), chapters)),
            Err(e) => failures.push((manga.source, format!("{e:#}"))),
        }
    }

    let groups = grouped
        .into_iter()
        .zip(summaries)
        .filter(|(_, sources)| !sources.is_empty())
        .map(|((key, title, _), mut sources)| {
            sources.sort_by_key(|s| Reverse(s.latest));
            TitleGroup {
                key,
                title,
                sources,
            }
        })
        .collect();
    SearchOutcome { groups, failures }
}

fn title_keys(m: &MangaRef) -> impl Iterator<Item = String> + '_ {
    std::iter::once(&m.title)
        .chain(&m.alt_titles)
        .map(|t| normalize_title(t))
        .filter(|k| !k.is_empty())
}

/// Groups results (in source registration order) into `(key, display title, members)`,
/// ordered by first appearance.
pub fn group(results: Vec<MangaRef>) -> Vec<(String, String, Vec<MangaRef>)> {
    let mut groups: Vec<(BTreeSet<String>, Vec<MangaRef>)> = Vec::new();
    for m in results {
        let keys: BTreeSet<String> = title_keys(&m).collect();
        let existing = groups.iter_mut().find(|(gkeys, members)| {
            members.iter().all(|o| o.source != m.source) && !gkeys.is_disjoint(&keys)
        });
        match existing {
            Some((gkeys, members)) => {
                gkeys.extend(keys);
                members.push(m);
            }
            None => groups.push((keys, vec![m])),
        }
    }
    groups
        .into_iter()
        .map(|(_, members)| {
            let title = members[0].title.clone();
            (normalize_title(&title), title, members)
        })
        .collect()
}

/// Moves groups whose key equals the normalized query to the front, keeping order otherwise.
fn rank_by_query(groups: &mut [(String, String, Vec<MangaRef>)], query: &str) {
    let q = normalize_title(query);
    groups.sort_by_key(|(key, _, _)| *key != q);
}

/// Inclusive ranges of integer chapter numbers missing between the lowest and highest
/// integer chapter present. Fractional chapters are ignored.
pub fn find_gaps(chapters: &[ChapterRef]) -> Vec<(u32, u32)> {
    let ints: BTreeSet<u32> = chapters
        .iter()
        .map(|c| c.number.value())
        .filter(|v| v.fract() == 0.0)
        .map(|v| v as u32)
        .collect();
    ints.iter()
        .zip(ints.iter().skip(1))
        .filter(|(a, b)| **b - **a > 1)
        .map(|(a, b)| (a + 1, b - 1))
        .collect()
}

pub fn resolve<'a>(query: &str, groups: &'a [TitleGroup]) -> Result<&'a TitleGroup, Vec<String>> {
    let q = normalize_title(query);
    let mut exact = groups.iter().filter(|g| g.key == q);
    if let (Some(g), None) = (exact.next(), exact.next()) {
        return Ok(g);
    }
    if let [g] = groups {
        return Ok(g);
    }
    Err(groups.iter().map(|g| g.title.clone()).collect())
}

/// For each wanted chapter not on `on`, the first other source (in `group.sources` order)
/// that has it. Chapters no source has are left out.
pub fn missing_elsewhere(
    group: &TitleGroup,
    on: SourceId,
    wanted: &[ChapterNo],
) -> Vec<(ChapterNo, SourceId)> {
    let has = |s: &SourceSummary, n: ChapterNo| s.chapters.iter().any(|c| c.number == n);
    wanted
        .iter()
        .filter(|&&n| !group.source(on).is_some_and(|s| has(s, n)))
        .filter_map(|&n| {
            group
                .sources
                .iter()
                .find(|s| s.manga.source != on && has(s, n))
                .map(|s| (n, s.manga.source))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PageRef;
    use anyhow::anyhow;

    fn manga(source: SourceId, id: &str, title: &str, alts: &[&str]) -> MangaRef {
        MangaRef {
            source,
            id: id.into(),
            title: title.into(),
            alt_titles: alts.iter().map(|s| s.to_string()).collect(),
            url: format!("https://example.test/{id}"),
        }
    }

    fn chs_on(source: SourceId, nums: &[&str]) -> Vec<ChapterRef> {
        nums.iter()
            .map(|n| ChapterRef {
                source,
                id: format!("c{n}"),
                number: ChapterNo::parse(n).unwrap(),
                title: None,
                published: None,
            })
            .collect()
    }

    fn chs(nums: &[&str]) -> Vec<ChapterRef> {
        chs_on(SourceId::MangaDex, nums)
    }

    fn no(n: &str) -> ChapterNo {
        ChapterNo::parse(n).unwrap()
    }

    fn summary(m: MangaRef, nums: &[&str]) -> SourceSummary {
        let chapters = chs_on(m.source, nums);
        SourceSummary {
            latest: chapters.iter().map(|c| c.number).max(),
            gaps: Vec::new(),
            updated: None,
            manga: m,
            chapters,
        }
    }

    fn title_group(title: &str, sources: Vec<SourceSummary>) -> TitleGroup {
        TitleGroup {
            key: crate::model::normalize_title(title),
            title: title.into(),
            sources,
        }
    }

    #[test]
    fn groups_by_alt_title_across_sources() {
        let groups = group(vec![
            manga(
                SourceId::MangaDex,
                "md1",
                "Solo Leveling",
                &["Na Honjaman Level-Up"],
            ),
            manga(SourceId::MangaKatana, "mk1", "Na Honjaman Level Up", &[]),
            manga(SourceId::MangaKatana, "mk2", "Solo Leveling: Ragnarok", &[]),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "solo leveling");
        assert_eq!(groups[0].1, "Solo Leveling");
        let ids: Vec<_> = groups[0].2.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["md1", "mk1"]);
        assert_eq!(groups[1].1, "Solo Leveling: Ragnarok");
        assert_eq!(groups[1].2.len(), 1);
    }

    #[test]
    fn same_source_never_merged() {
        let groups = group(vec![
            manga(SourceId::MangaDex, "a", "Solo Leveling", &[]),
            manga(SourceId::MangaDex, "b", "Solo Leveling", &[]),
        ]);
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| g.2.len() == 1));
    }

    #[test]
    fn exact_match_ranks_first() {
        let mut groups = group(vec![
            manga(SourceId::MangaDex, "r", "Solo Leveling: Ragnarok", &[]),
            manga(SourceId::MangaDex, "s", "Solo Leveling", &[]),
        ]);
        rank_by_query(&mut groups, "solo leveling");
        assert_eq!(groups[0].1, "Solo Leveling");
        assert_eq!(groups[1].1, "Solo Leveling: Ragnarok");
    }

    #[test]
    fn gaps_found() {
        assert_eq!(
            find_gaps(&chs(&["1", "2", "5", "6", "6.5", "9"])),
            vec![(3, 4), (7, 8)]
        );
        assert_eq!(find_gaps(&chs(&["0.5", "1", "1.5", "2"])), vec![]);
        assert_eq!(find_gaps(&[]), vec![]);
    }

    #[test]
    fn resolve_exact_single_and_ambiguous() {
        let md = |id: &str, t: &str| manga(SourceId::MangaDex, id, t, &[]);
        let exact = [
            title_group("Solo Leveling: Ragnarok", vec![summary(md("r", "x"), &[])]),
            title_group("Solo Leveling", vec![summary(md("s", "x"), &[])]),
        ];
        assert_eq!(
            resolve("solo leveling", &exact).unwrap().title,
            "Solo Leveling"
        );

        let single = [title_group(
            "Solo Leveling: Ragnarok",
            vec![summary(md("r", "x"), &[])],
        )];
        assert_eq!(
            resolve("ragnarok", &single).unwrap().title,
            "Solo Leveling: Ragnarok"
        );

        let ambiguous = [
            title_group("Solo Leveling: Ragnarok", vec![summary(md("r", "x"), &[])]),
            title_group(
                "Solo Leveling: Side Story",
                vec![summary(md("s", "x"), &[])],
            ),
        ];
        assert_eq!(
            resolve("solo leveling", &ambiguous).unwrap_err(),
            ["Solo Leveling: Ragnarok", "Solo Leveling: Side Story"]
        );

        assert_eq!(resolve("anything", &[]).unwrap_err(), Vec::<String>::new());
    }

    #[test]
    fn missing_elsewhere_points_to_other_source() {
        let g = title_group(
            "Solo Leveling",
            vec![
                summary(
                    manga(SourceId::MangaDex, "md", "Solo Leveling", &[]),
                    &["44", "46"],
                ),
                summary(
                    manga(SourceId::MangaKatana, "mk", "Solo Leveling", &[]),
                    &["44", "45"],
                ),
            ],
        );
        assert_eq!(
            missing_elsewhere(&g, SourceId::MangaDex, &[no("44"), no("45"), no("47")]),
            vec![(no("45"), SourceId::MangaKatana)]
        );
    }

    struct Fake {
        id: SourceId,
        search: Result<Vec<MangaRef>, String>,
        chapters: Vec<&'static str>,
    }

    #[async_trait::async_trait]
    impl Source for Fake {
        fn id(&self) -> SourceId {
            self.id
        }

        async fn search(&self, _query: &str) -> anyhow::Result<Vec<MangaRef>> {
            self.search.clone().map_err(|e| anyhow!(e))
        }

        async fn chapters(&self, _manga: &MangaRef) -> anyhow::Result<Vec<ChapterRef>> {
            Ok(chs_on(self.id, &self.chapters))
        }

        async fn pages(&self, _chapter: &ChapterRef) -> anyhow::Result<Vec<PageRef>> {
            unimplemented!()
        }

        async fn fetch_page(&self, _page: &PageRef) -> anyhow::Result<Vec<u8>> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn one_failing_source_keeps_others() {
        let sources: Vec<Arc<dyn Source>> = vec![
            Arc::new(Fake {
                id: SourceId::MangaDex,
                search: Ok(vec![manga(SourceId::MangaDex, "md", "Solo Leveling", &[])]),
                chapters: vec!["1", "2"],
            }),
            Arc::new(Fake {
                id: SourceId::MangaKatana,
                search: Err("boom".into()),
                chapters: vec![],
            }),
        ];
        let out = search_all(&sources, "solo leveling").await;
        assert_eq!(out.groups.len(), 1);
        assert_eq!(out.groups[0].sources.len(), 1);
        assert_eq!(out.groups[0].sources[0].latest, Some(no("2")));
        assert_eq!(out.failures.len(), 1);
        assert_eq!(out.failures[0].0, SourceId::MangaKatana);
        assert!(out.failures[0].1.contains("boom"), "{:?}", out.failures);
    }

    #[tokio::test]
    async fn sources_sorted_by_latest() {
        let sources: Vec<Arc<dyn Source>> = vec![
            Arc::new(Fake {
                id: SourceId::MangaDex,
                search: Ok(vec![manga(SourceId::MangaDex, "md", "Solo Leveling", &[])]),
                chapters: vec!["1", "2", "40"],
            }),
            Arc::new(Fake {
                id: SourceId::MangaKatana,
                search: Ok(vec![manga(
                    SourceId::MangaKatana,
                    "mk",
                    "Solo Leveling",
                    &[],
                )]),
                chapters: vec!["1", "50"],
            }),
        ];
        let out = search_all(&sources, "solo leveling").await;
        assert!(out.failures.is_empty());
        assert_eq!(out.groups.len(), 1);
        let g = &out.groups[0];
        assert_eq!(g.title, "Solo Leveling");
        assert_eq!(g.sources.len(), 2);
        assert!(g.sources[0].latest >= g.sources[1].latest);
        assert_eq!(g.best_source(), SourceId::MangaKatana);
        assert_eq!(g.sources[1].gaps, vec![(3, 39)]);
    }
}
