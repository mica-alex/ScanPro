//! Pages between the scanner and the exporter.
//!
//! Each scanned page is written to a session directory straight away (JPEG
//! for colour/gray, PNG for line art) and only a small thumbnail stays in
//! memory, so a 300-page duplex job doesn't hold gigabytes of pixels.
//! Everything downstream — preview, OCR, export — re-reads the stored file.
//!
//! A [`Page`]'s `rotation` is applied on read ([`Page::load`]); OCR results
//! are tied to the rotation they were computed for and discarded when it
//! changes.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use image::{DynamicImage, GrayImage, ImageFormat};

use crate::config::Processing;
use crate::ocr::OcrPage;
use crate::scanner::ScannedImage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoredFormat {
    Jpeg,
    Png,
}

#[derive(Clone, Debug)]
pub struct Page {
    pub id: u64,
    pub path: PathBuf,
    pub format: StoredFormat,
    /// Size of the stored (unrotated) image.
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub gray: bool,
    pub bilevel: bool,
    /// Clockwise degrees: 0, 90, 180 or 270.
    pub rotation: u16,
    /// Detected as blank at scan time. Kept (but flagged) when skipping is off.
    pub blank: bool,
    /// The user rotated this page by hand; auto-rotation must not undo it.
    pub rotation_locked: bool,
    /// OCR for the current rotation, once done.
    pub ocr: Option<Arc<OcrPage>>,
}

impl Page {
    /// Width/height as displayed, i.e. after rotation.
    pub fn oriented_size(&self) -> (u32, u32) {
        if self.rotation % 180 == 90 { (self.height, self.width) } else { (self.width, self.height) }
    }

    /// Decode the stored image with rotation applied.
    pub fn load(&self) -> Result<DynamicImage> {
        let img = image::open(&self.path).with_context(|| format!("reading {}", self.path.display()))?;
        Ok(rotate(img, self.rotation))
    }

    pub fn rotate_by(&mut self, degrees: i32) {
        self.rotation = ((self.rotation as i32 + degrees).rem_euclid(360)) as u16;
        self.ocr = None;
    }
}

pub fn rotate(img: DynamicImage, rotation: u16) -> DynamicImage {
    match rotation {
        90 => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _ => img,
    }
}

/// Owns the session directory holding page files; removes it on drop.
pub struct PageStore {
    dir: PathBuf,
    next_id: AtomicU64,
}

impl PageStore {
    pub fn new() -> Result<PageStore> {
        let base = crate::config::project_dirs()
            .map(|d| d.cache_dir().to_path_buf())
            .unwrap_or_else(std::env::temp_dir)
            .join("sessions");
        // Sessions left behind by a crash are swept on the next launch.
        if let Ok(entries) = std::fs::read_dir(&base) {
            for e in entries.flatten() {
                let stale = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().map(|d| d.as_secs() > 86_400).unwrap_or(false))
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = base.join(format!(
            "{}-{}-{}",
            std::process::id(),
            jiff::Timestamp::now().as_millisecond(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(PageStore { dir, next_id: AtomicU64::new(1) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Persist a freshly scanned image and analyse it. Returns the page and
    /// the decoded pixels so the caller can thumbnail/OCR without re-reading.
    pub fn ingest(&self, scanned: ScannedImage, processing: &Processing) -> Result<(Page, DynamicImage)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let dpi = scanned.dpi().max(1);
        let (image, format, path, bilevel) = match scanned {
            // JPEG from the scanner is kept byte-for-byte so the PDF can embed
            // it without a second generation of compression loss.
            ScannedImage::Encoded { bytes, format: ImageFormat::Jpeg, .. } => {
                let image = image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg)?;
                let path = self.dir.join(format!("{id}.jpg"));
                std::fs::write(&path, &bytes)?;
                (image, StoredFormat::Jpeg, path, false)
            }
            ScannedImage::Encoded { bytes, .. } => {
                let image = image::load_from_memory(&bytes)?;
                let bilevel = is_bilevel(&image);
                let (format, path) = self.write(id, &image, bilevel, processing.jpeg_quality)?;
                (image, format, path, bilevel)
            }
            ScannedImage::Pixels { image, bilevel, .. } => {
                let (format, path) = self.write(id, &image, bilevel, processing.jpeg_quality)?;
                (image, format, path, bilevel)
            }
        };
        let gray = matches!(image, DynamicImage::ImageLuma8(_) | DynamicImage::ImageLuma16(_)) || bilevel;
        let blank = is_blank(&image, processing.blank_threshold_percent);
        let page = Page {
            id,
            path,
            format,
            width: image.width(),
            height: image.height(),
            dpi,
            gray,
            bilevel,
            rotation: 0,
            blank,
            rotation_locked: false,
            ocr: None,
        };
        Ok((page, image))
    }

    /// Import an existing image file as a page (the `convert` command and
    /// drag-and-drop).
    pub fn import(&self, path: &Path, default_dpi: u32, processing: &Processing) -> Result<(Page, DynamicImage)> {
        let bytes = std::fs::read(path)?;
        let format = image::guess_format(&bytes)?;
        let dpi = jpeg_density(&bytes).unwrap_or(default_dpi);
        self.ingest(ScannedImage::Encoded { bytes, format, dpi }, processing)
    }

    fn write(&self, id: u64, image: &DynamicImage, bilevel: bool, quality: u8) -> Result<(StoredFormat, PathBuf)> {
        if bilevel {
            let path = self.dir.join(format!("{id}.png"));
            // Fast compression: page files are scratch; the exporter
            // compresses for real.
            let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
            let enc = image::codecs::png::PngEncoder::new_with_quality(
                file,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::NoFilter,
            );
            image.to_luma8().write_with_encoder(enc)?;
            Ok((StoredFormat::Png, path))
        } else {
            let path = self.dir.join(format!("{id}.jpg"));
            let bytes = encode_jpeg(image, quality)?;
            std::fs::write(&path, bytes)?;
            Ok((StoredFormat::Jpeg, path))
        }
    }

    /// Delete a page's file once the page is gone for good.
    pub fn remove(&self, page: &Page) {
        let _ = std::fs::remove_file(&page.path);
    }
}

impl Drop for PageStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut out), quality.clamp(1, 100));
    match image {
        DynamicImage::ImageLuma8(g) => g.write_with_encoder(enc)?,
        other => other.to_rgb8().write_with_encoder(enc)?,
    }
    Ok(out)
}

/// The JFIF density, when it's in dots per inch.
fn jpeg_density(bytes: &[u8]) -> Option<u32> {
    // SOI, then APP0 "JFIF\0" ver(2) units(1) xdens(2) ydens(2).
    let app0 = bytes.windows(5).position(|w| w == b"JFIF\0")?;
    let units = *bytes.get(app0 + 7)?;
    let x = u16::from_be_bytes([*bytes.get(app0 + 8)?, *bytes.get(app0 + 9)?]) as u32;
    match units {
        1 if x > 1 => Some(x),
        2 if x > 1 => Some((x as f32 * 2.54).round() as u32),
        _ => None,
    }
}

fn is_bilevel(image: &DynamicImage) -> bool {
    let DynamicImage::ImageLuma8(g) = image else { return false };
    g.as_raw().iter().step_by(97).all(|&v| v == 0 || v == 255)
}

/// Downscale to roughly `max` px on the long edge for analysis.
fn small_gray(image: &DynamicImage, max: u32) -> GrayImage {
    let (w, h) = (image.width(), image.height());
    let scale = (max as f32 / w.max(h) as f32).min(1.0);
    let (nw, nh) = (((w as f32 * scale) as u32).max(1), ((h as f32 * scale) as u32).max(1));
    image::imageops::thumbnail(&image.to_luma8(), nw, nh)
}

/// A page is blank when almost nothing on it is much darker than the paper.
///
/// The paper level is taken from the page itself (90th-percentile brightness)
/// so tinted or recycled paper works, and "ink" has to be well below it, so
/// faint show-through from the other side of the sheet doesn't count. Margins
/// are ignored: feeders leave dark edges and shadows there.
pub fn is_blank(image: &DynamicImage, threshold_percent: f32) -> bool {
    ink_percent(image) < threshold_percent
}

pub fn ink_percent(image: &DynamicImage) -> f32 {
    let g = small_gray(image, 600);
    let (w, h) = g.dimensions();
    let (mx, my) = (w / 16, h / 16);
    let mut hist = [0u32; 256];
    let mut total = 0u32;
    for y in my..h.saturating_sub(my) {
        for x in mx..w.saturating_sub(mx) {
            hist[g.get_pixel(x, y).0[0] as usize] += 1;
            total += 1;
        }
    }
    if total == 0 {
        return 0.0;
    }
    let mut acc = 0u32;
    let mut paper = 255u32;
    for (v, &n) in hist.iter().enumerate() {
        acc += n;
        if acc as f32 >= total as f32 * 0.9 {
            paper = v as u32;
            break;
        }
    }
    let ink_level = paper.saturating_sub(80).min(paper * 6 / 10 + 20);
    let ink: u32 = hist[..=ink_level as usize].iter().sum();
    ink as f32 * 100.0 / total as f32
}

/// RGBA thumbnail at most `max` px on the long edge, rotation applied.
pub fn thumbnail(image: &DynamicImage, rotation: u16, max: u32) -> image::RgbaImage {
    let (w, h) = (image.width(), image.height());
    let scale = (max as f32 / w.max(h) as f32).min(1.0);
    let (nw, nh) = (((w as f32 * scale) as u32).max(1), ((h as f32 * scale) as u32).max(1));
    let small = image.thumbnail_exact(nw, nh);
    rotate(small, rotation).to_rgba8()
}

/// The image handed to OCR: grayscale, and capped at ~300 dpi equivalent
/// because recognition gains nothing beyond that and time grows with area.
pub fn ocr_input(page: &Page) -> Result<(DynamicImage, f32)> {
    let img = page.load()?;
    let scale = if page.dpi > 330 { 300.0 / page.dpi as f32 } else { 1.0 };
    let img = if scale < 1.0 {
        let (w, h) = ((img.width() as f32 * scale) as u32, (img.height() as f32 * scale) as u32);
        DynamicImage::ImageLuma8(image::imageops::resize(&img.to_luma8(), w, h, image::imageops::FilterType::Triangle))
    } else {
        DynamicImage::ImageLuma8(img.to_luma8())
    };
    Ok((img, scale))
}

/// Run OCR for a page and scale the boxes back to the page's oriented pixel
/// size, so exporters never need to know OCR ran on a downscaled copy.
pub fn run_ocr(page: &Page, engine: &dyn crate::ocr::OcrEngine, languages: &[String]) -> Result<OcrPage> {
    let (img, scale) = ocr_input(page)?;
    let mut result = engine.recognize(&img, languages)?;
    if scale != 1.0 {
        let inv = 1.0 / scale;
        for line in &mut result.lines {
            scale_rect(&mut line.bbox, inv);
            for w in &mut line.words {
                scale_rect(&mut w.bbox, inv);
            }
        }
    }
    let (w, h) = page.oriented_size();
    result.width = w;
    result.height = h;
    Ok(result)
}

/// How much an OCR result looks like upright text, 0–1. The product of:
/// the share of words that sit in multi-word lines, the share of word pairs
/// within a line that run left to right, and the share of words that look
/// like real words.
///
/// Upside-down pages fail at least one: Vision often reads the glyphs fine
/// but either loses the lines (one "line" per word) or reports boxes running
/// right to left; other engines keep the geometry but read garbage.
pub fn upright_score(ocr: &OcrPage) -> f32 {
    let words: Vec<&str> = ocr.lines.iter().flat_map(|l| l.words.iter().map(|w| w.text.as_str())).collect();
    if words.is_empty() {
        return 1.0;
    }
    let in_lines: usize = ocr.lines.iter().filter(|l| l.words.len() >= 2).map(|l| l.words.len()).sum();
    let (mut forward, mut pairs) = (0usize, 0usize);
    for line in &ocr.lines {
        for pair in line.words.windows(2) {
            pairs += 1;
            if pair[1].bbox.x + pair[1].bbox.w / 2.0 > pair[0].bbox.x + pair[0].bbox.w / 2.0 {
                forward += 1;
            }
        }
    }
    let direction = if pairs == 0 { 1.0 } else { forward as f32 / pairs as f32 };
    let wordlike = words
        .iter()
        .filter(|w| {
            let letters = w.chars().filter(|c| c.is_alphabetic()).count();
            letters >= 2 && letters * 10 >= w.chars().count() * 7
        })
        .count();
    let alpha = words.iter().filter(|w| w.chars().any(char::is_alphabetic)).count().max(1);
    (in_lines as f32 / words.len() as f32) * direction * (wordlike as f32 / alpha as f32)
}

/// How much readable, upright text an OCR result holds: the upright score
/// weighted by word count (capped, so one dense page doesn't dominate).
fn orientation_evidence(ocr: &OcrPage) -> f32 {
    let words: usize = ocr.lines.iter().map(|l| l.words.len()).sum();
    upright_score(ocr) * words.min(40) as f32
}

/// [`run_ocr`], and when the result doesn't look like upright text, try the
/// page turned 90°, 180° and 270° and keep whichever reads best. Covers pages
/// fed upside down and landscape pages fed sideways. Returns the rotation
/// the result belongs to; the caller applies it to the page.
///
/// The other orientations are probed on one downscaled copy, so a page that
/// needs no turning costs nothing extra and one that does costs three quick
/// probes plus a single full-resolution pass in the winning orientation.
pub fn run_ocr_oriented(
    page: &Page,
    engine: &dyn crate::ocr::OcrEngine,
    languages: &[String],
) -> Result<(u16, OcrPage)> {
    let first = run_ocr(page, engine, languages)?;
    let score = upright_score(&first);
    let word_count: usize = first.lines.iter().map(|l| l.words.len()).sum();
    log::debug!("page {}: upright score {score:.2} over {word_count} words", page.id);
    if score >= 0.5 && word_count >= 8 {
        return Ok((page.rotation, first));
    }

    let small = DynamicImage::ImageLuma8(page.load()?.thumbnail(1600, 1600).to_luma8());
    let current = orientation_evidence(&first);
    let mut best: Option<(i32, f32)> = None;
    for turn in [90, 180, 270] {
        let probe = engine.recognize(&rotate(small.clone(), turn as u16), languages)?;
        let evidence = orientation_evidence(&probe);
        log::debug!("page {}: turned {turn}° evidence {evidence:.1} (as scanned {current:.1})", page.id);
        if best.is_none_or(|(_, e)| evidence > e) {
            best = Some((turn, evidence));
        }
    }
    // Demand a clear win, and enough text to be sure: a photo or a nearly
    // blank page should stay as scanned.
    match best {
        Some((turn, evidence)) if evidence >= 4.0 && evidence > current * 1.3 => {
            let mut turned = page.clone();
            turned.rotate_by(turn);
            log::info!("page {} was turned {turn}° to read upright", page.id);
            Ok((turned.rotation, run_ocr(&turned, engine, languages)?))
        }
        _ => Ok((page.rotation, first)),
    }
}

/// Standard paper lengths in mm, shortest first, that a feeder scan with
/// "Auto-detect" paper is trimmed to.
const PAPER_LENGTHS_MM: [f32; 4] = [210.0, 279.4, 297.0, 355.6];

/// Trim a feeder page scanned at the feeder's maximum length down to the
/// shortest standard paper length that still holds all of its content —
/// for scanners whose driver doesn't detect the paper end itself. A page
/// that is already that short, or whose content runs past every standard
/// length, is left alone.
pub fn trim_feeder_length(scanned: ScannedImage) -> ScannedImage {
    let ScannedImage::Pixels { image, dpi, bilevel } = scanned else { return scanned };
    let px_per_mm = dpi as f32 / 25.4;
    let lengths = PAPER_LENGTHS_MM.map(|mm| (mm * px_per_mm).round() as u32);
    let bottom = find_bottom(&image, px_per_mm);
    let target = bottom
        .edge
        // The sheet's trailing edge shows as a shadow band: snap to the
        // standard length that falls inside it.
        .and_then(|(top, end)| {
            let slack = (2.0 * px_per_mm) as u32;
            lengths.iter().copied().find(|&len| len + slack >= top && len <= end + slack && len >= bottom.content)
        })
        // Otherwise: the shortest length that holds all the content.
        .or_else(|| lengths.iter().copied().find(|&len| len as f32 >= bottom.content as f32 + 3.0 * px_per_mm));
    let image = match target {
        Some(len) if len < image.height() => image.crop_imm(0, 0, image.width(), len),
        _ => image,
    };
    ScannedImage::Pixels { image, dpi, bilevel }
}

struct Bottom {
    /// One past the last row of real content (full-size pixels).
    content: u32,
    /// Rows (top, end) of the sheet's trailing-edge shadow, when visible.
    edge: Option<(u32, u32)>,
}

fn find_bottom(image: &DynamicImage, px_per_mm: f32) -> Bottom {
    let g = small_gray(image, 800);
    let scale = image.height() as f32 / g.height() as f32;
    let (w, h) = g.dimensions();
    let margin = w / 20;
    let span = w.saturating_sub(2 * margin).max(1);
    let mut row_min = vec![255u8; h as usize];
    for y in 0..h {
        for x in margin..w - margin {
            row_min[y as usize] = row_min[y as usize].min(g.get_pixel(x, y).0[0]);
        }
    }
    let mut sorted = row_min.clone();
    sorted.sort_unstable();
    // Brightest rows are bare paper/backing; "content" is clearly darker.
    let paper = sorted[sorted.len() * 9 / 10];
    let threshold = paper.saturating_sub(40);
    let is_content = |y: usize| row_min[y] < threshold;
    let coverage = |y: usize| {
        (margin..w - margin).filter(|&x| g.get_pixel(x, y as u32).0[0] < threshold).count() as f32 / span as f32
    };
    let to_full = |row: usize| ((row + 1) as f32 * scale).ceil() as u32;
    let Some(last) = (0..h as usize).rposition(is_content) else { return Bottom { content: 0, edge: None } };
    let mut band_top = last;
    while band_top > 0 && is_content(band_top - 1) {
        band_top -= 1;
    }
    // An edge shadow is a short band (a few mm) shading a good part of the
    // width. A line of text can look similar, which is why the caller only
    // trusts it when a standard paper length falls inside the band.
    let band_mm = (last - band_top + 1) as f32 * scale / px_per_mm;
    let peak = (band_top..=last).map(coverage).fold(0.0, f32::max);
    if band_top > 0 && band_mm <= 7.0 && peak >= 0.3 {
        let above = (0..band_top).rposition(is_content).map(to_full).unwrap_or(0);
        let top = (band_top as f32 * scale) as u32;
        return Bottom { content: above, edge: Some((top, to_full(last))) };
    }
    Bottom { content: to_full(last), edge: None }
}

fn scale_rect(r: &mut crate::ocr::Rect, s: f32) {
    r.x *= s;
    r.y *= s;
    r.w *= s;
    r.h *= s;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_with(f: impl Fn(u32, u32) -> u8) -> DynamicImage {
        DynamicImage::ImageLuma8(GrayImage::from_fn(850, 1100, |x, y| image::Luma([f(x, y)])))
    }

    #[test]
    fn white_page_is_blank() {
        assert!(is_blank(&page_with(|_, _| 245), 0.2));
    }

    #[test]
    fn show_through_and_dark_edges_are_blank() {
        // Faint mirrored text (light gray) plus a black feeder edge.
        let img = page_with(|x, y| {
            if x < 20 {
                10
            } else if (y / 12) % 3 == 0 && x % 7 < 4 {
                205
            } else {
                240
            }
        });
        assert!(is_blank(&img, 0.2), "ink {}", ink_percent(&img));
    }

    #[test]
    fn text_page_is_not_blank() {
        let img = page_with(|x, y| if (300..800).contains(&y) && (y / 14) % 2 == 0 && x % 9 < 4 { 30 } else { 240 });
        assert!(!is_blank(&img, 0.2));
    }

    #[test]
    fn trims_long_feeder_page_to_letter() {
        // 300 dpi, 14.5" scan of a letter page whose text ends at 10".
        let img = DynamicImage::ImageLuma8(GrayImage::from_fn(2550, 4350, |x, y| {
            image::Luma([if y < 3000 && (y / 20) % 3 == 0 && x % 11 < 5 { 20 } else { 245 }])
        }));
        let out = trim_feeder_length(ScannedImage::Pixels { image: img, dpi: 300, bilevel: false });
        let ScannedImage::Pixels { image, .. } = out else { panic!() };
        assert_eq!(image.height(), 3300);
    }

    #[test]
    fn snaps_to_visible_paper_edge() {
        // Content ends at 10", then the sheet's trailing-edge shadow runs
        // from 10.86" to 11.03" (as on a real ES-300W scan) — Letter, not A4.
        let img = DynamicImage::ImageLuma8(GrayImage::from_fn(2550, 4350, |x, y| {
            let text = y < 3000 && (y / 20) % 3 == 0 && x % 11 < 5;
            let edge = (3258..3310).contains(&y) && x % 5 < 2;
            image::Luma([if text {
                20
            } else if edge {
                90
            } else {
                245
            }])
        }));
        let out = trim_feeder_length(ScannedImage::Pixels { image: img, dpi: 300, bilevel: false });
        let ScannedImage::Pixels { image, .. } = out else { panic!() };
        assert_eq!(image.height(), 3300);
    }

    #[test]
    fn keeps_page_with_content_past_legal() {
        let img = DynamicImage::ImageLuma8(GrayImage::from_fn(600, 1000, |_, y| {
            image::Luma([if y > 980 { 0 } else { 240 }])
        }));
        let out = trim_feeder_length(ScannedImage::Pixels { image: img, dpi: 60, bilevel: false });
        let ScannedImage::Pixels { image, .. } = out else { panic!() };
        assert_eq!(image.height(), 1000);
    }

    #[test]
    fn upright_score_spots_vision_upside_down_output() {
        use crate::ocr::{OcrLine, OcrWord, Rect};
        let line = |words: &[&str]| OcrLine {
            words: words
                .iter()
                .enumerate()
                .map(|(i, w)| OcrWord {
                    text: w.to_string(),
                    bbox: Rect { x: i as f32 * 100.0, y: 0.0, w: 90.0, h: 20.0 },
                })
                .collect(),
            bbox: Rect::default(),
        };
        let backwards = |words: &[&str]| {
            let mut l = line(words);
            l.words.iter_mut().for_each(|w| w.bbox.x = 1000.0 - w.bbox.x);
            l
        };
        let upright = OcrPage {
            width: 1,
            height: 1,
            lines: vec![line(&["Harbor", "View", "Suites"]), line(&["Total", "212.40"])],
        };
        let flipped = OcrPage {
            width: 1,
            height: 1,
            lines: ["Suites", "View", "Harbor", "212.40", "Total"].iter().map(|w| line(&[w])).collect(),
        };
        let reversed = OcrPage {
            width: 1,
            height: 1,
            lines: vec![backwards(&["Harbor", "View", "Suites"]), backwards(&["Total", "212.40"])],
        };
        assert!(upright_score(&upright) > 0.8);
        assert!(upright_score(&flipped) < 0.1);
        assert!(upright_score(&reversed) < 0.1);
    }

    #[test]
    fn rotation_swaps_size_and_drops_ocr() {
        let mut p = Page {
            id: 1,
            path: PathBuf::new(),
            format: StoredFormat::Jpeg,
            width: 100,
            height: 200,
            dpi: 300,
            gray: false,
            bilevel: false,
            rotation: 0,
            blank: false,
            rotation_locked: false,
            ocr: Some(Arc::new(OcrPage::default())),
        };
        p.rotate_by(-90);
        assert_eq!(p.rotation, 270);
        assert_eq!(p.oriented_size(), (200, 100));
        assert!(p.ocr.is_none());
    }
}
