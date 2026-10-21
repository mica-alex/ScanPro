//! Scanner access. Each [`Backend`] wraps one platform scanning API; the rest
//! of the app only ever sees [`DeviceInfo`], [`Device`] and [`ScannedImage`].
//!
//! | Backend | Platforms | Covers |
//! |---|---|---|
//! | `sane`  | Linux, macOS (Homebrew `sane-backends`) | USB/network scanners with a SANE backend (ES-300W: `epsonds`) |
//! | `ica`   | macOS | Anything with an ImageCaptureCore driver (vendor ICA drivers, AirScan) |
//! | `wia`   | Windows | Anything with a WIA driver (nearly every scanner on Windows) |
//! | `escl`  | all | Network scanners speaking eSCL/AirScan, found over mDNS |
//!
//! A device id is `"<backend>:<backend-specific id>"`, so [`open`] can route
//! it back to the backend that found it without re-discovering.

pub mod escl;
#[cfg(target_os = "macos")]
pub mod ica;
pub mod sane;
#[cfg(windows)]
pub mod wia;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    Flatbed,
    #[default]
    Feeder,
    FeederDuplex,
}

impl Source {
    pub const ALL: [Source; 3] = [Source::Flatbed, Source::Feeder, Source::FeederDuplex];

    pub fn label(self) -> &'static str {
        match self {
            Source::Flatbed => "Flatbed",
            Source::Feeder => "Feeder (single-sided)",
            Source::FeederDuplex => "Feeder (double-sided)",
        }
    }

    pub fn is_feeder(self) -> bool {
        !matches!(self, Source::Flatbed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ColorMode {
    #[default]
    Color,
    Gray,
    /// 1-bit line art. Smallest files, best for text-only pages.
    BlackWhite,
}

impl ColorMode {
    pub const ALL: [ColorMode; 3] = [ColorMode::Color, ColorMode::Gray, ColorMode::BlackWhite];

    pub fn label(self) -> &'static str {
        match self {
            ColorMode::Color => "Color",
            ColorMode::Gray => "Grayscale",
            ColorMode::BlackWhite => "Black & white",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PaperSize {
    /// Let the scanner detect the page edges (hardware auto-crop) when it
    /// can; otherwise scan the full area.
    #[default]
    Auto,
    Letter,
    Legal,
    A4,
    A5,
    /// The scanner's entire scan area.
    Maximum,
}

impl PaperSize {
    pub const ALL: [PaperSize; 6] =
        [PaperSize::Auto, PaperSize::Letter, PaperSize::Legal, PaperSize::A4, PaperSize::A5, PaperSize::Maximum];

    pub fn label(self) -> &'static str {
        match self {
            PaperSize::Auto => "Auto-detect",
            PaperSize::Letter => "Letter (8.5 × 11 in)",
            PaperSize::Legal => "Legal (8.5 × 14 in)",
            PaperSize::A4 => "A4",
            PaperSize::A5 => "A5",
            PaperSize::Maximum => "Maximum area",
        }
    }

    /// Width × height in millimetres, or `None` for "whatever the scanner gives".
    pub fn size_mm(self) -> Option<(f64, f64)> {
        match self {
            PaperSize::Letter => Some((215.9, 279.4)),
            PaperSize::Legal => Some((215.9, 355.6)),
            PaperSize::A4 => Some((210.0, 297.0)),
            PaperSize::A5 => Some((148.0, 210.0)),
            PaperSize::Auto | PaperSize::Maximum => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanSettings {
    pub source: Source,
    pub color: ColorMode,
    pub dpi: u32,
    pub paper: PaperSize,
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self { source: Source::Feeder, color: ColorMode::Color, dpi: 300, paper: PaperSize::Auto }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// `"<backend>:<backend-specific id>"`; stable across launches where the
    /// backend allows it, so profiles can remember a device.
    pub id: String,
    /// Human-readable name, e.g. "EPSON ES-300W".
    pub name: String,
    /// Short backend label shown next to the name, e.g. "SANE" or "WIA".
    pub backend: String,
}

/// What a device can do. Anything a backend cannot determine is left at the
/// permissive default rather than guessed narrower.
#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    pub sources: Vec<Source>,
    pub color_modes: Vec<ColorMode>,
    /// Discrete resolutions in dpi, ascending. Ranges are expanded to the
    /// usual stops (75, 100, 150, 200, 300, 400, 600, 1200) inside the range.
    pub resolutions: Vec<u32>,
    /// Scanner-side page-edge detection, used for [`PaperSize::Auto`].
    pub auto_crop: bool,
}

/// The usual resolution stops, for backends that report a range.
pub const STANDARD_DPI: [u32; 8] = [75, 100, 150, 200, 300, 400, 600, 1200];

pub fn standard_dpi_in(min: u32, max: u32) -> Vec<u32> {
    STANDARD_DPI.iter().copied().filter(|d| (min..=max).contains(d)).collect()
}

/// One page as it came off the scanner.
pub enum ScannedImage {
    /// Raw pixels (SANE, WIA): `Luma8` for gray/line-art, `Rgb8` for colour.
    Pixels { image: image::DynamicImage, dpi: u32, bilevel: bool },
    /// An already-encoded file (eSCL, ICA). JPEG bytes are passed straight
    /// through to the PDF without a re-encode.
    Encoded { bytes: Vec<u8>, format: image::ImageFormat, dpi: u32 },
}

impl ScannedImage {
    pub fn dpi(&self) -> u32 {
        match self {
            ScannedImage::Pixels { dpi, .. } | ScannedImage::Encoded { dpi, .. } => *dpi,
        }
    }
}

/// Cooperative cancellation shared between the UI and a running scan.
#[derive(Clone, Default, Debug)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub trait Device: Send {
    fn info(&self) -> &DeviceInfo;
    fn capabilities(&mut self) -> Result<Capabilities>;
    /// Scan until the feeder is empty (or one page for the flatbed), handing
    /// each page to `on_page` as soon as it is complete so the UI and OCR can
    /// start on page 1 while page 2 is still feeding. Returns the page count.
    ///
    /// An empty feeder at the *start* is an error ("load paper"); running out
    /// mid-batch is the normal end of a feeder scan.
    fn scan(
        &mut self,
        settings: &ScanSettings,
        cancel: &CancelToken,
        on_page: &mut dyn FnMut(ScannedImage) -> Result<()>,
    ) -> Result<usize>;
}

pub trait Backend: Send + Sync {
    /// The id prefix, e.g. `"sane"`.
    fn id(&self) -> &'static str;
    /// Whether the backend can run at all here (e.g. libsane is installed).
    fn available(&self) -> bool;
    fn discover(&self) -> Result<Vec<DeviceInfo>>;
    /// `id` is the part after `"<backend>:"`.
    fn open(&self, id: &str) -> Result<Box<dyn Device>>;
}

// Built up with per-platform `push`es; a `vec![]` literal can't take `#[cfg]`.
#[allow(clippy::vec_init_then_push)]
pub fn backends() -> Vec<Box<dyn Backend>> {
    let mut list: Vec<Box<dyn Backend>> = Vec::new();
    #[cfg(target_os = "macos")]
    list.push(Box::new(ica::IcaBackend::new()));
    #[cfg(windows)]
    list.push(Box::new(wia::WiaBackend::new()));
    list.push(Box::new(sane::SaneBackend::new()));
    list.push(Box::new(escl::EsclBackend::new()));
    list
}

/// Discover devices on every available backend, in parallel (network
/// discovery waits on mDNS; it should not hold up a USB scanner).
pub fn discover_all() -> Vec<DeviceInfo> {
    let backends = backends();
    let run = |b: &dyn Backend| match b.discover() {
        Ok(list) => list,
        Err(e) => {
            log::warn!("{} discovery failed: {e:#}", b.id());
            Vec::new()
        }
    };
    let mut found: Vec<DeviceInfo> = std::thread::scope(|s| {
        let mut available = backends.iter().filter(|b| b.available());
        // The first backend runs on the calling thread: on macOS that's ICA,
        // which needs the caller's run loop when the caller is the main
        // thread (the CLI) — a spawned thread would wait on a main thread
        // that is itself blocked in `join`.
        let first = available.next();
        let handles: Vec<_> = available.map(|b| s.spawn(move || run(b.as_ref()))).collect();
        let mut all = first.map(|b| run(b.as_ref())).unwrap_or_default();
        all.extend(handles.into_iter().flat_map(|h| h.join().unwrap_or_default()));
        all
    });
    found.sort_by_key(|a| a.name.to_lowercase());
    found
}

pub fn open(device_id: &str) -> Result<Box<dyn Device>> {
    let (prefix, rest) = device_id.split_once(':').ok_or_else(|| anyhow!("malformed device id {device_id:?}"))?;
    let backend = backends()
        .into_iter()
        .find(|b| b.id() == prefix)
        .ok_or_else(|| anyhow!("no scanner backend {prefix:?} on this platform"))?;
    backend.open(rest)
}
