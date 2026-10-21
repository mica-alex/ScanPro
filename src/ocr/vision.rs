//! Apple Vision OCR (`VNRecognizeTextRequest`), the default engine on macOS.
//!
//! Vision works on observations (≈ lines) in normalised coordinates with the
//! origin bottom-left. Word boxes come from `boundingBoxForRange:` over each
//! whitespace-separated run of the line's top candidate, and everything is
//! converted to the top-left pixel convention of [`super`] before it leaves
//! this file.

use std::io::Cursor;

use anyhow::{Context, Result, anyhow};
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_foundation::CGRect;
use objc2_foundation::{NSArray, NSData, NSDictionary, NSRange, NSString};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedText, VNRequest, VNRequestTextRecognitionLevel,
};

use super::{OcrEngine, OcrLine, OcrPage, OcrWord, Rect};

/// Stateless: Vision objects are created per call inside an autorelease pool,
/// which is what makes this safe to share across the OCR worker threads.
pub struct VisionEngine;

impl VisionEngine {
    pub fn new() -> Self {
        Self
    }
}

impl Default for VisionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl OcrEngine for VisionEngine {
    fn name(&self) -> &'static str {
        "Apple Vision"
    }

    fn recognize(&self, image: &image::DynamicImage, languages: &[String]) -> Result<OcrPage> {
        let (width, height) = (image.width(), image.height());
        // Uncompressed grayscale TIFF: colour adds nothing to recognition, and
        // skipping compression keeps the hand-off to Vision cheap.
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(opaque_luma(image))
            .write_to(&mut encoded, image::ImageFormat::Tiff)
            .context("encoding page for Vision")?;
        let encoded = encoded.into_inner();

        let mut lines = autoreleasepool(|_| recognize_encoded(&encoded, width, height, languages))?;
        // Vision's order is close to reading order but not guaranteed; the
        // contract is top-to-bottom, left-to-right within a row.
        lines.sort_by(|a, b| a.bbox.y.total_cmp(&b.bbox.y).then(a.bbox.x.total_cmp(&b.bbox.x)));
        Ok(OcrPage { width, height, lines })
    }
}

fn recognize_encoded(encoded: &[u8], width: u32, height: u32, languages: &[String]) -> Result<Vec<OcrLine>> {
    let data = NSData::with_bytes(encoded);
    let options = NSDictionary::new();
    let handler = VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &data, &options);

    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    if languages.is_empty() {
        request.setAutomaticallyDetectsLanguage(true);
    } else {
        let tags: Vec<Retained<NSString>> = languages.iter().map(|l| NSString::from_str(l)).collect();
        request.setRecognitionLanguages(&NSArray::from_retained_slice(&tags));
    }

    let as_request: &VNRequest = &request;
    handler
        .performRequests_error(&NSArray::from_slice(&[as_request]))
        .map_err(|e| anyhow!("Vision text recognition failed: {}", e.localizedDescription()))?;

    let Some(observations) = request.results() else {
        return Ok(Vec::new());
    };
    let (w, h) = (width as f32, height as f32);
    let mut lines = Vec::with_capacity(observations.count());
    for obs in observations.iter() {
        let candidates = obs.topCandidates(1);
        let Some(top) = candidates.firstObject() else { continue };
        let line_box = to_pixels(unsafe { obs.boundingBox() }, w, h);
        let words = split_words(&top, line_box, w, h);
        if !words.is_empty() {
            lines.push(OcrLine { words, bbox: line_box });
        }
    }
    Ok(lines)
}

/// Grayscale with any alpha flattened onto white. A plain `to_luma8` drops
/// alpha, turning a transparent background black and the page unreadable.
fn opaque_luma(image: &image::DynamicImage) -> image::GrayImage {
    if !image.color().has_alpha() {
        return image.to_luma8();
    }
    let la = image.to_luma_alpha8();
    image::GrayImage::from_fn(la.width(), la.height(), |x, y| {
        let [l, a] = la.get_pixel(x, y).0;
        let (l, a) = (l as u32, a as u32);
        image::Luma([((l * a + 255 * (255 - a)) / 255) as u8])
    })
}

/// Whitespace-separated runs of `s` as `(text, utf16_start, utf16_len)`;
/// `NSRange` counts UTF-16 code units, not bytes or chars.
fn word_ranges(s: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start = 0;
    let mut pos = 0;
    for c in s.chars() {
        let units = c.len_utf16();
        if c.is_whitespace() {
            if !current.is_empty() {
                out.push((std::mem::take(&mut current), start, pos - start));
            }
            start = pos + units;
        } else {
            if current.is_empty() {
                start = pos;
            }
            current.push(c);
        }
        pos += units;
    }
    if !current.is_empty() {
        out.push((current, start, pos - start));
    }
    out
}

fn split_words(top: &VNRecognizedText, line_box: Rect, w: f32, h: f32) -> Vec<OcrWord> {
    let text = top.string().to_string();
    let ranges = word_ranges(&text);
    let total_units = text.encode_utf16().count().max(1) as f32;
    ranges
        .into_iter()
        .map(|(word, start, len)| {
            let exact = unsafe { top.boundingBoxForRange_error(NSRange::new(start, len)) }
                .ok()
                .map(|r| to_pixels(unsafe { r.boundingBox() }, w, h))
                .filter(|r| r.w > 0.0 && r.h > 0.0);
            // Proportional split of the line box: crude, but it keeps the
            // text layer selectable where Vision declines to give a box.
            let bbox = exact.unwrap_or(Rect {
                x: line_box.x + line_box.w * start as f32 / total_units,
                y: line_box.y,
                w: line_box.w * len as f32 / total_units,
                h: line_box.h,
            });
            OcrWord { text: word, bbox }
        })
        .collect()
}

/// Normalised, origin bottom-left → pixels, origin top-left.
fn to_pixels(r: CGRect, w: f32, h: f32) -> Rect {
    let (x, y) = (r.origin.x as f32, r.origin.y as f32);
    let (rw, rh) = (r.size.width as f32, r.size.height as f32);
    Rect { x: x * w, y: (1.0 - y - rh) * h, w: rw * w, h: rh * h }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::{CGPoint, CGSize};

    #[test]
    fn flips_to_top_left_pixels() {
        let r = CGRect::new(CGPoint::new(0.1, 0.7), CGSize::new(0.5, 0.2));
        let px = to_pixels(r, 1000.0, 2000.0);
        assert_eq!((px.x, px.w), (100.0, 500.0));
        assert!((px.y - 200.0).abs() < 1e-3, "top edge {}", px.y);
        assert!((px.h - 400.0).abs() < 1e-3);
    }

    #[test]
    fn word_ranges_count_utf16_units() {
        let r = word_ranges("  a😀b  cd ");
        assert_eq!(r, vec![("a😀b".into(), 2, 4), ("cd".into(), 8, 2)]);
    }

    /// Needs a real Vision runtime; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn recognizes_a_blank_page_without_error() {
        let img = image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(400, 200, image::Luma([255])));
        let page = VisionEngine::new().recognize(&img, &[]).unwrap();
        assert_eq!((page.width, page.height), (400, 200));
        assert!(page.lines.is_empty());
    }
}
