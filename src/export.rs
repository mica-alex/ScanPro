//! Writing documents: searchable PDF, image PDF, multi-page TIFF, one image
//! per page, or plain text.
//!
//! The searchable PDF puts each page image on the page and the OCR words on
//! top of it in text render mode 3 (invisible), each word stretched to the
//! box it was found in — so selecting, searching and copying work in any
//! viewer while the page still *looks* like the scan.
//!
//! The text uses a Type0 font with Identity-H encoding where every CID is the
//! character's UTF-16 code unit, plus a ToUnicode map, and no embedded glyphs:
//! nothing is ever drawn, so none are needed, and any Unicode text survives.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use pdf_writer::types::{CidFontType, FontFlags, SystemInfo, TextRenderingMode, UnicodeCmap};
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref, Str, TextStr};
use rayon::prelude::*;

use crate::config::OutputFormat;
use crate::ocr::OcrPage;
use crate::pipeline::{Page, StoredFormat, encode_jpeg};

#[derive(Clone, Debug)]
pub struct ExportOptions {
    pub format: OutputFormat,
    pub jpeg_quality: u8,
    /// Document title for PDF metadata.
    pub title: Option<String>,
}

/// Write `pages` to `dest` (its extension is replaced by the format's).
/// Returns every file written: one for documents, one per page for PNG/JPEG.
pub fn export(pages: &[Page], opts: &ExportOptions, dest: &Path) -> Result<Vec<PathBuf>> {
    if pages.is_empty() {
        bail!("there are no pages to save");
    }
    let dest = dest.with_extension(opts.format.extension());
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    match opts.format {
        OutputFormat::SearchablePdf => write_atomic(&dest, &pdf(pages, opts, true)?).map(|_| vec![dest]),
        OutputFormat::Pdf => write_atomic(&dest, &pdf(pages, opts, false)?).map(|_| vec![dest]),
        OutputFormat::Tiff => write_atomic(&dest, &tiff(pages)?).map(|_| vec![dest]),
        OutputFormat::Text => write_atomic(&dest, text(pages).as_bytes()).map(|_| vec![dest]),
        OutputFormat::Png | OutputFormat::Jpeg => per_page_images(pages, opts, &dest),
    }
}

/// All OCR text, pages separated by form feeds (the plain-text convention
/// `pdftotext` and friends use).
pub fn text(pages: &[Page]) -> String {
    pages.iter().map(|p| p.ocr.as_ref().map(|o| o.text()).unwrap_or_default()).collect::<Vec<_>>().join("\x0c")
}

fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = dest.with_extension("partial");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, dest).with_context(|| format!("saving {}", dest.display()))?;
    Ok(())
}

/// A file name that doesn't exist yet: `name.ext`, `name (2).ext`, ….
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..).map(|n| path.with_file_name(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).expect("unbounded")
}

fn per_page_images(pages: &[Page], opts: &ExportOptions, dest: &Path) -> Result<Vec<PathBuf>> {
    let stem = dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Scan".into());
    let ext = opts.format.extension();
    let paths: Vec<PathBuf> = if pages.len() == 1 {
        vec![dest.to_path_buf()]
    } else {
        (1..=pages.len()).map(|i| dest.with_file_name(format!("{stem}-{i:03}.{ext}"))).collect()
    };
    pages.par_iter().zip(&paths).try_for_each(|(page, path)| -> Result<()> {
        let bytes = if opts.format == OutputFormat::Jpeg {
            if page.format == StoredFormat::Jpeg && page.rotation == 0 {
                std::fs::read(&page.path)?
            } else {
                encode_jpeg(&page.load()?, opts.jpeg_quality)?
            }
        } else {
            let mut out = Vec::new();
            let img = page.load()?;
            let img = if page.gray { image::DynamicImage::ImageLuma8(img.to_luma8()) } else { img };
            img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
            out
        };
        write_atomic(path, &bytes)
    })?;
    Ok(paths)
}

fn tiff(pages: &[Page]) -> Result<Vec<u8>> {
    use tiff::encoder::{Compression, DeflateLevel, Rational, TiffEncoder, colortype};
    use tiff::tags::ResolutionUnit;
    // Decode in parallel; the encoder itself is sequential.
    let images: Vec<image::DynamicImage> = pages.par_iter().map(|p| p.load()).collect::<Result<_>>()?;
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut enc = TiffEncoder::new(&mut buf)?.with_compression(Compression::Deflate(DeflateLevel::Balanced));
    for (page, img) in pages.iter().zip(images) {
        let res = Rational { n: page.dpi, d: 1 };
        if page.gray {
            let g = img.to_luma8();
            let mut im = enc.new_image::<colortype::Gray8>(g.width(), g.height())?;
            im.resolution(ResolutionUnit::Inch, res);
            im.write_data(g.as_raw())?;
        } else {
            let c = img.to_rgb8();
            let mut im = enc.new_image::<colortype::RGB8>(c.width(), c.height())?;
            im.resolution(ResolutionUnit::Inch, res);
            im.write_data(c.as_raw())?;
        }
    }
    Ok(buf.into_inner())
}

/// One page image, ready to embed.
struct PdfImage {
    data: Vec<u8>,
    filter: Filter,
    width: u32,
    height: u32,
    gray: bool,
    bits: i32,
}

fn pdf_image(page: &Page, quality: u8) -> Result<PdfImage> {
    if page.format == StoredFormat::Jpeg && page.rotation == 0 {
        let data = std::fs::read(&page.path)?;
        let gray = jpeg_components(&data).map(|c| c == 1).unwrap_or(page.gray);
        return Ok(PdfImage { data, filter: Filter::DctDecode, width: page.width, height: page.height, gray, bits: 8 });
    }
    let img = page.load()?;
    let (width, height) = (img.width(), img.height());
    if page.bilevel {
        // Pack to 1 bit per pixel; in DeviceGray a set bit is white.
        let g = img.to_luma8();
        let row_bytes = (width as usize).div_ceil(8);
        let mut packed = vec![0u8; row_bytes * height as usize];
        for (y, row) in g.rows().enumerate() {
            for (x, px) in row.enumerate() {
                if px.0[0] >= 128 {
                    packed[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(&packed)?;
        return Ok(PdfImage { data: z.finish()?, filter: Filter::FlateDecode, width, height, gray: true, bits: 1 });
    }
    let img = if page.gray { image::DynamicImage::ImageLuma8(img.to_luma8()) } else { img };
    Ok(PdfImage {
        data: encode_jpeg(&img, quality)?,
        filter: Filter::DctDecode,
        width,
        height,
        gray: page.gray,
        bits: 8,
    })
}

/// Number of colour components from a JPEG's SOF header.
fn jpeg_components(data: &[u8]) -> Option<u8> {
    let mut i = 2;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            return data.get(i + 9).copied();
        }
        i += 2 + len;
    }
    None
}

fn pdf(pages: &[Page], opts: &ExportOptions, with_text: bool) -> Result<Vec<u8>> {
    // Image preparation (decode/rotate/encode) dominates; do it in parallel.
    let images: Vec<PdfImage> = pages.par_iter().map(|p| pdf_image(p, opts.jpeg_quality)).collect::<Result<_>>()?;

    let mut next = 1;
    let mut alloc = || {
        let r = Ref::new(next);
        next += 1;
        r
    };
    let catalog_id = alloc();
    let tree_id = alloc();
    let info_id = alloc();
    let font_id = alloc();
    let cid_font_id = alloc();
    let descriptor_id = alloc();
    let cmap_id = alloc();
    let ids: Vec<(Ref, Ref, Ref)> = pages.iter().map(|_| (alloc(), alloc(), alloc())).collect();

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(tree_id);
    pdf.pages(tree_id).kids(ids.iter().map(|(p, _, _)| *p)).count(pages.len() as i32);
    {
        let mut info = pdf.document_info(info_id);
        info.producer(TextStr(concat!("ScanPro ", env!("CARGO_PKG_VERSION"))));
        info.creator(TextStr("ScanPro"));
        if let Some(t) = &opts.title {
            info.title(TextStr(t));
        }
        let now = jiff::Zoned::now();
        info.creation_date(
            pdf_writer::Date::new(now.year() as u16)
                .month(now.month() as u8)
                .day(now.day() as u8)
                .hour(now.hour() as u8)
                .minute(now.minute() as u8)
                .second(now.second() as u8),
        );
    }

    let mut used_chars = BTreeSet::new();
    let has_text = with_text && pages.iter().any(|p| p.ocr.as_ref().is_some_and(|o| !o.lines.is_empty()));

    for ((page, img), &(page_id, image_id, content_id)) in pages.iter().zip(&images).zip(&ids) {
        let dpi = page.dpi.max(1) as f32;
        let (w_px, h_px) = (img.width as f32, img.height as f32);
        let (w_pt, h_pt) = (w_px * 72.0 / dpi, h_px * 72.0 / dpi);

        let mut image = pdf.image_xobject(image_id, &img.data);
        image.filter(img.filter);
        image.width(img.width as i32);
        image.height(img.height as i32);
        if img.gray {
            image.color_space().device_gray();
        } else {
            image.color_space().device_rgb();
        }
        image.bits_per_component(img.bits);
        image.finish();

        let mut content = Content::new();
        content.save_state();
        content.transform([w_pt, 0.0, 0.0, h_pt, 0.0, 0.0]);
        content.x_object(Name(b"Im0"));
        content.restore_state();
        if has_text && let Some(ocr) = &page.ocr {
            text_layer(
                &mut content,
                ocr,
                w_pt / ocr.width.max(1) as f32,
                h_pt / ocr.height.max(1) as f32,
                h_pt,
                &mut used_chars,
            );
        }
        pdf.stream(content_id, &content.finish());

        let mut p = pdf.page(page_id);
        p.media_box(Rect::new(0.0, 0.0, w_pt, h_pt));
        p.parent(tree_id);
        p.contents(content_id);
        let mut res = p.resources();
        res.x_objects().pair(Name(b"Im0"), image_id);
        if has_text {
            res.fonts().pair(Name(b"F0"), font_id);
        }
    }

    if has_text {
        write_font(&mut pdf, font_id, cid_font_id, descriptor_id, cmap_id, &used_chars);
    }
    Ok(pdf.finish())
}

const FONT_NAME: Name = Name(b"ScanProGlyphless");
/// Advance width of every glyph, in 1/1000 em (the font's `DW`).
const GLYPH_WIDTH: f32 = 500.0;

fn utf16_cid(c: char) -> u16 {
    let mut buf = [0u16; 2];
    // Non-BMP characters don't fit a 2-byte CID; they're vanishingly rare in
    // OCR output, so they become U+FFFD rather than complicating the CMap.
    match c.encode_utf16(&mut buf) {
        [u] => *u,
        _ => 0xFFFD,
    }
}

fn text_layer(content: &mut Content, ocr: &OcrPage, sx: f32, sy: f32, page_h: f32, used: &mut BTreeSet<u16>) {
    content.begin_text();
    content.set_text_rendering_mode(TextRenderingMode::Invisible);
    for line in &ocr.lines {
        let words: Vec<_> = line.words.iter().filter(|w| !w.text.trim().is_empty()).collect();
        for (i, word) in words.iter().enumerate() {
            let b = word.bbox;
            // Font size = box height and ascent 1000 / descent 0 in the font,
            // so a viewer's selection highlight covers exactly the word's box.
            let size = (b.h * sy).max(1.0);
            let x = b.x * sx;
            let baseline = page_h - (b.y + b.h) * sy;
            // Trailing space (stretched over the gap to the next word) so
            // copied text has spaces even in viewers that don't infer them.
            let mut text: String = word.text.trim().to_string();
            let target_w = match words.get(i + 1) {
                Some(next) if next.bbox.x > b.x + b.w => {
                    text.push(' ');
                    (next.bbox.x - b.x) * sx
                }
                _ => b.w * sx,
            };
            let n = text.chars().count().max(1) as f32;
            let natural = n * GLYPH_WIDTH / 1000.0 * size;
            let mut encoded = Vec::with_capacity(text.len() * 2);
            for c in text.chars() {
                let cid = utf16_cid(c);
                used.insert(cid);
                encoded.extend_from_slice(&cid.to_be_bytes());
            }
            content.set_font(Name(b"F0"), size);
            content.set_horizontal_scaling((100.0 * target_w / natural).clamp(1.0, 10_000.0));
            content.set_text_matrix([1.0, 0.0, 0.0, 1.0, x, baseline]);
            content.show(Str(&encoded));
        }
    }
    content.end_text();
}

fn write_font(pdf: &mut Pdf, font_id: Ref, cid_font_id: Ref, descriptor_id: Ref, cmap_id: Ref, used: &BTreeSet<u16>) {
    let system_info = SystemInfo { registry: Str(b"Adobe"), ordering: Str(b"Identity"), supplement: 0 };

    pdf.type0_font(font_id)
        .base_font(FONT_NAME)
        .encoding_predefined(Name(b"Identity-H"))
        .descendant_font(cid_font_id)
        .to_unicode(cmap_id);

    let mut cid = pdf.cid_font(cid_font_id);
    cid.subtype(CidFontType::Type2);
    cid.base_font(FONT_NAME);
    cid.system_info(system_info);
    cid.font_descriptor(descriptor_id);
    cid.default_width(GLYPH_WIDTH);
    cid.cid_to_gid_map_predefined(Name(b"Identity"));
    cid.finish();

    pdf.font_descriptor(descriptor_id)
        .name(FONT_NAME)
        .flags(FontFlags::SYMBOLIC)
        .bbox(Rect::new(0.0, 0.0, GLYPH_WIDTH, 1000.0))
        .italic_angle(0.0)
        .ascent(1000.0)
        .descent(0.0)
        .cap_height(1000.0)
        .stem_v(80.0);

    let mut cmap = UnicodeCmap::new(Name(b"ScanPro-UTF16"), system_info);
    for &code in used {
        if let Some(c) = char::from_u32(code as u32) {
            cmap.pair(code, c);
        }
    }
    pdf.cmap(cmap_id, &cmap.finish());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Processing;
    use crate::ocr::{OcrLine, OcrWord, Rect as R};
    use crate::pipeline::PageStore;
    use crate::scanner::ScannedImage;
    use std::sync::Arc;

    fn sample_page(store: &PageStore, bilevel: bool) -> Page {
        let img = image::GrayImage::from_fn(850, 1100, |x, y| {
            image::Luma([if (x / 40 + y / 40) % 2 == 0 { 0 } else { 255 }])
        });
        let (mut page, _) = store
            .ingest(
                ScannedImage::Pixels { image: image::DynamicImage::ImageLuma8(img), dpi: 100, bilevel },
                &Processing::default(),
            )
            .unwrap();
        let word = |t: &str, x: f32| OcrWord { text: t.into(), bbox: R { x, y: 100.0, w: 120.0, h: 30.0 } };
        page.ocr = Some(Arc::new(OcrPage {
            width: 850,
            height: 1100,
            lines: vec![OcrLine {
                words: vec![word("Hello", 100.0), word("Wörld", 260.0)],
                bbox: R { x: 100.0, y: 100.0, w: 280.0, h: 30.0 },
            }],
        }));
        page
    }

    #[test]
    fn searchable_pdf_has_text_layer_and_unicode_map() {
        let store = PageStore::new().unwrap();
        let pages = vec![sample_page(&store, true), sample_page(&store, false)];
        let opts = ExportOptions { format: OutputFormat::SearchablePdf, jpeg_quality: 80, title: Some("Test".into()) };
        let bytes = pdf(&pages, &opts, true).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.starts_with("%PDF-"));
        assert!(s.contains("/Identity-H"));
        assert!(s.contains("/ToUnicode"));
        assert!(s.contains("/DCTDecode"));
        assert!(s.contains("/FlateDecode"));
        assert!(s.contains("/Count 2"));
    }

    #[test]
    fn mixes_portrait_and_landscape_pages() {
        let store = PageStore::new().unwrap();
        let portrait = sample_page(&store, false);
        let mut landscape = sample_page(&store, false);
        landscape.rotate_by(-90);
        let mut landscape_bw = sample_page(&store, true);
        landscape_bw.rotate_by(90);
        let opts = ExportOptions { format: OutputFormat::Pdf, jpeg_quality: 80, title: None };
        let bytes = pdf(&[portrait, landscape, landscape_bw], &opts, false).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        // 850×1100 px at 100 dpi is 612×792 pt; turned, 792×612.
        assert_eq!(s.matches("/MediaBox [0 0 612 792]").count(), 1, "one portrait page");
        assert_eq!(s.matches("/MediaBox [0 0 792 612]").count(), 2, "two landscape pages");
        assert_eq!(s.matches("/Width 1100").count(), 2, "landscape images are stored turned");
    }

    #[test]
    fn image_pdf_has_no_font() {
        let store = PageStore::new().unwrap();
        let pages = vec![sample_page(&store, false)];
        let opts = ExportOptions { format: OutputFormat::Pdf, jpeg_quality: 80, title: None };
        let s = String::from_utf8_lossy(&pdf(&pages, &opts, false).unwrap()).into_owned();
        assert!(!s.contains("/Type0"));
    }

    #[test]
    fn exports_every_format() {
        let store = PageStore::new().unwrap();
        let pages = vec![sample_page(&store, false), sample_page(&store, true)];
        let dir = tempfile::tempdir().unwrap();
        for format in OutputFormat::ALL {
            let opts = ExportOptions { format, jpeg_quality: 80, title: None };
            let written = export(&pages, &opts, &dir.path().join("doc")).unwrap();
            assert!(!written.is_empty());
            for p in written {
                assert!(std::fs::metadata(&p).unwrap().len() > 0, "{format:?} wrote empty {}", p.display());
            }
        }
        let txt = std::fs::read_to_string(dir.path().join("doc.txt")).unwrap();
        assert!(txt.contains("Hello Wörld"));
    }

    #[test]
    fn unique_path_appends_counter() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.pdf");
        assert_eq!(unique_path(&p), p);
        std::fs::write(&p, b"x").unwrap();
        assert_eq!(unique_path(&p), dir.path().join("a (2).pdf"));
    }

    #[test]
    fn reads_jpeg_component_count() {
        let rgb = encode_jpeg(&image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 8)), 80).unwrap();
        let gray = encode_jpeg(&image::DynamicImage::ImageLuma8(image::GrayImage::new(8, 8)), 80).unwrap();
        assert_eq!(jpeg_components(&rgb), Some(3));
        assert_eq!(jpeg_components(&gray), Some(1));
    }
}
