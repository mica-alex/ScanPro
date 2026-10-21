//! SANE backend, loaded at runtime with `dlopen` so ScanPro still starts (with
//! this backend unavailable) on machines without libsane.
//!
//! libsane is not thread-safe and `sane_init` is process-global, so every
//! call goes through one [`Sane`] behind a mutex, initialised on first use and
//! kept for the life of the process.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::{Mutex, MutexGuard, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;

use super::{
    Backend, CancelToken, Capabilities, ColorMode, Device, DeviceInfo, PaperSize, STANDARD_DPI, ScanSettings,
    ScannedImage, Source,
};

type Status = c_int;
type Handle = *mut c_void;
type Word = c_int;

const STATUS_GOOD: Status = 0;
const STATUS_CANCELLED: Status = 2;
const STATUS_EOF: Status = 5;
const STATUS_JAMMED: Status = 6;
const STATUS_NO_DOCS: Status = 7;
const STATUS_COVER_OPEN: Status = 8;

const TYPE_BOOL: c_int = 0;
const TYPE_INT: c_int = 1;
const TYPE_FIXED: c_int = 2;

const CONSTRAINT_RANGE: c_int = 1;
const CONSTRAINT_WORD_LIST: c_int = 2;
const CONSTRAINT_STRING_LIST: c_int = 3;

const CAP_SOFT_SELECT: Word = 1;
const CAP_INACTIVE: Word = 32;

const ACTION_GET: c_int = 0;
const ACTION_SET: c_int = 1;

const FRAME_GRAY: c_int = 0;
const FRAME_RGB: c_int = 1;

const UNIT_MM: c_int = 3;

#[repr(C)]
struct RawDevice {
    name: *const c_char,
    vendor: *const c_char,
    model: *const c_char,
    kind: *const c_char,
}

#[repr(C)]
struct RawRange {
    min: Word,
    max: Word,
    quant: Word,
}

#[repr(C)]
struct RawOption {
    name: *const c_char,
    title: *const c_char,
    desc: *const c_char,
    value_type: c_int,
    unit: c_int,
    size: c_int,
    cap: Word,
    constraint_type: c_int,
    constraint: *const c_void,
}

#[repr(C)]
#[derive(Default)]
struct RawParameters {
    format: c_int,
    last_frame: Word,
    bytes_per_line: c_int,
    pixels_per_line: c_int,
    lines: c_int,
    depth: c_int,
}

struct Api {
    _lib: Library,
    init: unsafe extern "C" fn(*mut Word, *const c_void) -> Status,
    get_devices: unsafe extern "C" fn(*mut *const *const RawDevice, Word) -> Status,
    open: unsafe extern "C" fn(*const c_char, *mut Handle) -> Status,
    close: unsafe extern "C" fn(Handle),
    get_option_descriptor: unsafe extern "C" fn(Handle, c_int) -> *const RawOption,
    control_option: unsafe extern "C" fn(Handle, c_int, c_int, *mut c_void, *mut Word) -> Status,
    get_parameters: unsafe extern "C" fn(Handle, *mut RawParameters) -> Status,
    start: unsafe extern "C" fn(Handle) -> Status,
    read: unsafe extern "C" fn(Handle, *mut u8, c_int, *mut c_int) -> Status,
    cancel: unsafe extern "C" fn(Handle),
    strstatus: unsafe extern "C" fn(Status) -> *const c_char,
}

// SAFETY: the function pointers are plain C entry points; all calls are
// serialised through the `SANE` mutex.
unsafe impl Send for Api {}

fn library_candidates() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("SCANPRO_LIBSANE") {
        v.push(p);
    }
    if cfg!(target_os = "macos") {
        for prefix in ["/opt/homebrew", "/usr/local", "/opt/local"] {
            v.push(format!("{prefix}/lib/libsane.1.dylib"));
        }
        v.push("libsane.1.dylib".into());
    } else if cfg!(windows) {
        v.push("libsane-1.dll".into());
    } else {
        v.push("libsane.so.1".into());
        v.push("libsane.so".into());
    }
    v
}

impl Api {
    fn load() -> Result<Api> {
        let mut last_err = None;
        for path in library_candidates() {
            // SAFETY: loading libsane runs its (trivial) initialisers only.
            match unsafe { Library::new(&path) } {
                Ok(lib) => return unsafe { Api::bind(lib) }.with_context(|| format!("binding {path}")),
                Err(e) => last_err = Some(e),
            }
        }
        Err(anyhow!("libsane not found ({})", last_err.map(|e| e.to_string()).unwrap_or_default()))
    }

    unsafe fn bind(lib: Library) -> Result<Api> {
        unsafe {
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes())?
                };
            }
            Ok(Api {
                init: sym!("sane_init"),
                get_devices: sym!("sane_get_devices"),
                open: sym!("sane_open"),
                close: sym!("sane_close"),
                get_option_descriptor: sym!("sane_get_option_descriptor"),
                control_option: sym!("sane_control_option"),
                get_parameters: sym!("sane_get_parameters"),
                start: sym!("sane_start"),
                read: sym!("sane_read"),
                cancel: sym!("sane_cancel"),
                strstatus: sym!("sane_strstatus"),
                _lib: lib,
            })
        }
    }

    fn status_text(&self, status: Status) -> String {
        // SAFETY: sane_strstatus returns a static string.
        unsafe { cstr((self.strstatus)(status)) }
    }

    fn check(&self, status: Status, what: &str) -> Result<()> {
        match status {
            STATUS_GOOD => Ok(()),
            STATUS_JAMMED => bail!("Paper jam — clear the feeder and try again"),
            STATUS_COVER_OPEN => bail!("The scanner cover is open"),
            STATUS_NO_DOCS => bail!("Load paper into the feeder"),
            other => bail!("{what}: {}", self.status_text(other)),
        }
    }
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: SANE strings are NUL-terminated and outlive the call.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

struct Sane {
    api: Api,
}

static SANE: OnceLock<Option<Mutex<Sane>>> = OnceLock::new();

fn sane() -> Result<MutexGuard<'static, Sane>> {
    let cell = SANE.get_or_init(|| match Api::load() {
        Ok(api) => {
            let mut version: Word = 0;
            // SAFETY: valid out-pointer; no auth callback.
            let status = unsafe { (api.init)(&mut version, std::ptr::null()) };
            if status != STATUS_GOOD {
                log::warn!("sane_init failed: {}", api.status_text(status));
                return None;
            }
            log::info!("SANE {}.{}.{}", version >> 24, (version >> 16) & 0xff, version & 0xffff);
            Some(Mutex::new(Sane { api }))
        }
        Err(e) => {
            log::info!("SANE unavailable: {e:#}");
            None
        }
    });
    let mutex = cell.as_ref().ok_or_else(|| anyhow!("SANE is not installed"))?;
    Ok(mutex.lock().unwrap_or_else(|p| p.into_inner()))
}

pub struct SaneBackend;

impl SaneBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SaneBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for SaneBackend {
    fn id(&self) -> &'static str {
        "sane"
    }

    fn available(&self) -> bool {
        sane().is_ok()
    }

    fn discover(&self) -> Result<Vec<DeviceInfo>> {
        let s = sane()?;
        let mut list: *const *const RawDevice = std::ptr::null();
        // SAFETY: valid out-pointer; the list stays valid until the next call,
        // which the mutex guarantees doesn't happen while we read it.
        s.api.check(unsafe { (s.api.get_devices)(&mut list, 0) }, "listing devices")?;
        let mut out = Vec::new();
        let mut i = 0;
        while !list.is_null() {
            // SAFETY: NULL-terminated array of device pointers.
            let dev = unsafe { *list.add(i) };
            if dev.is_null() {
                break;
            }
            let dev = unsafe { &*dev };
            let name = cstr(dev.name);
            // The SANE "net" backend re-exports devices of other hosts, and
            // pixma/escl backends can surface the same network scanner the
            // eSCL backend finds; both are still useful, so keep them.
            let label = format!("{} {}", cstr(dev.vendor), cstr(dev.model)).trim().to_string();
            out.push(DeviceInfo {
                id: format!("sane:{name}"),
                name: if label.is_empty() { name.clone() } else { label },
                backend: "SANE".into(),
            });
            i += 1;
        }
        Ok(out)
    }

    fn open(&self, id: &str) -> Result<Box<dyn Device>> {
        // Look the name up *before* opening: `sane_get_devices` re-probes USB,
        // and some backends (epsonds) hang or wedge the device when probing
        // one that is already open.
        let full_id = format!("sane:{id}");
        let info = self
            .discover()
            .ok()
            .and_then(|found| found.into_iter().find(|d| d.id == full_id))
            .unwrap_or_else(|| DeviceInfo { id: full_id, name: id.to_string(), backend: "SANE".into() });
        let s = sane()?;
        let name = CString::new(id)?;
        let mut handle: Handle = std::ptr::null_mut();
        // SAFETY: valid C string and out-pointer.
        s.api.check(unsafe { (s.api.open)(name.as_ptr(), &mut handle) }, "opening scanner")?;
        Ok(Box::new(SaneDevice { handle, info }))
    }
}

/// A snapshot of one option descriptor, copied out of libsane's memory.
#[derive(Debug, Clone)]
struct OptionDesc {
    index: c_int,
    name: String,
    value_type: c_int,
    unit: c_int,
    size: c_int,
    cap: Word,
    constraint: Constraint,
}

#[derive(Debug, Clone)]
enum Constraint {
    None,
    Range { min: Word, max: Word },
    Words(Vec<Word>),
    Strings(Vec<String>),
}

impl OptionDesc {
    fn settable(&self) -> bool {
        self.cap & CAP_SOFT_SELECT != 0 && self.cap & CAP_INACTIVE == 0
    }
    fn is_fixed(&self) -> bool {
        self.value_type == TYPE_FIXED
    }
    /// Numeric constraint as f64 in the option's real units.
    fn numbers(&self) -> Vec<f64> {
        let conv = |w: Word| if self.is_fixed() { fixed_to_f64(w) } else { w as f64 };
        match &self.constraint {
            Constraint::Words(w) => w.iter().map(|&w| conv(w)).collect(),
            Constraint::Range { min, max } => vec![conv(*min), conv(*max)],
            _ => Vec::new(),
        }
    }
    fn range(&self) -> Option<(f64, f64)> {
        let v = self.numbers();
        Some((v.iter().copied().fold(f64::INFINITY, f64::min), v.iter().copied().fold(f64::NEG_INFINITY, f64::max)))
            .filter(|(a, b)| a.is_finite() && b.is_finite())
    }
}

fn fixed_to_f64(w: Word) -> f64 {
    w as f64 / 65536.0
}

fn f64_to_fixed(v: f64) -> Word {
    (v * 65536.0).round() as Word
}

struct SaneDevice {
    handle: Handle,
    info: DeviceInfo,
}

// SAFETY: the handle is only used under the global SANE mutex.
unsafe impl Send for SaneDevice {}

impl Drop for SaneDevice {
    fn drop(&mut self) {
        if let Ok(s) = sane() {
            // SAFETY: handle came from sane_open and is closed exactly once.
            unsafe { (s.api.close)(self.handle) };
        }
    }
}

impl SaneDevice {
    fn options(&self, s: &Sane) -> Vec<OptionDesc> {
        let mut out = Vec::new();
        // Option 0 is the option count; real options start at 1. Iterate until
        // libsane returns NULL rather than trusting the count.
        for index in 1..512 {
            // SAFETY: returns NULL past the last option.
            let raw = unsafe { (s.api.get_option_descriptor)(self.handle, index) };
            if raw.is_null() {
                break;
            }
            let raw = unsafe { &*raw };
            let constraint = unsafe {
                match raw.constraint_type {
                    CONSTRAINT_RANGE if !raw.constraint.is_null() => {
                        let r = &*(raw.constraint as *const RawRange);
                        let _ = r.quant;
                        Constraint::Range { min: r.min, max: r.max }
                    }
                    CONSTRAINT_WORD_LIST if !raw.constraint.is_null() => {
                        let p = raw.constraint as *const Word;
                        let n = *p as usize;
                        Constraint::Words((1..=n).map(|i| *p.add(i)).collect())
                    }
                    CONSTRAINT_STRING_LIST if !raw.constraint.is_null() => {
                        let mut p = raw.constraint as *const *const c_char;
                        let mut v = Vec::new();
                        while !(*p).is_null() {
                            v.push(cstr(*p));
                            p = p.add(1);
                        }
                        Constraint::Strings(v)
                    }
                    _ => Constraint::None,
                }
            };
            out.push(OptionDesc {
                index,
                name: cstr(raw.name),
                value_type: raw.value_type,
                unit: raw.unit,
                size: raw.size,
                cap: raw.cap,
                constraint,
            });
        }
        out
    }

    fn find<'a>(opts: &'a [OptionDesc], name: &str) -> Option<&'a OptionDesc> {
        opts.iter().find(|o| o.name == name)
    }

    fn set_string(&self, s: &Sane, opt: &OptionDesc, value: &str) -> Result<()> {
        let mut buf = vec![0u8; (opt.size.max(value.len() as c_int + 1)) as usize];
        buf[..value.len()].copy_from_slice(value.as_bytes());
        let mut info: Word = 0;
        // SAFETY: buffer is at least `size` bytes as the API requires.
        let st =
            unsafe { (s.api.control_option)(self.handle, opt.index, ACTION_SET, buf.as_mut_ptr().cast(), &mut info) };
        s.api.check(st, &format!("setting {}", opt.name))
    }

    fn set_number(&self, s: &Sane, opt: &OptionDesc, value: f64) -> Result<()> {
        let mut w: Word = match opt.value_type {
            TYPE_FIXED => f64_to_fixed(value),
            TYPE_INT => value.round() as Word,
            TYPE_BOOL => (value != 0.0) as Word,
            _ => bail!("option {} is not numeric", opt.name),
        };
        let mut info: Word = 0;
        // SAFETY: single-word option value.
        let st = unsafe {
            (s.api.control_option)(self.handle, opt.index, ACTION_SET, (&mut w as *mut Word).cast(), &mut info)
        };
        s.api.check(st, &format!("setting {}", opt.name))
    }

    #[allow(dead_code)]
    fn get_number(&self, s: &Sane, opt: &OptionDesc) -> Result<f64> {
        let mut w: Word = 0;
        let mut _info: Word = 0;
        // SAFETY: single-word option value.
        let st = unsafe {
            (s.api.control_option)(self.handle, opt.index, ACTION_GET, (&mut w as *mut Word).cast(), &mut _info)
        };
        s.api.check(st, &format!("reading {}", opt.name))?;
        Ok(if opt.is_fixed() { fixed_to_f64(w) } else { w as f64 })
    }

    /// Apply `settings`, re-reading descriptors after each change because
    /// setting `source`/`mode` can (de)activate or re-constrain later options.
    fn configure(&self, s: &Sane, settings: &ScanSettings) -> Result<()> {
        let opts = self.options(s);
        if let Some(opt) = Self::find(&opts, "source").filter(|o| o.settable())
            && let Constraint::Strings(list) = &opt.constraint
        {
            let (choice, needs_duplex_flag) = pick_source(list, settings.source)
                .ok_or_else(|| anyhow!("this scanner has no {}", settings.source.label().to_lowercase()))?;
            self.set_string(s, opt, &choice)?;
            if needs_duplex_flag {
                let opts = self.options(s);
                let duplex = Self::find(&opts, "duplex")
                    .filter(|o| o.settable())
                    .ok_or_else(|| anyhow!("this scanner can't scan double-sided"))?;
                self.set_number(s, duplex, 1.0)?;
            }
        }

        let opts = self.options(s);
        if let Some(opt) = Self::find(&opts, "mode").filter(|o| o.settable())
            && let Constraint::Strings(list) = &opt.constraint
            && let Some(choice) = pick_mode(list, settings.color)
        {
            self.set_string(s, opt, &choice)?;
        }

        let opts = self.options(s);
        if let Some(opt) = Self::find(&opts, "resolution").filter(|o| o.settable()) {
            let dpi = nearest_dpi(opt, settings.dpi);
            self.set_number(s, opt, dpi as f64)?;
            // Some backends expose separate x/y resolutions bound together;
            // setting `resolution` covers them. Nothing else to do.
        }

        let opts = self.options(s);
        let auto_crop = Self::find(&opts, "adf-crp").filter(|o| o.settable() && o.value_type == TYPE_BOOL);
        if let Some(opt) = auto_crop {
            self.set_number(s, opt, (settings.paper == PaperSize::Auto) as u8 as f64)?;
        }
        // Hardware deskew is always worth having on a feeder: it fixes the
        // skew at no cost before the page ever reaches us.
        if let Some(opt) = Self::find(&opts, "adf-skew").filter(|o| o.settable() && o.value_type == TYPE_BOOL) {
            self.set_number(s, opt, settings.source.is_feeder() as u8 as f64)?;
        }

        let opts = self.options(s);
        let geometry = ["tl-x", "tl-y", "br-x", "br-y"].map(|n| Self::find(&opts, n).filter(|o| o.settable()).cloned());
        if let [Some(tlx), Some(tly), Some(brx), Some(bry)] = geometry {
            let (_, max_x) = brx.range().unwrap_or((0.0, 0.0));
            let (_, max_y) = bry.range().unwrap_or((0.0, 0.0));
            let (w, h) = match settings.paper.size_mm() {
                Some((w, h)) if brx.unit == UNIT_MM => (w.min(max_x), h.min(max_y)),
                _ => (max_x, max_y),
            };
            self.set_number(s, &tlx, 0.0)?;
            self.set_number(s, &tly, 0.0)?;
            self.set_number(s, &brx, w)?;
            self.set_number(s, &bry, h)?;
        }
        Ok(())
    }

    /// Read one frame after a successful `sane_start`.
    fn read_frame(&self, s: &Sane, cancel: &CancelToken, dpi: u32) -> Result<Option<ScannedImage>> {
        let mut p = RawParameters::default();
        s.api.check(unsafe { (s.api.get_parameters)(self.handle, &mut p) }, "reading scan parameters")?;
        if p.format != FRAME_GRAY && p.format != FRAME_RGB {
            bail!("three-pass colour scanners are not supported");
        }
        let bpl = p.bytes_per_line.max(1) as usize;
        // `lines` is -1 when the length isn't known in advance (feeders).
        let expected = if p.lines > 0 { bpl * p.lines as usize } else { bpl * 3300 };
        let mut data = Vec::with_capacity(expected);
        let mut chunk = vec![0u8; 256 * 1024];
        loop {
            if cancel.is_cancelled() {
                unsafe { (s.api.cancel)(self.handle) };
                return Ok(None);
            }
            let mut len: c_int = 0;
            // SAFETY: chunk is `chunk.len()` bytes.
            let st = unsafe { (s.api.read)(self.handle, chunk.as_mut_ptr(), chunk.len() as c_int, &mut len) };
            match st {
                STATUS_GOOD => data.extend_from_slice(&chunk[..len.max(0) as usize]),
                STATUS_EOF => break,
                STATUS_CANCELLED => return Ok(None),
                other => s.api.check(other, "reading image")?,
            }
        }
        let lines = data.len() / bpl;
        data.truncate(lines * bpl);
        let width = p.pixels_per_line.max(0) as u32;
        let (image, bilevel) = frame_to_image(&data, width, lines as u32, bpl, p.format, p.depth)?;
        Ok(Some(ScannedImage::Pixels { image, dpi, bilevel }))
    }
}

/// Normalise a `sane_start` status. The epsonds backend (ES-300W) reports
/// an *empty* feeder as `STATUS_JAMMED` when a scan starts, so on the first
/// sheet that means "load paper". Mid-batch, a jam is taken at its word.
fn start_status(status: Status, pages_so_far: usize, source: Source, device_id: &str) -> Status {
    let empty_reported_as_jam = device_id.starts_with("sane:epsonds:");
    if status == STATUS_JAMMED && pages_so_far == 0 && source.is_feeder() && empty_reported_as_jam {
        STATUS_NO_DOCS
    } else {
        status
    }
}

/// Map our [`Source`] onto the backend's own source names. Returns the name
/// and whether a separate `duplex` bool option must be switched on.
fn pick_source(list: &[String], source: Source) -> Option<(String, bool)> {
    let lower: Vec<String> = list.iter().map(|s| s.to_lowercase()).collect();
    let is_feeder = |s: &str| s.contains("adf") || s.contains("feeder") || s.contains("automatic document");
    let find = |pred: &dyn Fn(&str) -> bool| lower.iter().position(|s| pred(s)).map(|i| list[i].clone());
    match source {
        Source::Flatbed => {
            find(&|s| s.contains("flatbed") || s.contains("platen") || s == "normal").map(|s| (s, false))
        }
        Source::Feeder => find(&|s| is_feeder(s) && !s.contains("duplex") && !s.contains("back")).map(|s| (s, false)),
        Source::FeederDuplex => {
            find(&|s| s.contains("duplex")).map(|s| (s, false)).or_else(|| find(&|s| is_feeder(s)).map(|s| (s, true)))
        }
    }
}

fn pick_mode(list: &[String], color: ColorMode) -> Option<String> {
    let lower: Vec<String> = list.iter().map(|s| s.to_lowercase()).collect();
    let wanted: &[&str] = match color {
        ColorMode::Color => &["color", "colour", "24bit"],
        ColorMode::Gray => &["gray", "grey", "8bit"],
        ColorMode::BlackWhite => &["lineart", "binary", "black", "halftone"],
    };
    wanted.iter().find_map(|w| lower.iter().position(|s| s.contains(w))).map(|i| list[i].clone())
}

fn nearest_dpi(opt: &OptionDesc, wanted: u32) -> u32 {
    let candidates: Vec<u32> = match &opt.constraint {
        Constraint::Words(_) => opt.numbers().into_iter().map(|v| v.round() as u32).collect(),
        Constraint::Range { .. } => {
            let (lo, hi) = opt.range().unwrap_or((wanted as f64, wanted as f64));
            return wanted.clamp(lo as u32, hi as u32);
        }
        _ => return wanted,
    };
    candidates.into_iter().min_by_key(|&d| (d as i64 - wanted as i64).abs()).unwrap_or(wanted)
}

fn frame_to_image(
    data: &[u8],
    width: u32,
    height: u32,
    bpl: usize,
    format: c_int,
    depth: c_int,
) -> Result<(image::DynamicImage, bool)> {
    let w = width as usize;
    let channels = if format == FRAME_RGB { 3 } else { 1 };
    let mut out = Vec::with_capacity(w * height as usize * channels);
    for row in data.chunks_exact(bpl).take(height as usize) {
        match depth {
            // SANE line art: a set bit is *black* (the opposite of most formats).
            1 => out.extend((0..w).map(|x| if row[x / 8] & (0x80 >> (x % 8)) != 0 { 0u8 } else { 255 })),
            8 => out.extend_from_slice(&row[..w * channels]),
            // 16-bit samples are in host byte order.
            16 => out.extend(
                row[..w * channels * 2]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| (u16::from_ne_bytes([b[0], b[1]]) >> 8) as u8),
            ),
            d => bail!("unsupported bit depth {d}"),
        }
    }
    let bilevel = depth == 1;
    let image = if channels == 3 {
        image::DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(width, height, out).ok_or_else(|| anyhow!("short colour frame"))?,
        )
    } else {
        image::DynamicImage::ImageLuma8(
            image::GrayImage::from_raw(width, height, out).ok_or_else(|| anyhow!("short gray frame"))?,
        )
    };
    Ok((image, bilevel))
}

impl Device for SaneDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&mut self) -> Result<Capabilities> {
        let s = sane()?;
        let opts = self.options(&s);
        let mut caps = Capabilities::default();
        match Self::find(&opts, "source").map(|o| &o.constraint) {
            Some(Constraint::Strings(list)) => {
                let has_duplex_flag = Self::find(&opts, "duplex").is_some();
                for src in Source::ALL {
                    if let Some((_, needs_flag)) = pick_source(list, src)
                        && (!needs_flag || has_duplex_flag)
                    {
                        caps.sources.push(src);
                    }
                }
            }
            // No source option at all: a plain flatbed.
            _ => caps.sources.push(Source::Flatbed),
        }
        match Self::find(&opts, "mode").map(|o| &o.constraint) {
            Some(Constraint::Strings(list)) => {
                caps.color_modes = ColorMode::ALL.into_iter().filter(|&m| pick_mode(list, m).is_some()).collect()
            }
            _ => caps.color_modes = ColorMode::ALL.to_vec(),
        }
        caps.resolutions = match Self::find(&opts, "resolution") {
            Some(opt @ OptionDesc { constraint: Constraint::Words(_), .. }) => {
                let mut v: Vec<u32> = opt.numbers().into_iter().map(|d| d.round() as u32).collect();
                v.sort_unstable();
                v.dedup();
                v
            }
            Some(opt) => opt
                .range()
                .map(|(lo, hi)| super::standard_dpi_in(lo as u32, hi as u32))
                .unwrap_or_else(|| STANDARD_DPI.to_vec()),
            None => vec![300],
        };
        caps.auto_crop = Self::find(&opts, "adf-crp").is_some();
        Ok(caps)
    }

    fn scan(
        &mut self,
        settings: &ScanSettings,
        cancel: &CancelToken,
        on_page: &mut dyn FnMut(ScannedImage) -> Result<()>,
    ) -> Result<usize> {
        let mut count = 0;
        let dpi = {
            let s = sane()?;
            self.configure(&s, settings)?;
            let opts = self.options(&s);
            Self::find(&opts, "resolution")
                .and_then(|o| self.get_number(&s, o).ok())
                .map(|d| d.round() as u32)
                .unwrap_or(settings.dpi)
        };
        loop {
            // Take the lock per page, not for the whole batch, so device
            // discovery from the UI isn't frozen for the length of a 50-page job.
            let page = {
                let s = sane()?;
                if cancel.is_cancelled() {
                    unsafe { (s.api.cancel)(self.handle) };
                    break;
                }
                let st = start_status(unsafe { (s.api.start)(self.handle) }, count, settings.source, &self.info.id);
                if st == STATUS_NO_DOCS && count > 0 {
                    unsafe { (s.api.cancel)(self.handle) };
                    break;
                }
                if let Err(e) = s.api.check(st, "starting scan") {
                    unsafe { (s.api.cancel)(self.handle) };
                    return Err(e);
                }
                match self.read_frame(&s, cancel, dpi) {
                    Ok(Some(page)) => page,
                    Ok(None) => break,
                    Err(e) => {
                        unsafe { (s.api.cancel)(self.handle) };
                        return Err(e);
                    }
                }
            };
            on_page(page)?;
            count += 1;
            if !settings.source.is_feeder() {
                if let Ok(s) = sane() {
                    unsafe { (s.api.cancel)(self.handle) };
                }
                break;
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn picks_epsonds_sources() {
        let list = strings(&["ADF Front", "ADF Duplex"]);
        assert_eq!(pick_source(&list, Source::Feeder), Some(("ADF Front".into(), false)));
        assert_eq!(pick_source(&list, Source::FeederDuplex), Some(("ADF Duplex".into(), false)));
        assert_eq!(pick_source(&list, Source::Flatbed), None);
    }

    #[test]
    fn falls_back_to_duplex_flag() {
        let list = strings(&["Flatbed", "Automatic Document Feeder"]);
        assert_eq!(pick_source(&list, Source::FeederDuplex), Some(("Automatic Document Feeder".into(), true)));
        assert_eq!(pick_source(&list, Source::Flatbed), Some(("Flatbed".into(), false)));
    }

    #[test]
    fn picks_modes() {
        let list = strings(&["Lineart", "Gray", "Color"]);
        assert_eq!(pick_mode(&list, ColorMode::BlackWhite).as_deref(), Some("Lineart"));
        assert_eq!(pick_mode(&list, ColorMode::Gray).as_deref(), Some("Gray"));
        assert_eq!(pick_mode(&list, ColorMode::Color).as_deref(), Some("Color"));
    }

    #[test]
    fn epsonds_empty_feeder_jam_means_load_paper() {
        let id = "sane:epsonds:libusb:002:001";
        assert_eq!(start_status(STATUS_JAMMED, 0, Source::FeederDuplex, id), STATUS_NO_DOCS);
        // A jam after pages have fed is real.
        assert_eq!(start_status(STATUS_JAMMED, 3, Source::Feeder, id), STATUS_JAMMED);
        // Other backends keep reporting jams.
        assert_eq!(start_status(STATUS_JAMMED, 0, Source::Feeder, "sane:fujitsu:libusb:001:004"), STATUS_JAMMED);
        assert_eq!(start_status(STATUS_GOOD, 0, Source::Feeder, id), STATUS_GOOD);
    }

    #[test]
    fn lineart_set_bit_is_black() {
        // 10 pixels wide → 2 bytes per line; first pixel black, rest white.
        let data = [0b1000_0000u8, 0];
        let (img, bilevel) = frame_to_image(&data, 10, 1, 2, FRAME_GRAY, 1).unwrap();
        assert!(bilevel);
        let g = img.to_luma8();
        assert_eq!(g.get_pixel(0, 0).0[0], 0);
        assert_eq!(g.get_pixel(1, 0).0[0], 255);
    }
}
