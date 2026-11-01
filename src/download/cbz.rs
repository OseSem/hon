use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Pages are stored uncompressed (images are already compressed) as `001.ext`, `002.ext`, …
pub fn write_cbz(pages: &[Vec<u8>], dest: &Path) -> anyhow::Result<()> {
    let mut zip = ZipWriter::new(BufWriter::new(File::create(dest)?));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (i, bytes) in pages.iter().enumerate() {
        zip.start_file(super::page_name(i, bytes)?, options)?;
        zip.write_all(bytes)?;
    }
    zip.finish()?.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use zip::{CompressionMethod, ZipArchive};

    use super::*;
    use crate::download::test_images;

    #[test]
    fn cbz_has_ordered_entries() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("c.cbz");
        let pages = vec![
            test_images::jpeg_rgb(),
            test_images::jpeg_gray(),
            test_images::png(),
        ];
        write_cbz(&pages, &dest).unwrap();

        let mut zip = ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
        let names: Vec<_> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().unwrap().into_owned())
            .collect();
        assert_eq!(names, ["001.jpg", "002.jpg", "003.png"]);
        for (i, page) in pages.iter().enumerate() {
            let mut entry = zip.by_index(i).unwrap();
            assert_eq!(entry.compression(), CompressionMethod::Stored);
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            assert_eq!(&bytes, page);
        }
    }
}
