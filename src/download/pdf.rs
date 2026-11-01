use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, bail};
use image::{ImageFormat, ImageReader};
use lopdf::{Dictionary, Document, Object, Stream, dictionary};

/// One page per image, page size = image pixel size in points.
/// JPEGs are embedded as-is; other formats are stored losslessly as Flate-compressed RGB.
pub fn write_pdf(pages: &[Vec<u8>], dest: &Path) -> anyhow::Result<()> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut kids = Vec::with_capacity(pages.len());
    for (i, bytes) in pages.iter().enumerate() {
        let (image, width, height) =
            image_xobject(bytes).with_context(|| format!("page {}", i + 1))?;
        let image_id = doc.add_object(image);
        let content = format!("q {width} 0 0 {height} 0 0 cm /Im0 Do Q");
        let content_id = doc.add_object(Stream::new(Dictionary::new(), content.into_bytes()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), width.into(), height.into()],
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
            "Contents" => content_id,
        });
        kids.push(Object::Reference(page_id));
    }
    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    doc.save(dest)?;
    Ok(())
}

fn image_xobject(bytes: &[u8]) -> anyhow::Result<(Stream, u32, u32)> {
    let format = image::guess_format(bytes).context("not a recognised image")?;
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format).into_dimensions()?;
    let mut dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => width,
        "Height" => height,
        "BitsPerComponent" => 8,
    };
    if format == ImageFormat::Jpeg {
        match jpeg_components(bytes) {
            Some(1) => dict.set("ColorSpace", "DeviceGray"),
            Some(3) => dict.set("ColorSpace", "DeviceRGB"),
            Some(4) => {
                dict.set("ColorSpace", "DeviceCMYK");
                dict.set(
                    "Decode",
                    [1, 0, 1, 0, 1, 0, 1, 0].map(Object::from).to_vec(),
                );
            }
            Some(n) => bail!("unsupported JPEG with {n} colour components"),
            None => bail!("JPEG without a frame header"),
        }
        dict.set("Filter", "DCTDecode");
        let stream = Stream::new(dict, bytes.to_vec()).with_compression(false);
        return Ok((stream, width, height));
    }
    let rgb = image::load_from_memory_with_format(bytes, format)?.into_rgb8();
    dict.set("ColorSpace", "DeviceRGB");
    let mut stream = Stream::new(dict, rgb.into_raw());
    stream.compress()?;
    Ok((stream, width, height))
}

/// Component count from the JPEG's start-of-frame header.
fn jpeg_components(bytes: &[u8]) -> Option<u8> {
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let marker = bytes[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            return bytes.get(i + 9).copied();
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use lopdf::{Document, Object, Stream};

    use super::*;
    use crate::download::test_images;

    fn page_image(doc: &Document, page_no: u32) -> &Stream {
        let page_id = doc.get_pages()[&page_no];
        let page = doc.get_dictionary(page_id).unwrap();
        let resources = page
            .get_deref(b"Resources", doc)
            .unwrap()
            .as_dict()
            .unwrap();
        let xobjects = resources
            .get_deref(b"XObject", doc)
            .unwrap()
            .as_dict()
            .unwrap();
        let (_, image) = xobjects.iter().next().unwrap();
        doc.dereference(image).unwrap().1.as_stream().unwrap()
    }

    fn name<'a>(stream: &'a Stream, key: &[u8]) -> &'a [u8] {
        stream.dict.get(key).and_then(Object::as_name).unwrap()
    }

    #[test]
    fn pdf_embeds_jpeg_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("c.pdf");
        let pages = vec![
            test_images::jpeg_rgb(),
            test_images::jpeg_gray(),
            test_images::png(),
        ];
        write_pdf(&pages, &dest).unwrap();

        let doc = Document::load(&dest).unwrap();
        assert_eq!(doc.get_pages().len(), 3);

        let first = page_image(&doc, 1);
        assert_eq!(first.content, pages[0]);
        assert_eq!(name(first, b"Filter"), b"DCTDecode");
        assert_eq!(name(first, b"ColorSpace"), b"DeviceRGB");

        let second = page_image(&doc, 2);
        assert_eq!(second.content, pages[1]);
        assert_eq!(name(second, b"ColorSpace"), b"DeviceGray");

        let third = page_image(&doc, 3);
        assert_eq!(name(third, b"Filter"), b"FlateDecode");
        assert_eq!(third.dict.get(b"Width").unwrap().as_i64().unwrap(), 50);
        let rgb = image::load_from_memory(&pages[2]).unwrap().into_rgb8();
        assert_eq!(third.decompressed_content().unwrap(), rgb.into_raw());

        let first_page = doc.get_dictionary(doc.get_pages()[&1]).unwrap();
        let media_box: Vec<i64> = first_page
            .get(b"MediaBox")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o.as_i64().unwrap())
            .collect();
        assert_eq!(media_box, [0, 0, 40, 60]);
    }
}
