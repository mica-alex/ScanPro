//! [ocrs](https://github.com/robertknight/ocrs): pure-Rust OCR, used on Linux
//! and as the fallback elsewhere. Latin script only.
//!
//! The two models (~12 MB together) are not bundled: they are downloaded into
//! the app data directory the first time the engine is used and loaded from
//! there afterwards.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use ocrs::{ImageSource, OcrEngineParams, TextItem};

use super::{OcrEngine, OcrLine, OcrPage, OcrWord, Rect};

const MODEL_BASE_URL: &str = "https://ocrs-models.s3-accelerate.amazonaws.com";
const DETECTION_MODEL: &str = "text-detection.rten";
const RECOGNITION_MODEL: &str = "text-recognition.rten";
/// Anything smaller is an error page or a truncated download, not a model.
const MIN_MODEL_BYTES: u64 = 512 * 1024;

/// Serialises first-use downloads, so two OCR workers starting at once do not
/// both fetch (and race to rename) the same file.
static DOWNLOAD_LOCK: Mutex<()> = Mutex::new(());

pub struct OcrsEngine {
    engine: ocrs::OcrEngine,
}

/// `<app data>/models/`, e.g. `~/Library/Application Support/com.hawka97.ScanPro/models`.
pub fn models_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "hawka97", "ScanPro")
        .ok_or_else(|| anyhow!("no home directory to store OCR models in"))?;
    Ok(dirs.data_dir().join("models"))
}

/// Whether both models are already on disk (i.e. [`OcrsEngine::load`] will
/// not need the network).
pub fn models_present() -> bool {
    models_dir().is_ok_and(|dir| [DETECTION_MODEL, RECOGNITION_MODEL].iter().all(|name| model_file_ok(&dir.join(name))))
}

fn model_file_ok(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() >= MIN_MODEL_BYTES)
}

/// Returns the model's path, downloading it first if it is missing.
fn ensure_model(dir: &Path, name: &str) -> Result<PathBuf> {
    let path = dir.join(name);
    if model_file_ok(&path) {
        return Ok(path);
    }
    let _guard = DOWNLOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if model_file_ok(&path) {
        return Ok(path); // Another worker fetched it while we waited.
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let url = format!("{MODEL_BASE_URL}/{name}");
    log::info!("downloading OCR model {url}");
    let mut resp = ureq::get(&url).call().with_context(|| format!("downloading {url}"))?;
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(256 * 1024 * 1024)
        .read_to_vec()
        .with_context(|| format!("downloading {url}"))?;
    if (bytes.len() as u64) < MIN_MODEL_BYTES {
        bail!("download of {url} was only {} bytes", bytes.len());
    }
    // Write beside the target and rename, so an interrupted download never
    // leaves a truncated file that looks like a model.
    let part = dir.join(format!("{name}.part"));
    std::fs::write(&part, &bytes).with_context(|| format!("writing {}", part.display()))?;
    std::fs::rename(&part, &path).with_context(|| format!("installing {}", path.display()))?;
    Ok(path)
}

impl OcrsEngine {
    /// Loads the models, downloading them on first use.
    pub fn load() -> Result<Self> {
        let dir = models_dir()?;
        let detection = ensure_model(&dir, DETECTION_MODEL)?;
        let recognition = ensure_model(&dir, RECOGNITION_MODEL)?;
        let engine = ocrs::OcrEngine::new(OcrEngineParams {
            detection_model: Some(
                rten::Model::load_file(&detection).with_context(|| format!("loading {}", detection.display()))?,
            ),
            recognition_model: Some(
                rten::Model::load_file(&recognition).with_context(|| format!("loading {}", recognition.display()))?,
            ),
            ..Default::default()
        })?;
        Ok(Self { engine })
    }
}

/// Axis-aligned box of a recognised line or word, in input-image pixels.
fn item_rect(item: &impl TextItem) -> Rect {
    let r = item.bounding_rect();
    Rect { x: r.left() as f32, y: r.top() as f32, w: r.width() as f32, h: r.height() as f32 }
}

impl OcrEngine for OcrsEngine {
    fn name(&self) -> &'static str {
        "ocrs"
    }

    fn recognize(&self, image: &image::DynamicImage, _languages: &[String]) -> Result<OcrPage> {
        // ocrs works on greyscale internally; a 1-channel source skips a copy.
        let gray = image.to_luma8();
        let source = ImageSource::from_bytes(gray.as_raw(), gray.dimensions())
            .map_err(|e| anyhow!("preparing image for OCR: {e}"))?;
        let input = self.engine.prepare_input(source)?;
        let word_rects = self.engine.detect_words(&input)?;
        let line_rects = self.engine.find_text_lines(&input, &word_rects);
        let recognized = self.engine.recognize_text(&input, &line_rects)?;

        let mut lines: Vec<OcrLine> = recognized
            .into_iter()
            .flatten()
            .filter_map(|line| {
                // Recognition yields per-character boxes, so word boxes come
                // straight from the characters each word is made of.
                let words: Vec<OcrWord> = line
                    .words()
                    .map(|w| OcrWord { text: w.to_string(), bbox: item_rect(&w) })
                    .filter(|w| !w.text.trim().is_empty())
                    .collect();
                if words.is_empty() {
                    return None;
                }
                Some(OcrLine { bbox: item_rect(&line), words })
            })
            .collect();
        // find_text_lines already orders lines; this only settles ties so
        // output is stable top-to-bottom, then left-to-right.
        lines.sort_by(|a, b| a.bbox.y.total_cmp(&b.bbox.y).then(a.bbox.x.total_cmp(&b.bbox.x)));

        Ok(OcrPage { width: gray.width(), height: gray.height(), lines })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn engine_is_send_sync() {
        assert_send_sync::<OcrsEngine>();
    }

    /// Downloads the models (network) and reads a synthetic page. The text is
    /// drawn as blocky 5×7 glyphs, so this checks plumbing and box geometry,
    /// not recognition quality.
    #[test]
    #[ignore = "downloads ~12 MB of models"]
    fn recognizes_synthetic_text() {
        let engine = OcrsEngine::load().unwrap();
        assert!(models_present());
        let img = render_text("HELLO   WORLD", 10);
        let page = engine.recognize(&image::DynamicImage::ImageLuma8(img), &[]).unwrap();
        let text = page.text();
        eprintln!("ocrs read: {text:?}");
        // Blocky glyphs read unreliably; assert the plumbing, not accuracy.
        assert!(!page.lines.is_empty(), "no text found");
        for word in page.lines.iter().flat_map(|l| &l.words) {
            let b = word.bbox;
            assert!(b.w > 0.0 && b.h > 0.0);
            assert!(b.x >= 0.0 && b.y >= 0.0);
            assert!(b.x + b.w <= page.width as f32 + 1.0 && b.y + b.h <= page.height as f32 + 1.0);
        }
    }

    /// OCRs a real image: `SCANPRO_OCR_SAMPLE=/path/to/page.png cargo test --lib
    /// ocrs_sample -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs SCANPRO_OCR_SAMPLE and the models"]
    fn ocrs_sample() {
        let Ok(path) = std::env::var("SCANPRO_OCR_SAMPLE") else { return };
        let engine = OcrsEngine::load().unwrap();
        let img = image::open(path).unwrap();
        let page = engine.recognize(&img, &[]).unwrap();
        for line in &page.lines {
            let boxes: Vec<_> =
                line.words.iter().map(|w| (w.text.as_str(), w.bbox.x as i32, w.bbox.y as i32)).collect();
            eprintln!("{boxes:?}");
        }
    }

    /// Minimal 5×7 bitmap font for the letters the test needs.
    fn render_text(text: &str, scale: u32) -> image::GrayImage {
        fn glyph(c: char) -> [u8; 7] {
            match c {
                'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
                'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
                'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
                'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
                'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
                'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
                'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
                _ => [0; 7],
            }
        }
        let margin = 40;
        let w = margin * 2 + text.len() as u32 * 7 * scale;
        let h = margin * 2 + 7 * scale;
        let mut img = image::GrayImage::from_pixel(w, h, image::Luma([255]));
        for (i, c) in text.chars().enumerate() {
            let rows = glyph(c);
            for (row, bits) in rows.iter().enumerate() {
                for col in 0..5 {
                    if bits & (0x10 >> col) != 0 {
                        let x0 = margin + (i as u32 * 7 + col) * scale;
                        let y0 = margin + row as u32 * scale;
                        for y in y0..y0 + scale {
                            for x in x0..x0 + scale {
                                img.put_pixel(x, y, image::Luma([0]));
                            }
                        }
                    }
                }
            }
        }
        img
    }
}
