//! Text recognition. Platform engines are preferred because they are better
//! and cost nothing to ship; `ocrs` is the pure-Rust engine everywhere else.
//!
//! All engines report boxes in **pixels of the image they were given, origin
//! top-left, y down**. The PDF writer is the only place that flips to PDF's
//! bottom-left origin.

#[cfg(feature = "ocrs-engine")]
pub mod ocrs_engine;
#[cfg(target_os = "macos")]
pub mod vision;
#[cfg(windows)]
pub mod winrt;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OcrWord {
    pub text: String,
    pub bbox: Rect,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OcrLine {
    pub words: Vec<OcrWord>,
    pub bbox: Rect,
}

impl OcrLine {
    pub fn text(&self) -> String {
        self.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OcrPage {
    /// Size of the image the boxes refer to.
    pub width: u32,
    pub height: u32,
    /// In reading order.
    pub lines: Vec<OcrLine>,
}

impl OcrPage {
    pub fn text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.text());
            out.push('\n');
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EngineKind {
    /// The best engine available on this platform.
    #[default]
    Auto,
    /// macOS Vision framework.
    Vision,
    /// Windows.Media.Ocr (built into Windows 10/11).
    Windows,
    /// ocrs, pure Rust.
    Ocrs,
}

impl EngineKind {
    pub fn label(self) -> &'static str {
        match self {
            EngineKind::Auto => "Automatic",
            EngineKind::Vision => "Apple Vision",
            EngineKind::Windows => "Windows OCR",
            EngineKind::Ocrs => "ocrs (built-in)",
        }
    }

    /// Engines compiled into this build, `Auto` first.
    pub fn available() -> Vec<EngineKind> {
        let mut v = vec![EngineKind::Auto];
        #[cfg(target_os = "macos")]
        v.push(EngineKind::Vision);
        #[cfg(windows)]
        v.push(EngineKind::Windows);
        #[cfg(feature = "ocrs-engine")]
        v.push(EngineKind::Ocrs);
        v
    }
}

pub trait OcrEngine: Send + Sync {
    fn name(&self) -> &'static str;
    /// `languages` are BCP-47 tags in priority order (e.g. `["en-US", "de-DE"]`);
    /// empty means the engine's default. Engines ignore tags they lack.
    fn recognize(&self, image: &image::DynamicImage, languages: &[String]) -> Result<OcrPage>;
}

pub fn create(kind: EngineKind) -> Result<Arc<dyn OcrEngine>> {
    match kind {
        EngineKind::Auto => {
            #[cfg(target_os = "macos")]
            return Ok(Arc::new(vision::VisionEngine::new()));
            #[cfg(windows)]
            return Ok(Arc::new(winrt::WindowsOcrEngine::new()?));
            #[allow(unreachable_code)]
            create(EngineKind::Ocrs)
        }
        #[cfg(target_os = "macos")]
        EngineKind::Vision => Ok(Arc::new(vision::VisionEngine::new())),
        #[cfg(windows)]
        EngineKind::Windows => Ok(Arc::new(winrt::WindowsOcrEngine::new()?)),
        #[cfg(feature = "ocrs-engine")]
        EngineKind::Ocrs => Ok(Arc::new(ocrs_engine::OcrsEngine::load()?)),
        #[allow(unreachable_patterns)]
        other => bail!("{} is not available in this build", other.label()),
    }
}

/// A process-wide engine per kind. Engines can be expensive to set up (ocrs
/// loads ~12 MB of models), so the GUI and CLI share one of each.
pub fn shared(kind: EngineKind) -> Result<Arc<dyn OcrEngine>> {
    static CACHE: Mutex<Option<HashMap<EngineKind, Arc<dyn OcrEngine>>>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let map = cache.get_or_insert_with(HashMap::new);
    if let Some(engine) = map.get(&kind) {
        return Ok(engine.clone());
    }
    let engine = create(kind)?;
    map.insert(kind, engine.clone());
    Ok(engine)
}
