//! Windows.Media.Ocr — the OCR engine built into Windows 10/11. Languages come
//! from the user's installed OCR language packs (Settings › Time & language ›
//! Language › *language* › Optical character recognition).
//!
//! WinRT objects are not `Send`, so the engine stores only the language tags
//! and builds the WinRT `OcrEngine` per call; creation is cheap next to
//! recognition.

use anyhow::{Context, Result, anyhow};
use image::imageops::FilterType;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine as WinOcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::core::HSTRING;

use super::{OcrEngine, OcrLine, OcrPage, OcrWord, Rect};
use crate::scanner::wia::ComGuard;

pub struct WindowsOcrEngine;

impl WindowsOcrEngine {
    /// Fails if no OCR language pack is installed, so the caller can fall
    /// back to another engine instead of failing every page.
    pub fn new() -> Result<Self> {
        let _com = ComGuard::init()?;
        create_engine(&[])?;
        Ok(Self)
    }
}

impl OcrEngine for WindowsOcrEngine {
    fn name(&self) -> &'static str {
        "Windows OCR"
    }

    fn recognize(&self, image: &image::DynamicImage, languages: &[String]) -> Result<OcrPage> {
        let _com = ComGuard::init()?;
        let engine = create_engine(languages)?;

        // The engine rejects images larger than MaxImageDimension on either
        // side (10 000 px today; a 600 dpi Legal page is 5100 × 8400).
        let (width, height) = (image.width(), image.height());
        let max = WinOcrEngine::MaxImageDimension().unwrap_or(10_000).max(1);
        let scale = (max as f32 / width.max(height) as f32).min(1.0);
        let scaled;
        let source = if scale < 1.0 {
            let w = ((width as f32 * scale) as u32).max(1);
            let h = ((height as f32 * scale) as u32).max(1);
            scaled = image.resize_exact(w, h, FilterType::Triangle);
            &scaled
        } else {
            image
        };

        let bitmap = to_software_bitmap(source)?;
        let result = engine.RecognizeAsync(&bitmap)?.join().context("Windows OCR failed")?;

        let unscale = 1.0 / scale;
        let mut lines = Vec::new();
        for line in result.Lines()? {
            let mut words = Vec::new();
            for word in line.Words()? {
                let text = word.Text()?.to_string_lossy();
                if text.is_empty() {
                    continue;
                }
                let r = word.BoundingRect()?;
                words.push(OcrWord {
                    text,
                    bbox: Rect { x: r.X * unscale, y: r.Y * unscale, w: r.Width * unscale, h: r.Height * unscale },
                });
            }
            if words.is_empty() {
                continue;
            }
            let bbox = union(words.iter().map(|w| w.bbox));
            lines.push(OcrLine { words, bbox });
        }
        Ok(OcrPage { width, height, lines })
    }
}

/// The first requested language with an installed recognizer — matched
/// exactly, then by primary subtag ("de" finds "de-DE") — else the user's
/// profile languages.
fn create_engine(languages: &[String]) -> Result<WinOcrEngine> {
    let available: Vec<Language> =
        WinOcrEngine::AvailableRecognizerLanguages().map(|v| v.into_iter().collect()).unwrap_or_default();
    let tag_of = |l: &Language| l.LanguageTag().map(|t| t.to_string_lossy()).unwrap_or_default();

    for wanted in languages {
        if let Ok(lang) = Language::CreateLanguage(&HSTRING::from(wanted.as_str()))
            && WinOcrEngine::IsLanguageSupported(&lang).unwrap_or(false)
            && let Ok(engine) = WinOcrEngine::TryCreateFromLanguage(&lang)
        {
            return Ok(engine);
        }
        let primary = wanted.split(['-', '_']).next().unwrap_or(wanted).to_ascii_lowercase();
        let fallback =
            available.iter().find(|l| tag_of(l).split('-').next().is_some_and(|p| p.eq_ignore_ascii_case(&primary)));
        if let Some(lang) = fallback
            && let Ok(engine) = WinOcrEngine::TryCreateFromLanguage(lang)
        {
            return Ok(engine);
        }
    }
    WinOcrEngine::TryCreateFromUserProfileLanguages().map_err(|_| {
        anyhow!(
            "No Windows OCR language is installed — add one under Settings › Time & language › \
             Language (with \"Optical character recognition\")"
        )
    })
}

fn to_software_bitmap(image: &image::DynamicImage) -> Result<SoftwareBitmap> {
    let rgba = image.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut bgra = rgba.into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let writer = DataWriter::new()?;
    writer.WriteBytes(&bgra)?;
    let buffer = writer.DetachBuffer()?;
    Ok(SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, w as i32, h as i32)?)
}

fn union(rects: impl Iterator<Item = Rect>) -> Rect {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for r in rects {
        x0 = x0.min(r.x);
        y0 = y0.min(r.y);
        x1 = x1.max(r.x + r.w);
        y1 = y1.max(r.y + r.h);
    }
    if x0 > x1 { Rect::default() } else { Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 } }
}
