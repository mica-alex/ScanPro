//! Persistent settings. Everything a scan needs — device, scan settings,
//! processing, OCR, output — lives in a named [`Profile`], so "receipts to a
//! folder as B&W PDFs" and "contracts double-sided, ask where to save" are
//! one click (or one `--profile` flag) apart.
//!
//! Stored as TOML at `<config dir>/config.toml`. Unknown or missing keys fall
//! back to defaults, so older files keep loading as fields are added.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ocr::EngineKind;
use crate::scanner::ScanSettings;

pub fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("com", "mica-alex", "ScanPro")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// PDF with an invisible OCR text layer (selectable, searchable).
    #[default]
    SearchablePdf,
    /// Image-only PDF.
    Pdf,
    /// One multi-page TIFF.
    Tiff,
    /// One PNG per page.
    Png,
    /// One JPEG per page.
    Jpeg,
    /// OCR text only.
    Text,
}

impl OutputFormat {
    pub const ALL: [OutputFormat; 6] = [
        OutputFormat::SearchablePdf,
        OutputFormat::Pdf,
        OutputFormat::Tiff,
        OutputFormat::Png,
        OutputFormat::Jpeg,
        OutputFormat::Text,
    ];

    pub fn label(self) -> &'static str {
        match self {
            OutputFormat::SearchablePdf => "PDF (searchable)",
            OutputFormat::Pdf => "PDF (image only)",
            OutputFormat::Tiff => "TIFF (multi-page)",
            OutputFormat::Png => "PNG (one per page)",
            OutputFormat::Jpeg => "JPEG (one per page)",
            OutputFormat::Text => "Plain text (OCR)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::SearchablePdf | OutputFormat::Pdf => "pdf",
            OutputFormat::Tiff => "tif",
            OutputFormat::Png => "png",
            OutputFormat::Jpeg => "jpg",
            OutputFormat::Text => "txt",
        }
    }

    pub fn needs_ocr(self) -> bool {
        matches!(self, OutputFormat::SearchablePdf | OutputFormat::Text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SaveMode {
    /// Show a save dialog, pre-filled with the folder and suggested name.
    #[default]
    Ask,
    /// Save straight into the output folder with the name template — for
    /// back-to-back documents.
    Auto,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Processing {
    /// Drop pages with (almost) no ink — the blank backs of a duplex scan.
    pub skip_blank: bool,
    /// Percentage of the page that must be "ink" for it to count as content.
    pub blank_threshold_percent: f32,
    /// Quality for pages stored and exported as JPEG (colour/grayscale).
    pub jpeg_quality: u8,
}

impl Default for Processing {
    fn default() -> Self {
        Self { skip_blank: true, blank_threshold_percent: 0.2, jpeg_quality: 85 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OcrSettings {
    pub enabled: bool,
    pub engine: EngineKind,
    /// BCP-47 tags in priority order; empty = engine default / auto-detect.
    pub languages: Vec<String>,
    /// Turn pages fed upside down or sideways upright (judged from the OCR result).
    pub fix_orientation: bool,
}

impl Default for OcrSettings {
    fn default() -> Self {
        Self { enabled: true, engine: EngineKind::Auto, languages: Vec::new(), fix_orientation: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LlmApi {
    /// Ollama's native API.
    #[default]
    Ollama,
    /// LM Studio: OpenAI-style chat plus its own API for listing models
    /// with their state and loading them.
    LmStudio,
    /// Any OpenAI-compatible `/v1/chat/completions` (llama.cpp server,
    /// LocalAI, vLLM…).
    OpenAi,
}

impl LlmApi {
    pub const ALL: [LlmApi; 3] = [LlmApi::Ollama, LlmApi::LmStudio, LlmApi::OpenAi];

    pub fn label(self) -> &'static str {
        match self {
            LlmApi::Ollama => "Ollama",
            LlmApi::LmStudio => "LM Studio",
            LlmApi::OpenAi => "OpenAI-compatible (llama.cpp, LocalAI, vLLM…)",
        }
    }
}

/// What the model is given to name a document from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LlmInput {
    /// The OCR text. Fastest, works with every model.
    #[default]
    Text,
    /// The OCR text plus a picture of page 1 (vision models): logos and
    /// layout help with letterheads the OCR reads poorly.
    TextAndImage,
    /// Only the picture of page 1 (vision models).
    Image,
}

impl LlmInput {
    pub const ALL: [LlmInput; 3] = [LlmInput::Text, LlmInput::TextAndImage, LlmInput::Image];

    pub fn label(self) -> &'static str {
        match self {
            LlmInput::Text => "OCR text (fastest)",
            LlmInput::TextAndImage => "Page image + OCR text (vision models)",
            LlmInput::Image => "Page image only (vision models)",
        }
    }

    pub fn wants_image(self) -> bool {
        self != LlmInput::Text
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamingSettings {
    /// Suggest a name from the OCR text after each scan.
    pub suggest: bool,
    /// Ask a local language model instead of (falling back to) the built-in
    /// heuristics. Nothing leaves the machine unless the endpoint does.
    pub use_llm: bool,
    pub llm_api: LlmApi,
    pub llm_endpoint: String,
    pub llm_model: String,
    pub llm_input: LlmInput,
    /// Load the model through the server's API when it isn't loaded
    /// (Ollama and LM Studio).
    pub llm_auto_load: bool,
}

impl Default for NamingSettings {
    fn default() -> Self {
        Self {
            suggest: true,
            use_llm: false,
            llm_api: LlmApi::Ollama,
            llm_endpoint: "http://localhost:11434".into(),
            llm_model: "llama3.2".into(),
            llm_input: LlmInput::Text,
            llm_auto_load: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputSettings {
    pub format: OutputFormat,
    /// Where documents go. Empty = `~/Documents/ScanPro`.
    pub folder: PathBuf,
    /// File name template without extension; see [`crate::naming::render`].
    pub name_template: String,
    pub save_mode: SaveMode,
    /// Save as soon as a scan (and its OCR) finishes, without pressing Save —
    /// scan, swap paper, scan again.
    pub save_after_scan: bool,
    /// Clear the page list after a successful save, ready for the next document.
    pub clear_after_save: bool,
    pub open_after_save: bool,
    pub reveal_after_save: bool,
    /// Shell command run after each save; `{path}` is replaced by the saved
    /// file (quoted). For hand-offs to scripts, sync tools, printers…
    pub after_save_command: String,
}

/// The document's own date (from the OCR text, else today) and the
/// suggested name: "2026-08-19 Valley Medical Receipt".
pub const DEFAULT_NAME_TEMPLATE: &str = "{doc_date} {suggested}";

/// Earlier default templates, upgraded on load. A template the user typed
/// themselves is never touched.
const OLD_DEFAULT_NAME_TEMPLATES: [&str; 1] = ["{date} {suggested}"];

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            format: OutputFormat::SearchablePdf,
            folder: PathBuf::new(),
            name_template: DEFAULT_NAME_TEMPLATE.into(),
            save_mode: SaveMode::Ask,
            save_after_scan: false,
            clear_after_save: true,
            open_after_save: false,
            reveal_after_save: false,
            after_save_command: String::new(),
        }
    }
}

impl OutputSettings {
    pub fn resolved_folder(&self) -> PathBuf {
        if self.folder.as_os_str().is_empty() { default_output_folder() } else { expand_home(&self.folder) }
    }
}

pub fn default_output_folder() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(Path::to_path_buf))
        .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ScanPro")
}

fn expand_home(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => directories::BaseDirs::new().map(|b| b.home_dir().join(rest)).unwrap_or_else(|| p.into()),
        Err(_) => p.into(),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    /// Device to use; `None` = whatever is selected in the app.
    pub device: Option<String>,
    pub scan: ScanSettings,
    pub processing: Processing,
    pub ocr: OcrSettings,
    pub naming: NamingSettings,
    pub output: OutputSettings,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "Document".into(),
            device: None,
            scan: ScanSettings::default(),
            processing: Processing::default(),
            ocr: OcrSettings::default(),
            naming: NamingSettings::default(),
            output: OutputSettings::default(),
        }
    }
}

impl Profile {
    /// The profiles a fresh install starts with.
    pub fn builtin() -> Vec<Profile> {
        use crate::scanner::{ColorMode, Source};
        let base = Profile::default();
        vec![
            Profile { name: "Document (searchable PDF)".into(), ..base.clone() },
            Profile {
                name: "Double-sided document".into(),
                scan: ScanSettings { source: Source::FeederDuplex, ..base.scan.clone() },
                ..base.clone()
            },
            Profile {
                name: "Batch to folder (auto-save)".into(),
                scan: ScanSettings { source: Source::FeederDuplex, color: ColorMode::Gray, ..base.scan.clone() },
                output: OutputSettings { save_mode: SaveMode::Auto, save_after_scan: true, ..base.output.clone() },
                ..base.clone()
            },
            Profile {
                name: "Black & white archive".into(),
                scan: ScanSettings { color: ColorMode::BlackWhite, ..base.scan.clone() },
                ..base.clone()
            },
            Profile {
                name: "Photo / image".into(),
                scan: ScanSettings { source: Source::Flatbed, dpi: 600, ..base.scan.clone() },
                processing: Processing { skip_blank: false, jpeg_quality: 92, ..base.processing.clone() },
                ocr: OcrSettings { enabled: false, ..base.ocr.clone() },
                naming: NamingSettings { suggest: false, ..base.naming.clone() },
                output: OutputSettings {
                    format: OutputFormat::Jpeg,
                    name_template: "Scan {datetime}".into(),
                    ..base.output.clone()
                },
                ..base
            },
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Last device used, re-selected on launch.
    pub last_device: Option<String>,
    /// Name of the profile selected on launch.
    pub active_profile: String,
    pub profiles: Vec<Profile>,
    /// Next value of `{counter}` in name templates.
    pub counter: u64,
}

impl Default for Config {
    fn default() -> Self {
        let profiles = Profile::builtin();
        Self { last_device: None, active_profile: profiles[0].name.clone(), profiles, counter: 1 }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        std::env::var_os("SCANPRO_CONFIG")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| project_dirs().map(|d| d.config_dir().join("config.toml")))
    }

    /// Load, or create the default config. A file that fails to parse is
    /// moved aside rather than silently overwritten.
    pub fn load() -> Config {
        let Some(path) = Self::path() else { return Config::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(mut cfg) => {
                    if cfg.profiles.is_empty() {
                        cfg.profiles = Profile::builtin();
                    }
                    if cfg.upgrade_defaults() {
                        // Persist right away so the file matches what the app shows.
                        let _ = cfg.save();
                    }
                    cfg
                }
                Err(e) => {
                    log::error!("{} is invalid ({e}); starting from defaults", path.display());
                    let _ = std::fs::rename(&path, path.with_extension("toml.invalid"));
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path().context("no config directory on this system")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)?;
        // Write-then-rename so a crash mid-save can't truncate the config.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Move profiles still on an old default to the current one. Returns
    /// whether anything changed.
    fn upgrade_defaults(&mut self) -> bool {
        let mut changed = false;
        for p in &mut self.profiles {
            if OLD_DEFAULT_NAME_TEMPLATES.contains(&p.output.name_template.as_str()) {
                p.output.name_template = DEFAULT_NAME_TEMPLATE.into();
                changed = true;
            }
        }
        changed
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn active(&self) -> Profile {
        self.profile(&self.active_profile).or(self.profiles.first()).cloned().unwrap_or_default()
    }

    /// Insert or replace a profile by name.
    pub fn upsert(&mut self, profile: Profile) {
        match self.profiles.iter_mut().find(|p| p.name == profile.name) {
            Some(slot) => *slot = profile,
            None => self.profiles.push(profile),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn upgrades_old_default_template_but_not_custom_ones() {
        let mut cfg: Config = toml::from_str(
            "[[profiles]]\nname = \"A\"\n[profiles.output]\nname_template = \"{date} {suggested}\"\n\
             [[profiles]]\nname = \"B\"\n[profiles.output]\nname_template = \"{date} receipts/{title}\"\n",
        )
        .unwrap();
        assert!(cfg.upgrade_defaults());
        assert!(!cfg.upgrade_defaults(), "second pass is a no-op");
        assert_eq!(cfg.profiles[0].output.name_template, DEFAULT_NAME_TEMPLATE);
        assert_eq!(cfg.profiles[1].output.name_template, "{date} receipts/{title}");
    }

    #[test]
    fn missing_keys_take_defaults() {
        let cfg: Config = toml::from_str("active_profile = \"X\"\n[[profiles]]\nname = \"X\"\n").unwrap();
        let p = cfg.active();
        assert_eq!(p.name, "X");
        assert_eq!(p.scan, ScanSettings::default());
        assert!(p.ocr.enabled);
    }
}
