//! The steps from "pages in hand" to "document on disk", shared by the GUI
//! and the CLI: finish OCR, suggest a name, pick the destination, export, and
//! run the after-save actions.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use rayon::prelude::*;

use crate::config::{OcrSettings, OutputSettings, Profile};
use crate::export::{self, ExportOptions};
use crate::naming::{self, NameContext, Suggestion};
use crate::ocr;
use crate::pipeline::{self, Page};

/// OCR every page that doesn't have a result yet, in parallel. Pages that
/// fail keep `ocr: None` (they still export, just without text) and the
/// first error is logged rather than aborting the document.
pub fn ensure_ocr(pages: &mut [Page], settings: &OcrSettings) -> Result<()> {
    if pages.iter().all(|p| p.ocr.is_some()) {
        return Ok(());
    }
    let engine = ocr::shared(settings.engine)?;
    pages.par_iter_mut().filter(|p| p.ocr.is_none()).for_each(|page| match ocr_page(page, engine.as_ref(), settings) {
        Ok((rotation, result)) => {
            if rotation != page.rotation {
                page.rotate_by(rotation as i32 - page.rotation as i32);
            }
            page.ocr = Some(Arc::new(result));
        }
        Err(e) => log::warn!("OCR failed on page {}: {e:#}", page.id),
    });
    Ok(())
}

/// OCR one page, fixing its orientation if enabled and the user hasn't
/// rotated it by hand. Returns the rotation the result belongs to (which may
/// differ from the page's).
pub fn ocr_page(page: &Page, engine: &dyn ocr::OcrEngine, settings: &OcrSettings) -> Result<(u16, ocr::OcrPage)> {
    if settings.fix_orientation && !page.rotation_locked {
        pipeline::run_ocr_oriented(page, engine, &settings.languages)
    } else {
        Ok((page.rotation, pipeline::run_ocr(page, engine, &settings.languages)?))
    }
}

pub fn suggest(pages: &[Page], profile: &Profile) -> Option<Suggestion> {
    if !profile.naming.suggest {
        return None;
    }
    let ocr: Vec<&ocr::OcrPage> = pages.iter().filter_map(|p| p.ocr.as_deref()).collect();
    // Only render page 1 when a vision mode will actually use it.
    let naming = &profile.naming;
    let image = (naming.use_llm && naming.llm_input.wants_image())
        .then(|| pages.first())
        .flatten()
        .and_then(|p| p.load().ok())
        .and_then(|img| crate::llm::page_jpeg(&img).ok());
    naming::suggest(&ocr, naming, image.as_deref())
}

/// The name (relative path, no extension) the template produces.
pub fn file_name(
    output: &OutputSettings,
    profile_name: &str,
    scanner: &str,
    suggestion: Option<&Suggestion>,
    counter: u64,
    pages: usize,
) -> PathBuf {
    naming::render(
        &output.name_template,
        &NameContext { now: jiff::Zoned::now(), suggestion, counter, pages, profile: profile_name, scanner },
    )
}

/// `folder/name.ext`, made unique so back-to-back saves never overwrite.
pub fn destination(folder: &Path, name: &Path, output: &OutputSettings) -> PathBuf {
    let mut file = folder.join(name).into_os_string();
    file.push(".");
    file.push(output.format.extension());
    export::unique_path(Path::new(&file))
}

pub struct Saved {
    pub files: Vec<PathBuf>,
    /// Non-fatal problems (after-save command failed, couldn't open…).
    pub warnings: Vec<String>,
}

/// Export and run the after-save actions. OCR must already be done for
/// formats that need it (see [`ensure_ocr`]).
pub fn save(pages: &[Page], profile: &Profile, dest: &Path, title: Option<String>) -> Result<Saved> {
    let opts = ExportOptions { format: profile.output.format, jpeg_quality: profile.processing.jpeg_quality, title };
    let files = export::export(pages, &opts, dest)?;
    let mut warnings = Vec::new();
    let first = &files[0];
    let out = &profile.output;
    if !out.after_save_command.trim().is_empty() {
        for f in &files {
            if let Err(e) = crate::system::run_hook(&out.after_save_command, f) {
                warnings.push(format!("{e:#}"));
            }
        }
    }
    if out.open_after_save
        && let Err(e) = crate::system::open(first)
    {
        warnings.push(format!("{e:#}"));
    }
    if out.reveal_after_save
        && let Err(e) = crate::system::reveal(first)
    {
        warnings.push(format!("{e:#}"));
    }
    Ok(Saved { files, warnings })
}
