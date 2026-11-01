pub mod cbz;
pub mod pdf;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, ensure};
use futures::{StreamExt, TryStreamExt};

use crate::config::Format;
use crate::model::{ChapterNo, ChapterRef};
use crate::sources::Source;

const MAX_NAME_CHARS: usize = 120;
const PAGES_IN_FLIGHT: usize = 4;
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// File-name-safe on every OS (Windows rules), so paths match across machines.
pub fn sanitize(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| {
            if c.is_control() || r#"/\:*?"<>|"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let unreserved = escape_reserved(&collapsed);
    let capped: String = unreserved.chars().take(MAX_NAME_CHARS).collect();
    let trimmed = capped.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Windows refuses device names (`CON`, `com1.txt`, …) as file stems; append `_` to the stem.
fn escape_reserved(name: &str) -> String {
    let stem = name.split('.').next().unwrap_or_default().trim_end();
    let upper = stem.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        format!("{stem}_{}", &name[stem.len()..])
    } else {
        name.to_string()
    }
}

pub fn chapter_path(
    download_dir: &Path,
    series_title: &str,
    ch: ChapterNo,
    fmt: Format,
) -> PathBuf {
    let series = sanitize(series_title);
    let stem = format!("{series} - Ch {ch}");
    let name = match fmt {
        Format::Pdf => format!("{stem}.pdf"),
        Format::Cbz => format!("{stem}.cbz"),
        Format::Images => stem,
    };
    download_dir.join(series).join(name)
}

/// The chapter's file on disk in any format, preferring `preferred`.
pub fn existing_download(
    download_dir: &Path,
    series_title: &str,
    ch: ChapterNo,
    preferred: Format,
) -> Option<PathBuf> {
    [preferred, Format::Pdf, Format::Cbz, Format::Images]
        .into_iter()
        .map(|fmt| chapter_path(download_dir, series_title, ch, fmt))
        .find(|path| path.exists())
}

/// Downloads every page of `ch` in order, a few at a time, retrying each page once.
/// `on_progress(done, total)` runs after each page.
pub async fn fetch_pages(
    source: &dyn Source,
    ch: &ChapterRef,
    on_progress: impl Fn(usize, usize),
) -> anyhow::Result<Vec<Vec<u8>>> {
    let pages = source
        .pages(ch)
        .await
        .with_context(|| format!("listing pages of chapter {}", ch.number))?;
    let total = pages.len();
    let done = AtomicUsize::new(0);
    let (done, on_progress) = (&done, &on_progress);
    futures::stream::iter(pages.iter().enumerate())
        .map(|(i, page)| async move {
            let bytes = match source.fetch_page(page).await {
                Ok(bytes) => bytes,
                Err(first) => {
                    tokio::time::sleep(RETRY_DELAY).await;
                    source.fetch_page(page).await.with_context(|| {
                        format!(
                            "downloading page {} of {total} ({}); first attempt: {first:#}",
                            i + 1,
                            page.url
                        )
                    })?
                }
            };
            on_progress(done.fetch_add(1, Ordering::SeqCst) + 1, total);
            Ok(bytes)
        })
        .buffered(PAGES_IN_FLIGHT)
        .try_collect()
        .await
}

/// Writes to a `.part` sibling and renames it into place, so a failed write
/// never leaves anything that looks downloaded.
pub fn write_chapter(pages: &[Vec<u8>], fmt: Format, dest: &Path) -> anyhow::Result<()> {
    ensure!(!pages.is_empty(), "chapter has no pages");
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let part = part_path(dest);
    remove_path(&part);
    let result = match fmt {
        Format::Pdf => pdf::write_pdf(pages, &part),
        Format::Cbz => cbz::write_cbz(pages, &part),
        Format::Images => write_images(pages, &part),
    }
    .and_then(|()| {
        if fmt == Format::Images && dest.is_dir() {
            fs::remove_dir_all(dest).with_context(|| format!("replacing {}", dest.display()))?;
        }
        fs::rename(&part, dest).with_context(|| format!("moving into {}", dest.display()))
    });
    if result.is_err() {
        remove_path(&part);
    }
    result.with_context(|| format!("writing {}", dest.display()))
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().map(OsString::from).unwrap_or_default();
    name.push(".part");
    dest.with_file_name(name)
}

fn remove_path(path: &Path) {
    let _ = if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
}

fn write_images(pages: &[Vec<u8>], dir: &Path) -> anyhow::Result<()> {
    fs::create_dir(dir)?;
    for (i, bytes) in pages.iter().enumerate() {
        fs::write(dir.join(page_name(i, bytes)?), bytes)?;
    }
    Ok(())
}

/// `001.jpg`, `002.png`, … with the extension taken from the image bytes.
fn page_name(index: usize, bytes: &[u8]) -> anyhow::Result<String> {
    let format = image::guess_format(bytes)
        .with_context(|| format!("page {} is not a recognised image", index + 1))?;
    let ext = format.extensions_str().first().copied().unwrap_or("img");
    Ok(format!("{:03}.{ext}", index + 1))
}

#[cfg(test)]
pub(crate) mod test_images {
    use std::io::Cursor;

    use image::{DynamicImage, GrayImage, ImageFormat, RgbImage};

    fn encode(img: DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    pub fn jpeg_rgb() -> Vec<u8> {
        let img = RgbImage::from_fn(40, 60, |x, y| image::Rgb([x as u8 * 6, y as u8 * 4, 128]));
        encode(DynamicImage::ImageRgb8(img), ImageFormat::Jpeg)
    }

    pub fn jpeg_gray() -> Vec<u8> {
        let img = GrayImage::from_fn(30, 20, |x, _| image::Luma([x as u8 * 8]));
        encode(DynamicImage::ImageLuma8(img), ImageFormat::Jpeg)
    }

    pub fn png() -> Vec<u8> {
        let img = RgbImage::from_fn(50, 70, |x, y| image::Rgb([x as u8, y as u8, 0]));
        encode(DynamicImage::ImageRgb8(img), ImageFormat::Png)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::model::{MangaRef, PageRef, SourceId};

    fn ch(s: &str) -> ChapterNo {
        ChapterNo::parse(s).unwrap()
    }

    #[test]
    fn sanitize_windows_illegal() {
        assert_eq!(sanitize("Re:Zero"), "Re_Zero");
        assert_eq!(sanitize("Fate/Zero"), "Fate_Zero");
        assert_eq!(sanitize("What?. "), "What_");
        assert_eq!(sanitize("   "), "untitled");
        assert_eq!(sanitize(r#"a\b*c"d<e>f|g"#), "a_b_c_d_e_f_g");
        assert_eq!(sanitize("  Solo   Leveling\u{7}  "), "Solo Leveling_");
        assert_eq!(sanitize("...").as_str(), "untitled");
        assert_eq!(sanitize(&"x".repeat(200)).chars().count(), 120);
    }

    #[test]
    fn sanitize_windows_reserved_names() {
        assert_eq!(sanitize("CON"), "CON_");
        assert_eq!(sanitize("nul"), "nul_");
        assert_eq!(sanitize("Com1.x"), "Com1_.x");
        assert_eq!(sanitize("LPT9 ."), "LPT9_");
        assert_eq!(sanitize("AUX..."), "AUX_");
        assert_eq!(sanitize("COM0"), "COM0");
        assert_eq!(sanitize("CONAN"), "CONAN");
        assert_eq!(sanitize("Con Man"), "Con Man");
    }

    #[test]
    fn chapter_path_layout() {
        let dir = Path::new("/d");
        assert_eq!(
            chapter_path(dir, "Re:Zero", ch("10.5"), Format::Pdf),
            Path::new("/d/Re_Zero/Re_Zero - Ch 10.5.pdf")
        );
        assert_eq!(
            chapter_path(dir, "Re:Zero", ch("112"), Format::Cbz),
            Path::new("/d/Re_Zero/Re_Zero - Ch 112.cbz")
        );
        assert_eq!(
            chapter_path(dir, "Re:Zero", ch("10.5"), Format::Images),
            Path::new("/d/Re_Zero/Re_Zero - Ch 10.5")
        );
    }

    #[test]
    fn no_partial_file_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let pages = vec![test_images::jpeg_rgb(), b"not an image".to_vec()];
        for (fmt, name) in [
            (Format::Pdf, "c.pdf"),
            (Format::Cbz, "c.cbz"),
            (Format::Images, "c"),
        ] {
            let dest = dir.path().join("series").join(name);
            assert!(write_chapter(&pages, fmt, &dest).is_err(), "{fmt:?}");
            assert!(!dest.exists(), "{fmt:?}");
        }
        let left: Vec<_> = std::fs::read_dir(dir.path().join("series"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn write_chapter_images_folder() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("s").join("s - Ch 1");
        let pages = vec![test_images::jpeg_rgb(), test_images::png()];
        write_chapter(&pages, Format::Images, &dest).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["001.jpg", "002.png"]);
        assert_eq!(std::fs::read(dest.join("002.png")).unwrap(), pages[1]);
    }

    #[test]
    fn existing_download_finds_any_format_preferring_configured() {
        let dir = tempfile::tempdir().unwrap();
        let find = |n: &str, preferred| existing_download(dir.path(), "Re:Zero", ch(n), preferred);
        assert_eq!(find("3", Format::Pdf), None);
        let cbz = chapter_path(dir.path(), "Re:Zero", ch("3"), Format::Cbz);
        write_chapter(&[test_images::jpeg_rgb()], Format::Cbz, &cbz).unwrap();
        assert_eq!(find("3", Format::Images), Some(cbz.clone()));
        assert_eq!(find("4", Format::Cbz), None);

        let pdf = chapter_path(dir.path(), "Re:Zero", ch("3"), Format::Pdf);
        write_chapter(&[test_images::jpeg_rgb()], Format::Pdf, &pdf).unwrap();
        assert_eq!(find("3", Format::Cbz), Some(cbz));
        assert_eq!(find("3", Format::Images), Some(pdf));
    }

    struct FakeSource {
        pages: usize,
        failed_once: AtomicBool,
    }

    #[async_trait::async_trait]
    impl Source for FakeSource {
        fn id(&self) -> SourceId {
            SourceId::MangaDex
        }
        async fn search(&self, _: &str) -> anyhow::Result<Vec<MangaRef>> {
            unimplemented!()
        }
        async fn chapters(&self, _: &MangaRef) -> anyhow::Result<Vec<ChapterRef>> {
            unimplemented!()
        }
        async fn pages(&self, _: &ChapterRef) -> anyhow::Result<Vec<PageRef>> {
            Ok((0..self.pages)
                .map(|i| PageRef { url: i.to_string() })
                .collect())
        }
        async fn fetch_page(&self, page: &PageRef) -> anyhow::Result<Vec<u8>> {
            let i: u64 = page.url.parse().unwrap();
            // Later pages finish first; page 2 fails on its first attempt.
            tokio::time::sleep(Duration::from_millis((7 * (13 - i)) % 40)).await;
            if i == 2 && !self.failed_once.swap(true, Ordering::SeqCst) {
                anyhow::bail!("transient");
            }
            Ok(page.url.clone().into_bytes())
        }
    }

    fn chapter_ref() -> ChapterRef {
        ChapterRef {
            source: SourceId::MangaDex,
            id: "c".into(),
            number: ch("1"),
            title: None,
            published: None,
        }
    }

    #[tokio::test]
    async fn fetch_pages_keeps_order() {
        let source = FakeSource {
            pages: 10,
            failed_once: AtomicBool::new(false),
        };
        let calls = Mutex::new(Vec::new());
        let pages = fetch_pages(&source, &chapter_ref(), |done, total| {
            calls.lock().unwrap().push((done, total))
        })
        .await
        .unwrap();
        let expected: Vec<Vec<u8>> = (0..10).map(|i| i.to_string().into_bytes()).collect();
        assert_eq!(pages, expected);
        let calls = calls.into_inner().unwrap();
        assert_eq!(calls, (1..=10).map(|d| (d, 10)).collect::<Vec<_>>());
        assert!(source.failed_once.load(Ordering::SeqCst));
    }
}
