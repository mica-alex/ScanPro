//! The GUI controller: owns the app state on the UI thread and moves all slow
//! work (device I/O, image processing, OCR, export) onto worker threads.
//!
//! Workers never touch the UI directly. They call [`post`], which runs a
//! closure on the event loop with the window and the [`State`]. Anything a
//! worker reports about a page is matched back by the page's `key`, never by
//! index, because pages can be reordered or deleted while work is in flight.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use image::RgbaImage;
use slint::{ComponentHandle, Model, ModelRc, SharedPixelBuffer, SharedString, VecModel};

use scanpro::config::LlmInput;
use scanpro::config::{Config, LlmApi, OutputFormat, Profile, SaveMode};
use scanpro::job;
use scanpro::llm;
use scanpro::naming::Suggestion;
use scanpro::ocr::{self, EngineKind, OcrPage};
use scanpro::pipeline::{self, Page, PageStore};
use scanpro::scanner::{self, CancelToken, Capabilities, ColorMode, Device, DeviceInfo, PaperSize, Source};

slint::include_modules!();

const THUMB_SIZE: u32 = 280;
const PREVIEW_SIZE: u32 = 1800;

/// One slot in the page list. `page` is `None` while the scan is still
/// being stored and analysed.
struct Entry {
    key: u64,
    page: Option<Page>,
    thumb: Option<RgbaImage>,
    ocr_state: i32,
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Append,
    Replace,
    InsertAfter,
}

/// Where the pages of the running scan go.
struct ScanJob {
    cancel: CancelToken,
    /// Index the first page is inserted at.
    base: usize,
    /// Key of the job's most recent page; the next goes right after it.
    last_key: Option<u64>,
    /// Rescan: the page being replaced, removed when the first new page lands
    /// so a failed rescan leaves the original in place.
    replace_key: Option<u64>,
    pages: usize,
    skipped: usize,
    save_when_done: bool,
}

struct State {
    config: Config,
    profile: Profile,
    devices: Vec<DeviceInfo>,
    caps: Option<Capabilities>,
    sources: Vec<Source>,
    colors: Vec<ColorMode>,
    dpis: Vec<u32>,
    engines: Vec<EngineKind>,
    entries: Vec<Entry>,
    model: Rc<VecModel<PageItem>>,
    store: Arc<PageStore>,
    next_key: u64,
    job: Option<ScanJob>,
    /// Pages scanned by the last job are still processing / a save is
    /// queued for when they finish.
    autosave_pending: bool,
    saving: bool,
    name_manual: bool,
    suggestion: Option<Suggestion>,
    suggest_generation: u64,
    preview_generation: u64,
    ocr_total: usize,
    ocr_done: usize,
    /// The launch-time OCR warm-up is still running; queued pages wait on it.
    ocr_warming: bool,
    /// Models the naming server reported; the dropdown indexes into this.
    llm_models: Vec<llm::ModelInfo>,
    llm_list_generation: u64,
    message: String,
    last_saved: Option<PathBuf>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static UI: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
}

/// Serialises device access between worker threads (capability query vs.
/// scan). The device itself is opened per operation and closed right after:
/// some backends (epsonds — the ES-300W) lock the scanner while it's open,
/// and a lock left behind by a process that dies or quits without closing
/// survives until the scanner is power-cycled.
static DEVICE_LOCK: Mutex<()> = Mutex::new(());

fn ocr_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        // OCR engines are internally parallel; half the cores keeps the UI
        // and the scan pipeline responsive.
        let n = std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(1).max(1);
        rayon::ThreadPoolBuilder::new().num_threads(n).thread_name(|i| format!("ocr-{i}")).build().unwrap()
    })
}

fn with_state<R>(f: impl FnOnce(&AppWindow, &mut State) -> R) -> Option<R> {
    let ui = UI.with(|u| u.borrow().as_ref().and_then(|w| w.upgrade()))?;
    STATE.with(|s| s.borrow_mut().as_mut().map(|state| f(&ui, state)))
}

/// Run `f` on the UI thread with the window and state.
fn post(f: impl FnOnce(&AppWindow, &mut State) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        with_state(f);
    });
}

fn strings<I: IntoIterator<Item = S>, S: Into<SharedString>>(items: I) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(items.into_iter().map(Into::into).collect::<Vec<SharedString>>()))
}

/// Turn a thumbnail clockwise by `degrees` (any multiple of 90, negative
/// for counter-clockwise), matching what [`Page::rotate_by`] did to the page.
fn turn_thumb(t: RgbaImage, degrees: i32) -> RgbaImage {
    match degrees.rem_euclid(360) {
        90 => image::imageops::rotate90(&t),
        180 => image::imageops::rotate180(&t),
        270 => image::imageops::rotate270(&t),
        _ => t,
    }
}

fn to_image(rgba: &RgbaImage) -> slint::Image {
    slint::Image::from_rgba8(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()))
}

fn device_with<R>(id: &str, f: impl FnOnce(&mut dyn Device) -> Result<R>) -> Result<R> {
    let _guard = DEVICE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut device = scanner::open(id)?;
    f(device.as_mut())
    // `device` drops (closes) here, before the lock is released.
}

pub fn run(files: Vec<PathBuf>) -> Result<()> {
    let ui = AppWindow::new()?;
    let config = Config::load();
    let profile = config.active();
    let state = State {
        engines: EngineKind::available(),
        config,
        profile,
        devices: Vec::new(),
        caps: None,
        sources: Source::ALL.to_vec(),
        colors: ColorMode::ALL.to_vec(),
        dpis: scanner::STANDARD_DPI.to_vec(),
        entries: Vec::new(),
        model: Rc::new(VecModel::default()),
        store: Arc::new(PageStore::new()?),
        next_key: 1,
        job: None,
        autosave_pending: false,
        saving: false,
        name_manual: false,
        suggestion: None,
        suggest_generation: 0,
        preview_generation: 0,
        ocr_total: 0,
        ocr_done: 0,
        ocr_warming: false,
        llm_models: Vec::new(),
        llm_list_generation: 0,
        message: "Ready".into(),
        last_saved: None,
    };
    ui.set_pages(ModelRc::from(state.model.clone()));
    ui.set_paper_names(strings(PaperSize::ALL.map(|p| p.label())));
    ui.set_format_names(strings(OutputFormat::ALL.map(|f| f.label())));
    ui.set_engine_names(strings(state.engines.iter().map(|e| e.label())));
    ui.set_llm_api_names(strings(LlmApi::ALL.map(|a| a.label())));
    ui.set_llm_input_names(strings(LlmInput::ALL.map(|i| i.label())));
    UI.with(|u| *u.borrow_mut() = Some(ui.as_weak()));
    STATE.with(|s| *s.borrow_mut() = Some(state));
    with_state(|ui, s| {
        refresh_profiles(ui, s);
        apply_profile(ui, s);
        reset_name(ui, s);
        update_status(ui, s);
    });

    wire(&ui);
    refresh_devices();
    warm_up_ocr();
    with_state(refresh_llm_models);
    if !files.is_empty() {
        with_state(|ui, s| import(ui, s, files));
    }
    ui.run()?;

    // Window closed: stop any scan and wait for the worker to close the
    // device, so the scanner isn't left locked (see `DEVICE_LOCK`).
    let scanning = with_state(|_, s| {
        if let Some(job) = &s.job {
            job.cancel.cancel();
        }
        s.job.is_some()
    });
    if scanning == Some(true) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if DEVICE_LOCK.try_lock().is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    Ok(())
}

fn wire(ui: &AppWindow) {
    ui.on_refresh_devices(refresh_devices);
    ui.on_device_selected(|i| {
        with_state(|ui, s| {
            if let Some(d) = s.devices.get(i as usize) {
                s.config.last_device = Some(d.id.clone());
                let _ = s.config.save();
                load_capabilities(ui, d.id.clone());
            }
        });
    });
    ui.on_profile_selected(|i| {
        with_state(|ui, s| {
            if let Some(p) = s.config.profiles.get(i as usize).cloned() {
                s.config.active_profile = p.name.clone();
                s.profile = p;
                let _ = s.config.save();
                apply_profile(ui, s);
                if !s.name_manual {
                    reset_name(ui, s);
                }
                s.llm_models.clear();
                ui.set_llm_model_names(strings(Vec::<String>::new()));
                refresh_llm_models(ui, s);
            }
        });
    });
    ui.on_save_profile_as(|name| {
        with_state(|ui, s| {
            let mut p = read_profile(ui, s);
            p.name = name.trim().to_string();
            if p.name.is_empty() {
                return;
            }
            s.config.active_profile = p.name.clone();
            s.config.upsert(p.clone());
            s.profile = p;
            let _ = s.config.save();
            refresh_profiles(ui, s);
            s.message = format!("Saved profile “{}”", s.profile.name);
            update_status(ui, s);
        });
    });
    ui.on_delete_profile(|| {
        with_state(|ui, s| {
            if s.config.profiles.len() <= 1 {
                return;
            }
            let name = s.profile.name.clone();
            s.config.profiles.retain(|p| p.name != name);
            s.profile = s.config.profiles[0].clone();
            s.config.active_profile = s.profile.name.clone();
            let _ = s.config.save();
            refresh_profiles(ui, s);
            apply_profile(ui, s);
        });
    });
    ui.on_settings_changed(|| with_state(settings_changed).unwrap_or(()));
    ui.on_llm_api_changed(|| {
        with_state(|ui, s| {
            // Follow the server type with its usual address, unless the user
            // typed their own.
            let typed = ui.get_llm_endpoint().trim().to_string();
            let is_a_default = typed.is_empty() || LlmApi::ALL.iter().any(|a| llm::default_endpoint(*a) == typed);
            if is_a_default {
                let api = LlmApi::ALL[(ui.get_llm_api_index().max(0) as usize).min(LlmApi::ALL.len() - 1)];
                ui.set_llm_endpoint(llm::default_endpoint(api).into());
            }
            s.llm_models.clear();
            settings_changed(ui, s);
            refresh_llm_models(ui, s);
        });
    });
    ui.on_refresh_llm_models(|| {
        with_state(|ui, s| {
            settings_changed(ui, s);
            refresh_llm_models(ui, s);
        });
    });
    ui.on_llm_model_selected(|i| {
        with_state(|ui, s| {
            if let Some(m) = s.llm_models.get(i as usize) {
                ui.set_llm_model(m.id.clone().into());
            }
            settings_changed(ui, s);
        });
    });
    ui.on_load_llm_model(|| {
        with_state(|ui, s| {
            let n = read_profile(ui, s).naming;
            ui.set_llm_busy(true);
            s.message = format!("Loading {}…", n.llm_model);
            update_status(ui, s);
            std::thread::spawn(move || {
                let result = llm::Client::new(n.llm_api, &n.llm_endpoint).ensure_loaded(&n.llm_model);
                post(move |ui, s| {
                    ui.set_llm_busy(false);
                    s.message = match result {
                        Ok(secs) if secs > 0.0 => format!("Loaded {} in {secs:.1} s", n.llm_model),
                        Ok(_) => format!("{} is loaded", n.llm_model),
                        Err(e) => format!("Couldn't load the model: {e:#}"),
                    };
                    update_status(ui, s);
                    refresh_llm_models(ui, s);
                });
            });
        });
    });
    ui.on_scan(|target| {
        let target = match target {
            1 => Target::Replace,
            2 => Target::InsertAfter,
            _ => Target::Append,
        };
        with_state(|ui, s| start_scan(ui, s, target));
    });
    ui.on_stop_scan(|| {
        with_state(|ui, s| {
            if let Some(job) = &s.job {
                job.cancel.cancel();
                s.message = "Stopping…".into();
                update_status(ui, s);
            }
        });
    });
    ui.on_import_files(|| {
        let files = rfd::FileDialog::new()
            .set_title("Import images")
            .add_filter("Images", &["jpg", "jpeg", "png", "tif", "tiff", "pnm", "pgm", "ppm"])
            .pick_files();
        if let Some(files) = files {
            with_state(|ui, s| import(ui, s, files));
        }
    });
    ui.on_select_page(|i| with_state(|ui, s| select(ui, s, i)).unwrap_or(()));
    ui.on_rotate_page(|i, deg| {
        with_state(|ui, s| {
            let Some(e) = s.entries.get_mut(i as usize) else { return };
            let Some(page) = &mut e.page else { return };
            page.rotate_by(deg);
            page.rotation_locked = true;
            e.thumb = e.thumb.take().map(|t| turn_thumb(t, deg));
            e.ocr_state = 0;
            refresh_row(s, i as usize);
            queue_missing_ocr(ui, s);
            select(ui, s, i);
        });
    });
    ui.on_move_page(|i, delta| {
        with_state(|ui, s| {
            let from = i as usize;
            let to = i as i64 + delta as i64;
            if to < 0 || to as usize >= s.entries.len() || from >= s.entries.len() {
                return;
            }
            s.entries.swap(from, to as usize);
            refresh_row(s, from);
            refresh_row(s, to as usize);
            ui.set_selected(to as i32);
        });
    });
    ui.on_move_page_to(|from, to| {
        with_state(|ui, s| {
            let (from, to) = (from as usize, to as usize);
            if from >= s.entries.len() || to >= s.entries.len() || from == to {
                return;
            }
            let entry = s.entries.remove(from);
            s.entries.insert(to, entry);
            // Rewrite the affected rows in place rather than remove + insert,
            // so the grid keeps its card elements and doesn't flicker.
            for i in from.min(to)..=from.max(to) {
                refresh_row(s, i);
            }
            select(ui, s, to as i32);
        });
    });
    ui.on_delete_page(|i| {
        with_state(|ui, s| {
            let i = i as usize;
            if i >= s.entries.len() {
                return;
            }
            remove_entry(s, i);
            let sel = if s.entries.is_empty() { -1 } else { i.min(s.entries.len() - 1) as i32 };
            select(ui, s, sel);
            update_status(ui, s);
        });
    });
    ui.on_clear_pages(|| with_state(clear_pages).unwrap_or(()));
    ui.on_name_edited(|| {
        with_state(|_, s| s.name_manual = true);
    });
    ui.on_suggest_name(|| {
        with_state(|ui, s| {
            s.name_manual = false;
            request_suggestion(ui, s, true);
        });
    });
    ui.on_choose_folder(|| {
        let current = with_state(|_, s| s.profile.output.resolved_folder()).unwrap_or_default();
        let _ = std::fs::create_dir_all(&current);
        if let Some(dir) = rfd::FileDialog::new().set_title("Save scans to").set_directory(&current).pick_folder() {
            with_state(|ui, s| {
                s.profile.output.folder = dir;
                s.config.upsert(s.profile.clone());
                let _ = s.config.save();
                ui.set_folder(s.profile.output.resolved_folder().display().to_string().into());
            });
        }
    });
    ui.on_open_folder(|| {
        with_state(|ui, s| {
            let dir = s.profile.output.resolved_folder();
            let _ = std::fs::create_dir_all(&dir);
            if let Err(e) = scanpro::system::open(&dir) {
                s.message = format!("{e:#}");
                update_status(ui, s);
            }
        });
    });
    ui.on_reveal_last(|| {
        with_state(|_, s| {
            if let Some(p) = &s.last_saved {
                let _ = scanpro::system::reveal(p);
            }
        });
    });
    ui.on_save(|| with_state(|ui, s| save(ui, s, false)).unwrap_or(()));
    ui.on_test_llm(|| {
        with_state(|ui, s| {
            let settings = read_profile(ui, s).naming;
            ui.set_llm_busy(true);
            s.message = format!("Asking {}…", settings.llm_model);
            update_status(ui, s);
            std::thread::spawn(move || {
                // An invented sample; real documents never go in here.
                let sample = OcrPage {
                    width: 1000,
                    height: 1000,
                    lines: [
                        "ACME WIDGETS INC.",
                        "Invoice number 4411",
                        "Invoice date: March 3, 2026",
                        "Total due $120.00",
                    ]
                    .iter()
                    .map(|t| ocr::OcrLine {
                        words: t
                            .split(' ')
                            .map(|w| ocr::OcrWord { text: w.into(), bbox: Default::default() })
                            .collect(),
                        bbox: Default::default(),
                    })
                    .collect(),
                };
                let msg = match scanpro::naming::suggest_llm(&[&sample], &settings, None, None) {
                    Ok((s, stats)) => {
                        let load = if stats.load_secs > 0.0 {
                            format!(" (after loading it in {:.1} s)", stats.load_secs)
                        } else {
                            String::new()
                        };
                        format!(
                            "It works: {} named a sample invoice “{}” in {:.1} s{load}",
                            settings.llm_model, s.name, stats.answer_secs
                        )
                    }
                    Err(e) => format!("The model didn't answer: {e:#}"),
                };
                post(move |ui, s| {
                    ui.set_llm_busy(false);
                    s.message = msg;
                    update_status(ui, s);
                    refresh_llm_models(ui, s);
                });
            });
        });
    });
}

/// Load the OCR engine and run it once on a small bundled image of text, in
/// the background at launch. A cold start sometimes takes 10–25 s (macOS
/// reloads Vision's models — when exactly isn't predictable; ocrs may
/// download its models), and without this the first scanned pages pay for
/// it while the user watches.
fn warm_up_ocr() {
    let Some(settings) = with_state(|_, s| s.profile.ocr.clone()) else { return };
    if !settings.enabled {
        return;
    }
    with_state(|_, s| s.ocr_warming = true);
    ocr_pool().spawn(move || {
        let done = || {
            post(|ui, s| {
                s.ocr_warming = false;
                update_status(ui, s);
            })
        };
        // Real text, not a synthetic pattern: Vision only loads its
        // recognition models once it finds something worth reading.
        let Ok(page) = image::load_from_memory(include_bytes!("../assets/ocr-warmup.png")) else { return done() };
        let started = std::time::Instant::now();
        match ocr::shared(settings.engine) {
            Ok(engine) => match engine.recognize(&page, &settings.languages) {
                Ok(r) => log::info!(
                    "OCR warmed up in {:.1}s ({} words)",
                    started.elapsed().as_secs_f32(),
                    r.lines.iter().map(|l| l.words.len()).sum::<usize>()
                ),
                Err(e) => log::warn!("OCR warm-up failed: {e:#}"),
            },
            Err(e) => log::warn!("OCR engine unavailable: {e:#}"),
        }
        done();
    });
}

/// A setting changed in the UI: persist it into the active profile and
/// react to what changed.
fn settings_changed(ui: &AppWindow, s: &mut State) {
    let before = s.profile.clone();
    s.profile = read_profile(ui, s);
    s.config.upsert(s.profile.clone());
    if let Err(e) = s.config.save() {
        s.message = format!("Couldn't save settings: {e:#}");
        update_status(ui, s);
    }
    if before.output.name_template != s.profile.output.name_template && !s.name_manual {
        reset_name(ui, s);
    }
    // Turning OCR on (or switching engine) should cover pages already scanned.
    if s.profile.ocr != before.ocr {
        for e in &mut s.entries {
            if let Some(p) = &mut e.page
                && (s.profile.ocr.engine != before.ocr.engine || s.profile.ocr.languages != before.ocr.languages)
            {
                p.ocr = None;
            }
        }
        queue_missing_ocr(ui, s);
    }
    ui.set_llm_can_load(s.profile.naming.llm_api != LlmApi::OpenAi);
    // Switching the AI on: fetch the model list so the dropdown is ready.
    if s.profile.naming.use_llm && !before.naming.use_llm {
        refresh_llm_models(ui, s);
    }
}

/// Ask the naming server for its models and fill the dropdown. The current
/// model stays selectable even if the server doesn't list it (e.g. it's
/// down right now), so a refresh never silently changes the setting.
fn refresh_llm_models(ui: &AppWindow, s: &mut State) {
    let naming = s.profile.naming.clone();
    if !naming.use_llm {
        return;
    }
    s.llm_list_generation += 1;
    let generation = s.llm_list_generation;
    ui.set_llm_listing(true);
    std::thread::spawn(move || {
        let client = llm::Client::new(naming.llm_api, &naming.llm_endpoint);
        let result = client.list_models();
        let lmstudio_hint = naming.llm_api == LlmApi::OpenAi && result.is_ok() && client.looks_like_lmstudio();
        post(move |ui, s| {
            if generation != s.llm_list_generation {
                return;
            }
            ui.set_llm_listing(false);
            let current = s.profile.naming.llm_model.clone();
            let (mut models, status) = match result {
                Ok(models) => {
                    let loaded = models.iter().filter(|m| m.loaded == Some(true)).count();
                    let status = match (models.len(), naming.llm_api) {
                        (0, _) => "The server lists no models.".to_string(),
                        (n, LlmApi::OpenAi) => format!("{n} model{}", if n == 1 { "" } else { "s" }),
                        (n, _) => format!("{n} model{}, {loaded} loaded (●)", if n == 1 { "" } else { "s" }),
                    };
                    (models, status)
                }
                Err(e) => (Vec::new(), format!("Can't list models: {e:#}")),
            };
            // A short name the server also accepts ("tiny-4b" for "acme/tiny-4b") becomes
            // the full id, so the dropdown shows it and loading works.
            let current = match llm::find_model(&models, &current) {
                Some(m) if m.id != current => {
                    ui.set_llm_model(m.id.clone().into());
                    let id = m.id.clone();
                    s.profile.naming.llm_model = id.clone();
                    s.config.upsert(s.profile.clone());
                    let _ = s.config.save();
                    id
                }
                _ => current,
            };
            if !current.is_empty() && !models.is_empty() && llm::find_model(&models, &current).is_none() {
                models.insert(
                    0,
                    llm::ModelInfo {
                        id: current.clone(),
                        name: Some(format!("{current} (not on this server)")),
                        loaded: None,
                        vision: None,
                        size_bytes: None,
                        reasoning: false,
                    },
                );
            }
            ui.set_llm_model_names(strings(models.iter().map(|m| m.label())));
            ui.set_llm_model_index(models.iter().position(|m| m.id == current).unwrap_or(0) as i32);
            let status = if lmstudio_hint {
                format!(
                    "{status}. This is LM Studio: choose it as the server type to see which models are loaded and load them."
                )
            } else {
                status
            };
            ui.set_llm_models_status(status.into());
            s.llm_models = models;
            // Nothing chosen yet: take the first (loaded models sort first).
            if current.is_empty()
                && let Some(first) = s.llm_models.first()
            {
                ui.set_llm_model(first.id.clone().into());
                settings_changed(ui, s);
            }
        });
    });
}

// ─── Devices ───────────────────────────────────────────────────────────────

fn refresh_devices() {
    with_state(|ui, _| ui.set_discovering(true));
    std::thread::spawn(|| {
        let devices = scanner::discover_all();
        post(move |ui, s| {
            ui.set_discovering(false);
            s.devices = devices;
            ui.set_device_names(strings(s.devices.iter().map(|d| format!("{}  ·  {}", d.name, d.backend))));
            let wanted = s.profile.device.clone().or(s.config.last_device.clone());
            let index = wanted.and_then(|id| s.devices.iter().position(|d| d.id == id)).unwrap_or(0);
            ui.set_device_index(index as i32);
            match s.devices.get(index) {
                Some(d) => load_capabilities(ui, d.id.clone()),
                None => {
                    s.message = "No scanners found. Check the cable or network, then press Refresh.".into();
                    update_status(ui, s);
                }
            }
        });
    });
}

fn load_capabilities(_ui: &AppWindow, id: String) {
    std::thread::spawn(move || {
        let result = device_with(&id, |d| d.capabilities());
        post(move |ui, s| match result {
            Ok(caps) => {
                s.caps = Some(caps);
                apply_profile(ui, s);
                s.message = "Ready".into();
                update_status(ui, s);
            }
            Err(e) => {
                s.message = format!("Couldn't open the scanner: {e:#}");
                update_status(ui, s);
            }
        });
    });
}

// ─── Profile ⇄ UI ──────────────────────────────────────────────────────────

fn refresh_profiles(ui: &AppWindow, s: &State) {
    ui.set_profile_names(strings(s.config.profiles.iter().map(|p| p.name.clone())));
    let idx = s.config.profiles.iter().position(|p| p.name == s.profile.name).unwrap_or(0);
    ui.set_profile_index(idx as i32);
}

/// Push the active profile into the widgets, constrained to what the
/// current device supports. The profile itself is left unchanged, so
/// switching to a more capable scanner restores e.g. duplex.
fn apply_profile(ui: &AppWindow, s: &mut State) {
    let p = &s.profile;
    if let Some(caps) = &s.caps {
        s.sources = if caps.sources.is_empty() { Source::ALL.to_vec() } else { caps.sources.clone() };
        s.colors = if caps.color_modes.is_empty() { ColorMode::ALL.to_vec() } else { caps.color_modes.clone() };
        s.dpis = if caps.resolutions.is_empty() { scanner::STANDARD_DPI.to_vec() } else { caps.resolutions.clone() };
    }
    ui.set_source_names(strings(s.sources.iter().map(|x| x.label())));
    ui.set_source_index(s.sources.iter().position(|&x| x == p.scan.source).unwrap_or(0) as i32);
    ui.set_color_names(strings(s.colors.iter().map(|x| x.label())));
    ui.set_color_index(s.colors.iter().position(|&x| x == p.scan.color).unwrap_or(0) as i32);
    ui.set_dpi_names(strings(s.dpis.iter().map(|d| format!("{d} dpi"))));
    let dpi_idx = s
        .dpis
        .iter()
        .enumerate()
        .min_by_key(|(_, d)| (**d as i64 - p.scan.dpi as i64).abs())
        .map(|(i, _)| i)
        .unwrap_or(0);
    ui.set_dpi_index(dpi_idx as i32);
    ui.set_paper_index(PaperSize::ALL.iter().position(|&x| x == p.scan.paper).unwrap_or(0) as i32);
    ui.set_skip_blank(p.processing.skip_blank);
    ui.set_blank_threshold(p.processing.blank_threshold_percent);
    ui.set_jpeg_quality(p.processing.jpeg_quality as f32);
    ui.set_ocr_enabled(p.ocr.enabled);
    ui.set_engine_index(s.engines.iter().position(|&e| e == p.ocr.engine).unwrap_or(0) as i32);
    ui.set_ocr_languages(p.ocr.languages.join(", ").into());
    ui.set_fix_orientation(p.ocr.fix_orientation);
    ui.set_format_index(OutputFormat::ALL.iter().position(|&f| f == p.output.format).unwrap_or(0) as i32);
    ui.set_folder(p.output.resolved_folder().display().to_string().into());
    ui.set_name_template(p.output.name_template.clone().into());
    ui.set_auto_save(p.output.save_mode == SaveMode::Auto);
    ui.set_save_after_scan(p.output.save_after_scan);
    ui.set_clear_after_save(p.output.clear_after_save);
    ui.set_open_after_save(p.output.open_after_save);
    ui.set_reveal_after_save(p.output.reveal_after_save);
    ui.set_after_save_command(p.output.after_save_command.clone().into());
    ui.set_suggest_names(p.naming.suggest);
    ui.set_use_llm(p.naming.use_llm);
    ui.set_llm_api_index(LlmApi::ALL.iter().position(|&a| a == p.naming.llm_api).unwrap_or(0) as i32);
    ui.set_llm_input_index(LlmInput::ALL.iter().position(|&i| i == p.naming.llm_input).unwrap_or(0) as i32);
    ui.set_llm_auto_load(p.naming.llm_auto_load);
    ui.set_llm_can_load(p.naming.llm_api != LlmApi::OpenAi);
    ui.set_llm_endpoint(p.naming.llm_endpoint.clone().into());
    ui.set_llm_model(p.naming.llm_model.clone().into());
}

fn read_profile(ui: &AppWindow, s: &State) -> Profile {
    let mut p = s.profile.clone();
    let pick = |i: i32, len: usize| (i.max(0) as usize).min(len.saturating_sub(1));
    if let Some(&x) = s.sources.get(pick(ui.get_source_index(), s.sources.len())) {
        p.scan.source = x;
    }
    if let Some(&x) = s.colors.get(pick(ui.get_color_index(), s.colors.len())) {
        p.scan.color = x;
    }
    if let Some(&x) = s.dpis.get(pick(ui.get_dpi_index(), s.dpis.len())) {
        p.scan.dpi = x;
    }
    p.scan.paper = PaperSize::ALL[pick(ui.get_paper_index(), PaperSize::ALL.len())];
    p.processing.skip_blank = ui.get_skip_blank();
    p.processing.blank_threshold_percent = ui.get_blank_threshold();
    p.processing.jpeg_quality = ui.get_jpeg_quality().round().clamp(1.0, 100.0) as u8;
    p.ocr.enabled = ui.get_ocr_enabled();
    p.ocr.engine = s.engines[pick(ui.get_engine_index(), s.engines.len())];
    p.ocr.fix_orientation = ui.get_fix_orientation();
    p.ocr.languages =
        ui.get_ocr_languages().split([',', ' ']).map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    p.output.format = OutputFormat::ALL[pick(ui.get_format_index(), OutputFormat::ALL.len())];
    p.output.name_template = ui.get_name_template().to_string();
    p.output.save_mode = if ui.get_auto_save() { SaveMode::Auto } else { SaveMode::Ask };
    p.output.save_after_scan = ui.get_save_after_scan();
    p.output.clear_after_save = ui.get_clear_after_save();
    p.output.open_after_save = ui.get_open_after_save();
    p.output.reveal_after_save = ui.get_reveal_after_save();
    p.output.after_save_command = ui.get_after_save_command().to_string();
    p.naming.suggest = ui.get_suggest_names();
    p.naming.use_llm = ui.get_use_llm();
    p.naming.llm_api = LlmApi::ALL[pick(ui.get_llm_api_index(), LlmApi::ALL.len())];
    p.naming.llm_input = LlmInput::ALL[pick(ui.get_llm_input_index(), LlmInput::ALL.len())];
    p.naming.llm_auto_load = ui.get_llm_auto_load();
    p.naming.llm_endpoint = ui.get_llm_endpoint().trim().to_string();
    p.naming.llm_model = ui.get_llm_model().trim().to_string();
    p
}

// ─── Page list ─────────────────────────────────────────────────────────────

fn item(e: &Entry, number: usize) -> PageItem {
    let (aspect, blank) = match &e.page {
        Some(p) => {
            let (w, h) = p.oriented_size();
            (w as f32 / h.max(1) as f32, p.blank)
        }
        None => (8.5 / 11.0, false),
    };
    PageItem {
        key: e.key as i32,
        number: number as i32,
        thumb: e.thumb.as_ref().map(to_image).unwrap_or_default(),
        ready: e.page.is_some(),
        blank,
        ocr: e.ocr_state,
        aspect,
    }
}

fn refresh_row(s: &State, i: usize) {
    if let Some(e) = s.entries.get(i) {
        s.model.set_row_data(i, item(e, i + 1));
    }
}

/// Fix page numbers from `from` on after an insert/remove.
fn renumber(s: &State, from: usize) {
    for i in from..s.model.row_count() {
        if let Some(mut row) = s.model.row_data(i)
            && row.number != i as i32 + 1
        {
            row.number = i as i32 + 1;
            s.model.set_row_data(i, row);
        }
    }
}

fn insert_entry(s: &mut State, index: usize, entry: Entry) {
    let index = index.min(s.entries.len());
    s.model.insert(index, item(&entry, index + 1));
    s.entries.insert(index, entry);
    renumber(s, index + 1);
}

fn remove_entry(s: &mut State, index: usize) {
    let e = s.entries.remove(index);
    s.model.remove(index);
    if let Some(p) = &e.page {
        s.store.remove(p);
    }
    renumber(s, index);
}

fn position(s: &State, key: u64) -> Option<usize> {
    s.entries.iter().position(|e| e.key == key)
}

fn clear_pages(ui: &AppWindow, s: &mut State) {
    if s.job.is_some() {
        return;
    }
    for e in s.entries.drain(..) {
        if let Some(p) = &e.page {
            s.store.remove(p);
        }
    }
    s.model.set_vec(Vec::new());
    s.ocr_total = 0;
    s.ocr_done = 0;
    s.name_manual = false;
    s.suggestion = None;
    ui.set_selected(-1);
    reset_name(ui, s);
    update_status(ui, s);
}

fn select(ui: &AppWindow, s: &mut State, i: i32) {
    ui.set_selected(i);
    s.preview_generation += 1;
    let generation = s.preview_generation;
    let Some(page) = s.entries.get(i.max(0) as usize).and_then(|e| e.page.clone()).filter(|_| i >= 0) else {
        ui.set_preview(Default::default());
        return;
    };
    ui.set_preview_loading(true);
    rayon::spawn(move || {
        let preview = page.load().map(|img| {
            let img = if img.width().max(img.height()) > PREVIEW_SIZE {
                img.thumbnail(PREVIEW_SIZE, PREVIEW_SIZE)
            } else {
                img
            };
            img.to_rgba8()
        });
        post(move |ui, s| {
            if s.preview_generation != generation {
                return;
            }
            ui.set_preview_loading(false);
            match preview {
                Ok(p) => ui.set_preview(to_image(&p)),
                Err(e) => {
                    s.message = format!("Couldn't show the page: {e:#}");
                    update_status(ui, s);
                }
            }
        });
    });
}

// ─── Scanning & import ─────────────────────────────────────────────────────

fn start_scan(ui: &AppWindow, s: &mut State, target: Target) {
    if s.job.is_some() {
        return;
    }
    let Some(device) = s.devices.get(ui.get_device_index().max(0) as usize).cloned() else {
        s.message = "Select a scanner first".into();
        update_status(ui, s);
        return;
    };
    s.profile = read_profile(ui, s);
    let selected = ui.get_selected();
    let has_sel = selected >= 0 && (selected as usize) < s.entries.len();
    let (base, replace_key) = match target {
        Target::Replace if has_sel => (selected as usize, Some(s.entries[selected as usize].key)),
        Target::InsertAfter if has_sel => (selected as usize + 1, None),
        _ => (s.entries.len(), None),
    };
    let cancel = CancelToken::default();
    s.job = Some(ScanJob {
        cancel: cancel.clone(),
        base,
        last_key: None,
        replace_key,
        pages: 0,
        skipped: 0,
        save_when_done: target == Target::Append && s.profile.output.save_after_scan,
    });
    ui.set_scanning(true);
    s.message = format!("Scanning with {}…", device.name);
    update_status(ui, s);

    let settings = s.profile.scan.clone();
    let processing = s.profile.processing.clone();
    let trim = settings.paper == PaperSize::Auto && settings.source.is_feeder();
    let store = s.store.clone();
    std::thread::spawn(move || {
        let result = device_with(&device.id, |d| {
            d.scan(&settings, &cancel, &mut |img| {
                // Hand the pixels to the processing pool and go straight back
                // to the scanner, so storing/analysing page N overlaps
                // feeding page N+1.
                let (tx, rx) = std::sync::mpsc::channel::<u64>();
                post(move |ui, s| {
                    let key = page_arrived(ui, s);
                    let _ = tx.send(key);
                });
                let key = rx.recv().unwrap_or(0);
                let store = store.clone();
                let processing = processing.clone();
                rayon::spawn(move || {
                    let img = if trim { pipeline::trim_feeder_length(img) } else { img };
                    process_page(store, processing, key, img)
                });
                Ok(())
            })
        });
        post(move |ui, s| scan_finished(ui, s, result));
    });
}

/// Insert a placeholder for a page that just came off the scanner.
fn page_arrived(ui: &AppWindow, s: &mut State) -> u64 {
    let key = s.next_key;
    s.next_key += 1;
    if s.job.is_none() {
        return key;
    }
    if let Some(rk) = s.job.as_mut().and_then(|j| j.replace_key.take())
        && let Some(i) = position(s, rk)
    {
        remove_entry(s, i);
    }
    let job = s.job.as_mut().unwrap();
    job.pages += 1;
    let index = job.last_key.and_then(|k| s.entries.iter().position(|e| e.key == k)).map(|i| i + 1).unwrap_or(job.base);
    job.last_key = Some(key);
    insert_entry(s, index, Entry { key, page: None, thumb: None, ocr_state: 0 });
    s.message = format!("Scanning… page {}", s.job.as_ref().map(|j| j.pages).unwrap_or(0));
    update_status(ui, s);
    key
}

fn process_page(store: Arc<PageStore>, processing: scanpro::config::Processing, key: u64, img: scanner::ScannedImage) {
    let result = store.ingest(img, &processing).map(|(page, pixels)| {
        let thumb = pipeline::thumbnail(&pixels, page.rotation, THUMB_SIZE);
        (page, thumb)
    });
    post(move |ui, s| page_ready(ui, s, key, result.map_err(|e| format!("{e:#}")), processing.skip_blank));
}

fn page_ready(ui: &AppWindow, s: &mut State, key: u64, result: Result<(Page, RgbaImage), String>, skip_blank: bool) {
    let Some(i) = position(s, key) else {
        // Deleted while processing.
        if let Ok((p, _)) = result {
            s.store.remove(&p);
        }
        return;
    };
    match result {
        Ok((page, _)) if skip_blank && page.blank => {
            s.store.remove(&page);
            s.entries.remove(i);
            s.model.remove(i);
            renumber(s, i);
            if let Some(job) = s.job.as_mut() {
                job.skipped += 1;
                if job.last_key == Some(key) {
                    job.last_key = i.checked_sub(1).and_then(|j| s.entries.get(j)).map(|e| e.key);
                    if job.last_key.is_none() {
                        job.base = i;
                    }
                }
            }
        }
        Ok((page, thumb)) => {
            let e = &mut s.entries[i];
            e.page = Some(page);
            e.thumb = Some(thumb);
            refresh_row(s, i);
            if ui.get_selected() == i as i32 {
                select(ui, s, i as i32);
            }
            queue_missing_ocr(ui, s);
        }
        Err(e) => {
            s.entries.remove(i);
            s.model.remove(i);
            renumber(s, i);
            s.message = format!("A page couldn't be processed: {e}");
        }
    }
    update_status(ui, s);
    maybe_autosave(ui, s);
}

fn scan_finished(ui: &AppWindow, s: &mut State, result: Result<usize>) {
    ui.set_scanning(false);
    let Some(job) = s.job.take() else { return };
    s.message = match &result {
        Ok(_) if job.cancel.is_cancelled() => format!("Stopped after {} page(s)", job.pages),
        Ok(n) => {
            let skipped = if job.skipped > 0 { format!(", {} blank skipped", job.skipped) } else { String::new() };
            format!("Scanned {n} page(s){skipped}")
        }
        Err(e) => format!("{e:#}"),
    };
    if job.save_when_done && result.is_ok() && job.pages > 0 && !job.cancel.is_cancelled() {
        s.autosave_pending = true;
    }
    update_status(ui, s);
    maybe_autosave(ui, s);
    if ui.get_selected() < 0 && !s.entries.is_empty() {
        select(ui, s, 0);
    }
}

fn import(ui: &AppWindow, s: &mut State, files: Vec<PathBuf>) {
    let processing = s.profile.processing.clone();
    let base = s.entries.len();
    for (n, path) in files.into_iter().enumerate() {
        let key = s.next_key;
        s.next_key += 1;
        insert_entry(s, base + n, Entry { key, page: None, thumb: None, ocr_state: 0 });
        let store = s.store.clone();
        let processing = processing.clone();
        rayon::spawn(move || {
            let result = store.import(&path, 300, &processing).map(|(page, pixels)| {
                let thumb = pipeline::thumbnail(&pixels, 0, THUMB_SIZE);
                (page, thumb)
            });
            let result = result.map_err(|e| format!("{}: {e:#}", path.display()));
            // Imported pages are never dropped as blank: the user chose them.
            post(move |ui, s| page_ready(ui, s, key, result, false));
        });
    }
    update_status(ui, s);
}

// ─── OCR & naming ──────────────────────────────────────────────────────────

fn ocr_wanted(p: &Profile) -> bool {
    p.ocr.enabled && (p.output.format.needs_ocr() || p.naming.suggest)
}

/// Queue OCR for every ready page without a result that isn't already queued.
fn queue_missing_ocr(ui: &AppWindow, s: &mut State) {
    if !ocr_wanted(&s.profile) {
        return;
    }
    let settings = s.profile.ocr.clone();
    for e in &mut s.entries {
        let Some(page) = &e.page else { continue };
        if page.ocr.is_some() || e.ocr_state == 1 {
            continue;
        }
        e.ocr_state = 1;
        s.ocr_total += 1;
        let (page, key, settings) = (page.clone(), e.key, settings.clone());
        ocr_pool().spawn(move || {
            let result = ocr::shared(settings.engine)
                .and_then(|engine| job::ocr_page(&page, engine.as_ref(), &settings))
                .map_err(|e| format!("{e:#}"));
            post(move |ui, s| ocr_finished(ui, s, key, page.rotation, result));
        });
    }
    let keys: Vec<usize> = (0..s.entries.len()).filter(|&i| s.entries[i].ocr_state == 1).collect();
    for i in keys {
        refresh_row(s, i);
    }
    update_status(ui, s);
}

fn ocr_finished(ui: &AppWindow, s: &mut State, key: u64, rotation: u16, result: Result<(u16, OcrPage), String>) {
    s.ocr_done += 1;
    if let Some(i) = position(s, key) {
        let e = &mut s.entries[i];
        if let Some(page) = &mut e.page {
            if page.rotation == rotation {
                match result {
                    Ok((new_rotation, r)) => {
                        // OCR found the page upside down: turn the page and
                        // its thumbnail to match the result.
                        if new_rotation != rotation {
                            page.rotate_by(new_rotation as i32 - rotation as i32);
                            e.thumb = e.thumb.take().map(|t| turn_thumb(t, new_rotation as i32 - rotation as i32));
                        }
                        page.ocr = Some(Arc::new(r));
                        e.ocr_state = 2;
                        if new_rotation != rotation && ui.get_selected() == i as i32 {
                            refresh_row(s, i);
                            select(ui, s, i as i32);
                        }
                    }
                    Err(err) => {
                        e.ocr_state = 3;
                        s.message = format!("Text recognition failed: {err}");
                    }
                }
            } else {
                // Rotated while OCR ran; a fresh job is already queued.
                e.ocr_state = if e.ocr_state == 1 { 1 } else { 0 };
            }
            refresh_row(s, i);
        }
    }
    if s.ocr_done >= s.ocr_total {
        s.ocr_total = 0;
        s.ocr_done = 0;
        request_suggestion(ui, s, false);
    }
    update_status(ui, s);
    maybe_autosave(ui, s);
}

fn ready_pages(s: &State) -> Vec<Page> {
    s.entries.iter().filter_map(|e| e.page.clone()).collect()
}

fn current_device_name(ui: &AppWindow, s: &State) -> String {
    s.devices.get(ui.get_device_index().max(0) as usize).map(|d| d.name.clone()).unwrap_or_default()
}

/// The file name the template gives right now, from the current suggestion.
fn reset_name(ui: &AppWindow, s: &mut State) {
    let name = job::file_name(
        &s.profile.output,
        &s.profile.name,
        &current_device_name(ui, s),
        s.suggestion.as_ref(),
        s.config.counter,
        s.entries.len(),
    );
    ui.set_file_name(name.to_string_lossy().replace('\\', "/").into());
}

fn request_suggestion(ui: &AppWindow, s: &mut State, forced: bool) {
    if s.entries.is_empty() || (!forced && (s.name_manual || !s.profile.naming.suggest)) {
        return;
    }
    let mut pages = ready_pages(s);
    if pages.is_empty() {
        return;
    }
    s.suggest_generation += 1;
    let generation = s.suggest_generation;
    let profile = Profile {
        naming: scanpro::config::NamingSettings { suggest: true, ..s.profile.naming.clone() },
        ..s.profile.clone()
    };
    ui.set_suggesting(true);
    update_status(ui, s);
    std::thread::spawn(move || {
        // A forced suggestion (button) may come before OCR was ever run.
        if pages.iter().take(2).any(|p| p.ocr.is_none()) {
            let n = pages.len().min(2);
            let _ = job::ensure_ocr(&mut pages[..n], &profile.ocr);
        }
        let suggestion = job::suggest(&pages, &profile);
        post(move |ui, s| {
            ui.set_suggesting(false);
            update_status(ui, s);
            if generation != s.suggest_generation || (s.name_manual && !forced) {
                return;
            }
            if suggestion.is_none() && forced {
                s.message = "No text to suggest a name from".into();
                update_status(ui, s);
            }
            s.suggestion = suggestion;
            reset_name(ui, s);
        });
    });
}

// ─── Saving ────────────────────────────────────────────────────────────────

fn processing_count(s: &State) -> usize {
    s.entries.iter().filter(|e| e.page.is_none()).count()
}

fn ocr_pending(s: &State) -> usize {
    s.entries.iter().filter(|e| e.ocr_state == 1).count()
}

fn maybe_autosave(ui: &AppWindow, s: &mut State) {
    if s.autosave_pending && s.job.is_none() && processing_count(s) == 0 && ocr_pending(s) == 0 && !ui.get_suggesting()
    {
        s.autosave_pending = false;
        save(ui, s, true);
    }
}

fn save(ui: &AppWindow, s: &mut State, automatic: bool) {
    if s.saving || s.entries.is_empty() {
        return;
    }
    if processing_count(s) > 0 || s.job.is_some() {
        s.message = "Wait for the scan to finish before saving".into();
        update_status(ui, s);
        return;
    }
    s.profile = read_profile(ui, s);
    let profile = s.profile.clone();
    let folder = profile.output.resolved_folder();
    let typed = ui.get_file_name().to_string();
    let name = scanpro::naming::render(
        if typed.trim().is_empty() { "{suggested}" } else { &typed },
        &scanpro::naming::NameContext {
            // The field already holds an expanded name; rendering again only
            // sanitises it (and expands any placeholders the user typed).
            now: jiff::Zoned::now(),
            suggestion: s.suggestion.as_ref(),
            counter: s.config.counter,
            pages: s.entries.len(),
            profile: &profile.name,
            scanner: &current_device_name(ui, s),
        },
    );

    let dest = if automatic || profile.output.save_mode == SaveMode::Auto {
        job::destination(&folder, &name, &profile.output)
    } else {
        let full = folder.join(&name);
        let dir = full.parent().map(PathBuf::from).unwrap_or(folder.clone());
        let _ = std::fs::create_dir_all(&dir);
        let ext = profile.output.format.extension();
        let file = format!("{}.{ext}", full.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
        match rfd::FileDialog::new()
            .set_title("Save scan")
            .set_directory(&dir)
            .set_file_name(&file)
            .add_filter(profile.output.format.label(), &[ext])
            .save_file()
        {
            Some(p) => p,
            None => return,
        }
    };

    let pages = ready_pages(s);
    let keys: Vec<u64> = s.entries.iter().map(|e| e.key).collect();
    let title = s.suggestion.as_ref().map(|x| x.name.clone());
    s.saving = true;
    ui.set_saving(true);
    s.message = format!("Saving {}…", dest.file_name().map(|f| f.to_string_lossy()).unwrap_or_default());
    update_status(ui, s);
    std::thread::spawn(move || {
        let mut pages = pages;
        let result = (|| {
            if profile.ocr.enabled && profile.output.format.needs_ocr() {
                job::ensure_ocr(&mut pages, &profile.ocr)?;
            }
            job::save(&pages, &profile, &dest, title)
        })();
        post(move |ui, s| {
            s.saving = false;
            ui.set_saving(false);
            match result {
                Ok(saved) => {
                    let first = saved.files[0].clone();
                    s.message = format!(
                        "Saved {} ({} page{})",
                        first.display(),
                        pages.len(),
                        if pages.len() == 1 { "" } else { "s" }
                    );
                    if let Some(w) = saved.warnings.first() {
                        s.message.push_str(&format!(" — {w}"));
                    }
                    s.last_saved = Some(first.clone());
                    ui.set_last_saved(first.display().to_string().into());
                    s.config.counter += 1;
                    let _ = s.config.save();
                    // Only clear if nothing changed while saving.
                    let unchanged = s.entries.iter().map(|e| e.key).eq(keys.iter().copied());
                    if s.profile.output.clear_after_save && unchanged {
                        clear_pages(ui, s);
                    } else {
                        reset_name(ui, s);
                    }
                }
                Err(e) => s.message = format!("Save failed: {e:#}"),
            }
            update_status(ui, s);
        });
    });
}

fn update_status(ui: &AppWindow, s: &State) {
    let pending = ocr_pending(s);
    let preparing = processing_count(s);
    let (text, progress) = if s.job.is_some() || s.saving {
        (s.message.clone(), 0.0)
    } else if pending > 0 {
        let total = s.ocr_total.max(pending);
        let done = total - pending;
        (format!("Recognizing text… {done} of {total} pages"), done as f32 / total.max(1) as f32)
    } else {
        (s.message.clone(), -1.0)
    };
    let pages = s.entries.len();
    let text = if pages > 0 && s.job.is_none() && !s.saving {
        format!("{text}   ·   {pages} page{} in document", if pages == 1 { "" } else { "s" })
    } else {
        text
    };
    ui.set_status(text.into());
    ui.set_progress(progress);

    // The banner over the pages: everything still in flight, in pipeline
    // order, so a fast feeder doesn't look finished while work remains.
    let mut activity = Vec::new();
    if let Some(job) = &s.job {
        activity.push(if job.cancel.is_cancelled() {
            "Stopping the scan…".to_string()
        } else if job.pages == 0 {
            "Scanning — waiting for the first page…".to_string()
        } else {
            format!("Scanning — page {}", job.pages)
        });
    }
    if preparing > 0 {
        activity.push(format!("Preparing {preparing} page{}", if preparing == 1 { "" } else { "s" }));
    }
    if pending > 0 && s.ocr_warming {
        activity.push(
            "Getting text recognition ready — this can take up to ~20 s when it hasn't been used in a while"
                .to_string(),
        );
    } else if pending > 0 {
        let total = s.ocr_total.max(pending);
        activity.push(format!("Reading text — {} of {total} done", total - pending));
    }
    if ui.get_suggesting() {
        activity.push("Choosing a name".to_string());
    }
    if s.saving {
        activity.push("Saving…".to_string());
    }
    ui.set_activity(activity.join("   ·   ").into());
    // Determinate only when OCR is the sole thing left to wait for.
    let ocr_only = s.job.is_none() && preparing == 0 && !s.saving && pending > 0;
    ui.set_activity_progress(if ocr_only { progress } else { -1.0 });
}
