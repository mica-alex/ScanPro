//! ImageCaptureCore backend: every scanner with a macOS driver (vendor ICA
//! drivers, and AirScan network scanners through Apple's own driver).
//!
//! ImageCaptureCore objects live on the **main thread** and deliver every
//! delegate callback on the main run loop, while [`Device::scan`] and
//! [`Backend::discover`] are called from worker threads. So all ICA objects
//! are kept in a main-thread-only registry ([`MAIN`]); workers post closures
//! to the main run loop ([`on_main`]) and wait for the delegates' reports on
//! a channel. When the caller *is* the main thread (the CLI), the waits spin
//! the run loop instead of blocking, so callbacks still arrive.
//!
//! If nothing is servicing the main run loop (a CLI calling from a worker
//! while main is blocked), every main-thread hop times out with an error
//! rather than hanging.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use crossbeam_channel::{Receiver, Sender};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_core_foundation::{CFRunLoop, CFType, kCFRunLoopCommonModes, kCFRunLoopDefaultMode};
use objc2_foundation::{NSError, NSIndexSet, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL};
use objc2_image_capture_core::{
    ICDevice, ICDeviceBrowser, ICDeviceBrowserDelegate, ICDeviceDelegate, ICDeviceTypeMask, ICScannerBitDepth,
    ICScannerDevice, ICScannerDeviceDelegate, ICScannerDocumentType, ICScannerFunctionalUnit,
    ICScannerFunctionalUnitDocumentFeeder, ICScannerFunctionalUnitType, ICScannerMeasurementUnit,
    ICScannerPixelDataType, ICScannerTransferMode,
};

use super::{
    Backend, CancelToken, Capabilities, ColorMode, Device, DeviceInfo, PaperSize, ScanSettings, ScannedImage, Source,
};

/// How long a main-thread hop may take before we conclude nothing is running
/// the main run loop.
const MAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Discovery waits at least this long for network (Bonjour/shared) scanners…
const DISCOVERY_MIN: Duration = Duration::from_millis(1500);
/// …and at most this long overall.
const DISCOVERY_MAX: Duration = Duration::from_secs(3);
/// Opening a session or selecting a functional unit can wake the scanner up.
const SESSION_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest silence between pages before a scan is declared stuck (a 600 dpi
/// colour page over Wi-Fi can legitimately take a minute).
const PAGE_TIMEOUT: Duration = Duration::from_secs(180);

/// `NSNotFound`, which `NSIndexSet` returns past its last index.
const NS_NOT_FOUND: usize = isize::MAX as usize;

// ─── Main-thread plumbing ─────────────────────────────────────────────────

fn is_main_thread() -> bool {
    MainThreadMarker::new().is_some()
}

/// Run `f` on the main thread and return its result.
fn on_main<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Result<R> {
    if is_main_thread() {
        return Ok(f());
    }
    let (tx, rx) = crossbeam_channel::bounded(1);
    post_main(move || {
        let _ = tx.send(f());
    })?;
    rx.recv_timeout(MAIN_TIMEOUT)
        .map_err(|_| anyhow!("ImageCaptureCore needs the main run loop, and it did not respond"))
}

/// Queue `f` on the main run loop without waiting (runs inline on main).
fn post_main(f: impl FnOnce() + Send + 'static) -> Result<()> {
    if is_main_thread() {
        f();
        return Ok(());
    }
    let main = CFRunLoop::main().ok_or_else(|| anyhow!("no main run loop"))?;
    // CFRunLoopPerformBlock wants a re-callable block; it is called once.
    let once = Cell::new(Some(f));
    let block = RcBlock::new(move || {
        if let Some(f) = once.take() {
            f();
        }
    });
    // Common modes, so the block also runs while the GUI is in a modal or
    // event-tracking loop (a menu held open, a window being resized).
    let mode: Option<&CFType> = unsafe { kCFRunLoopCommonModes }.map(|m| &**m);
    unsafe { main.perform_block(mode, Some(&block)) };
    main.wake_up();
    Ok(())
}

/// Let the main run loop make progress for up to `dt`: spin it when we are
/// the main thread, otherwise just sleep (someone else is spinning it).
fn pump(dt: Duration) {
    if is_main_thread() {
        let mode = unsafe { kCFRunLoopDefaultMode };
        CFRunLoop::run_in_mode(mode, dt.as_secs_f64(), true);
    } else {
        std::thread::sleep(dt);
    }
}

fn recv<T>(rx: &Receiver<T>, timeout: Duration) -> Option<T> {
    if !is_main_thread() {
        return rx.recv_timeout(timeout).ok();
    }
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(v) = rx.try_recv() {
            return Some(v);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        pump((deadline - now).min(Duration::from_millis(50)));
    }
}

fn error_text(error: Option<&NSError>) -> Option<String> {
    error.map(|e| e.localizedDescription().to_string())
}

// ─── Main-thread registry ─────────────────────────────────────────────────

struct MainState {
    /// Kept alive (and browsing) for the life of the process.
    _browser: Retained<ICDeviceBrowser>,
    delegate: Retained<BrowserDelegate>,
    /// Open device sessions, keyed by our device id.
    sessions: HashMap<String, Session>,
}

struct Session {
    scanner: Retained<ICScannerDevice>,
    _delegate: Retained<ScannerDelegate>,
}

thread_local! {
    /// Only ever touched on the main thread.
    static MAIN: RefCell<Option<MainState>> = const { RefCell::new(None) };
}

/// Readable from any thread, so discovery can wait without main-thread hops.
static LOCAL_ENUMERATED: AtomicBool = AtomicBool::new(false);
static BROWSER_STARTED: Mutex<Option<Instant>> = Mutex::new(None);

fn device_id(device: &ICDevice) -> String {
    unsafe { device.persistentIDString().or_else(|| device.UUIDString()) }
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{:p}", device as *const ICDevice))
}

fn is_scanner(device: &ICDevice) -> bool {
    // The type carries location bits too; the low byte is the device kind.
    unsafe { device.r#type() }.0 & 0xFF == ICDeviceTypeMask::Scanner.0
}

/// Start the shared browser once; it keeps the device list warm afterwards.
fn ensure_browser(mtm: MainThreadMarker) {
    MAIN.with_borrow_mut(|state| {
        if state.is_some() {
            return;
        }
        let delegate = BrowserDelegate::new(mtm);
        let browser = unsafe { ICDeviceBrowser::new() };
        unsafe {
            browser.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            // Scanners, wherever they are: local (USB), shared, Bonjour, remote.
            browser.setBrowsedDeviceTypeMask(ICDeviceTypeMask(ICDeviceTypeMask::Scanner.0 | 0xFF00));
            browser.start();
        }
        *BROWSER_STARTED.lock().unwrap() = Some(Instant::now());
        *state = Some(MainState { _browser: browser, delegate, sessions: HashMap::new() });
    });
}

/// Start the browser if needed and wait out the initial enumeration.
fn browse() -> Result<()> {
    on_main(|| ensure_browser(MainThreadMarker::new().expect("on main")))?;
    let started = BROWSER_STARTED.lock().unwrap().unwrap_or_else(Instant::now);
    loop {
        let elapsed = started.elapsed();
        let local_done = LOCAL_ENUMERATED.load(Ordering::SeqCst);
        if elapsed >= DISCOVERY_MAX || (local_done && elapsed >= DISCOVERY_MIN) {
            return Ok(());
        }
        pump(Duration::from_millis(50));
    }
}

fn with_devices<R>(f: impl FnOnce(&[Retained<ICDevice>]) -> R) -> R {
    MAIN.with_borrow(|state| {
        let devices = state.as_ref().map(|s| s.delegate.ivars().devices.borrow().clone());
        f(devices.as_deref().unwrap_or(&[]))
    })
}

fn with_scanner<R>(id: &str, f: impl FnOnce(&ICScannerDevice) -> R) -> Result<R> {
    MAIN.with_borrow(|state| {
        let session =
            state.as_ref().and_then(|s| s.sessions.get(id)).ok_or_else(|| anyhow!("scanner session is gone"))?;
        Ok(f(&session.scanner))
    })
}

// ─── Delegates ────────────────────────────────────────────────────────────

struct BrowserIvars {
    devices: RefCell<Vec<Retained<ICDevice>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ScanProIcaBrowserDelegate"]
    #[ivars = BrowserIvars]
    struct BrowserDelegate;

    unsafe impl NSObjectProtocol for BrowserDelegate {}

    unsafe impl ICDeviceBrowserDelegate for BrowserDelegate {
        #[unsafe(method(deviceBrowser:didAddDevice:moreComing:))]
        fn did_add(&self, _browser: &ICDeviceBrowser, device: &ICDevice, _more: bool) {
            if is_scanner(device) {
                self.ivars().devices.borrow_mut().push(device.retain());
            }
        }

        #[unsafe(method(deviceBrowser:didRemoveDevice:moreGoing:))]
        fn did_remove(&self, _browser: &ICDeviceBrowser, device: &ICDevice, _more: bool) {
            let gone = device_id(device);
            self.ivars().devices.borrow_mut().retain(|d| device_id(d) != gone);
        }

        #[unsafe(method(deviceBrowserDidEnumerateLocalDevices:))]
        fn did_enumerate_local(&self, _browser: &ICDeviceBrowser) {
            LOCAL_ENUMERATED.store(true, Ordering::SeqCst);
        }
    }
);

impl BrowserDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(BrowserIvars { devices: RefCell::new(Vec::new()) });
        unsafe { msg_send![super(this), init] }
    }
}

/// What a selected functional unit can do, read on the main thread.
#[derive(Clone, Debug, Default)]
struct UnitReport {
    resolutions: Vec<u32>,
    duplex: bool,
}

enum Event {
    SessionOpened(Result<(), String>),
    UnitSelected(Result<UnitReport, String>),
    Page(PathBuf),
    Completed(Option<String>),
    Failed(String),
}

struct ScannerIvars {
    tx: Sender<Event>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ScanProIcaScannerDelegate"]
    #[ivars = ScannerIvars]
    struct ScannerDelegate;

    unsafe impl NSObjectProtocol for ScannerDelegate {}

    unsafe impl ICDeviceDelegate for ScannerDelegate {
        #[unsafe(method(device:didCloseSessionWithError:))]
        fn did_close(&self, _device: &ICDevice, _error: Option<&NSError>) {}

        #[unsafe(method(didRemoveDevice:))]
        fn did_remove(&self, _device: &ICDevice) {
            self.send(Event::Failed("The scanner was disconnected.".into()));
        }

        #[unsafe(method(device:didOpenSessionWithError:))]
        fn did_open(&self, _device: &ICDevice, error: Option<&NSError>) {
            self.send(Event::SessionOpened(error_text(error).map_or(Ok(()), Err)));
        }

        #[unsafe(method(device:didEncounterError:))]
        fn did_encounter_error(&self, _device: &ICDevice, error: Option<&NSError>) {
            if let Some(e) = error_text(error) {
                self.send(Event::Failed(e));
            }
        }
    }

    unsafe impl ICScannerDeviceDelegate for ScannerDelegate {
        #[unsafe(method(scannerDevice:didSelectFunctionalUnit:error:))]
        fn did_select_unit(&self, _scanner: &ICScannerDevice, unit: &ICScannerFunctionalUnit, error: Option<&NSError>) {
            let result = match error_text(error) {
                Some(e) => Err(e),
                None => Ok(report(unit)),
            };
            self.send(Event::UnitSelected(result));
        }

        #[unsafe(method(scannerDevice:didScanToURL:))]
        fn did_scan_to_url(&self, _scanner: &ICScannerDevice, url: &NSURL) {
            if let Some(path) = url.to_file_path() {
                self.send(Event::Page(path));
            }
        }

        #[unsafe(method(scannerDevice:didCompleteScanWithError:))]
        fn did_complete(&self, _scanner: &ICScannerDevice, error: Option<&NSError>) {
            self.send(Event::Completed(error_text(error)));
        }
    }
);

impl ScannerDelegate {
    fn new(mtm: MainThreadMarker, tx: Sender<Event>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ScannerIvars { tx });
        unsafe { msg_send![super(this), init] }
    }

    fn send(&self, event: Event) {
        let _ = self.ivars().tx.send(event);
    }
}

fn index_set(set: &NSIndexSet) -> Vec<usize> {
    let mut out = Vec::with_capacity(set.count());
    let mut i = set.firstIndex();
    while i != NS_NOT_FOUND {
        out.push(i);
        i = set.indexGreaterThanIndex(i);
    }
    out
}

fn report(unit: &ICScannerFunctionalUnit) -> UnitReport {
    unsafe {
        let resolutions = index_set(&unit.supportedResolutions()).into_iter().map(|r| r as u32).collect();
        let duplex =
            unit.downcast_ref::<ICScannerFunctionalUnitDocumentFeeder>().is_some_and(|f| f.supportsDuplexScanning());
        UnitReport { resolutions, duplex }
    }
}

// ─── Backend ──────────────────────────────────────────────────────────────

pub struct IcaBackend;

impl IcaBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for IcaBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for IcaBackend {
    fn id(&self) -> &'static str {
        "ica"
    }

    fn available(&self) -> bool {
        true
    }

    fn discover(&self) -> Result<Vec<DeviceInfo>> {
        browse()?;
        on_main(|| {
            with_devices(|devices| {
                devices
                    .iter()
                    .map(|d| DeviceInfo {
                        id: format!("ica:{}", device_id(d)),
                        name: unsafe { d.name() }.map(|n| n.to_string()).unwrap_or_else(|| "Scanner".into()),
                        backend: "Image Capture".into(),
                    })
                    .collect()
            })
        })
    }

    fn open(&self, id: &str) -> Result<Box<dyn Device>> {
        browse()?;
        let (tx, rx) = crossbeam_channel::unbounded();
        let key = id.to_string();
        let delegate_tx = tx.clone();
        let name = on_main(move || -> Result<String> {
            let mtm = MainThreadMarker::new().expect("on main");
            let device = with_devices(|devices| devices.iter().find(|d| device_id(d) == key).cloned())
                .ok_or_else(|| anyhow!("scanner not found (is it switched on and connected?)"))?;
            let name = unsafe { device.name() }.map(|n| n.to_string()).unwrap_or_else(|| "Scanner".into());
            let scanner =
                device.downcast::<ICScannerDevice>().map_err(|_| anyhow!("Image Capture device is not a scanner"))?;
            let delegate = ScannerDelegate::new(mtm, delegate_tx);
            unsafe { scanner.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
            MAIN.with_borrow_mut(|state| {
                let state = state.as_mut().expect("browser started");
                state.sessions.insert(key, Session { scanner, _delegate: delegate });
            });
            Ok(name)
        })??;
        Ok(Box::new(IcaDevice {
            key: id.to_string(),
            info: DeviceInfo { id: format!("ica:{id}"), name, backend: "Image Capture".into() },
            tx,
            rx,
            session_open: false,
            caps: None,
        }))
    }
}

pub struct IcaDevice {
    /// Our device id without the `ica:` prefix; the [`MAIN`] session key.
    key: String,
    info: DeviceInfo,
    /// Lets main-thread closures report on the same channel as the delegate.
    tx: Sender<Event>,
    rx: Receiver<Event>,
    session_open: bool,
    caps: Option<Capabilities>,
}

impl IcaDevice {
    /// Wait for one specific kind of event, failing on device errors.
    fn wait<T>(
        &self,
        what: &str,
        timeout: Duration,
        mut pick: impl FnMut(Event) -> Option<Result<T, String>>,
    ) -> Result<T> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = recv(&self.rx, left).ok_or_else(|| anyhow!("timed out waiting for the scanner to {what}"))?;
            if let Event::Failed(e) = event {
                bail!("{e}");
            }
            if let Some(result) = pick(event) {
                return result.map_err(|e| anyhow!("could not {what}: {e}"));
            }
        }
    }

    fn ensure_session(&mut self) -> Result<()> {
        if self.session_open {
            return Ok(());
        }
        let key = self.key.clone();
        on_main(move || with_scanner(&key, |s| unsafe { s.requestOpenSession() }))??;
        self.wait("open a session", SESSION_TIMEOUT, |e| match e {
            Event::SessionOpened(r) => Some(r),
            _ => None,
        })?;
        self.session_open = true;
        Ok(())
    }

    fn unit_types(&self) -> Result<Vec<usize>> {
        let key = self.key.clone();
        on_main(move || {
            with_scanner(&key, |s| unsafe {
                s.availableFunctionalUnitTypes().iter().map(|n| n.unsignedIntegerValue()).collect()
            })
        })?
    }

    fn select_unit(&self, kind: ICScannerFunctionalUnitType) -> Result<UnitReport> {
        let key = self.key.clone();
        let tx = self.tx.clone();
        on_main(move || {
            with_scanner(&key, |s| unsafe {
                // Re-selecting the current unit may not produce a callback.
                let current = s.selectedFunctionalUnit();
                if current.r#type() == kind {
                    let _ = tx.send(Event::UnitSelected(Ok(report(&current))));
                } else {
                    s.requestSelectFunctionalUnit(kind);
                }
            })
        })??;
        self.wait("select the scan source", SESSION_TIMEOUT, |e| match e {
            Event::UnitSelected(r) => Some(r),
            _ => None,
        })
    }
}

impl Drop for IcaDevice {
    fn drop(&mut self) {
        let key = std::mem::take(&mut self.key);
        let open = self.session_open;
        let _ = post_main(move || {
            MAIN.with_borrow_mut(|state| {
                if let Some(session) = state.as_mut().and_then(|s| s.sessions.remove(&key)) {
                    unsafe {
                        if open {
                            session.scanner.requestCloseSession();
                        }
                        session.scanner.setDelegate(None);
                    }
                }
            });
        });
    }
}

fn nearest(supported: &[u32], dpi: u32) -> u32 {
    supported.iter().copied().min_by_key(|r| r.abs_diff(dpi)).unwrap_or(dpi)
}

fn document_type(paper: PaperSize) -> ICScannerDocumentType {
    match paper {
        PaperSize::Letter => ICScannerDocumentType::TypeUSLetter,
        PaperSize::Legal => ICScannerDocumentType::TypeUSLegal,
        PaperSize::A4 => ICScannerDocumentType::TypeA4,
        PaperSize::A5 => ICScannerDocumentType::TypeA5,
        PaperSize::Auto | PaperSize::Maximum => ICScannerDocumentType::TypeDefault,
    }
}

static SCAN_SERIAL: AtomicUsize = AtomicUsize::new(0);

/// Configure the selected unit and start the scan. Runs on the main thread.
fn configure_and_start(scanner: &ICScannerDevice, settings: &ScanSettings, dpi: u32, dir: &Path) -> Result<()> {
    unsafe {
        let unit = scanner.selectedFunctionalUnit();
        if index_set(&unit.supportedMeasurementUnits()).contains(&ICScannerMeasurementUnit::Inches.0) {
            unit.setMeasurementUnit(ICScannerMeasurementUnit::Inches);
        }
        unit.setResolution(dpi as usize);

        let depths = index_set(&unit.supportedBitDepths());
        let (pixels, depth) = match settings.color {
            ColorMode::Color => (ICScannerPixelDataType::RGB, ICScannerBitDepth::Depth8Bits),
            ColorMode::Gray => (ICScannerPixelDataType::Gray, ICScannerBitDepth::Depth8Bits),
            ColorMode::BlackWhite if depths.contains(&1) => (ICScannerPixelDataType::BW, ICScannerBitDepth::Depth1Bit),
            // No 1-bit mode: scan gray; the pipeline thresholds it.
            ColorMode::BlackWhite => (ICScannerPixelDataType::Gray, ICScannerBitDepth::Depth8Bits),
        };
        unit.setPixelDataType(pixels);
        unit.setBitDepth(depth);

        if let Some(feeder) = unit.downcast_ref::<ICScannerFunctionalUnitDocumentFeeder>() {
            let wanted = document_type(settings.paper);
            if index_set(&feeder.supportedDocumentTypes()).contains(&wanted.0) {
                feeder.setDocumentType(wanted);
            }
            if feeder.supportsDuplexScanning() {
                feeder.setDuplexScanningEnabled(settings.source == Source::FeederDuplex);
            }
        }

        // Scan area in inches, origin top-left, clamped to what the unit has.
        let physical = unit.physicalSize();
        let (w, h) = match settings.paper.size_mm() {
            Some((w_mm, h_mm)) => (w_mm / 25.4, h_mm / 25.4),
            None => (physical.width, physical.height),
        };
        let (w, h) = (w.min(physical.width), h.min(physical.height));
        if w > 0.0 && h > 0.0 {
            unit.setScanArea(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)));
        }

        scanner.setTransferMode(ICScannerTransferMode::FileBased);
        let url = NSURL::from_directory_path(dir).ok_or_else(|| anyhow!("bad temp dir {dir:?}"))?;
        scanner.setDownloadsDirectory(&url);
        scanner.setDocumentName(&NSString::from_str("scanpro-page"));
        // Lossless; the pipeline decides the final encoding.
        scanner.setDocumentUTI(&NSString::from_str("public.png"));
        scanner.requestScan();
    }
    Ok(())
}

impl Device for IcaDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&mut self) -> Result<Capabilities> {
        if let Some(caps) = &self.caps {
            return Ok(caps.clone());
        }
        self.ensure_session()?;
        let types = self.unit_types()?;
        let mut caps = Capabilities { color_modes: ColorMode::ALL.to_vec(), auto_crop: false, ..Default::default() };
        let mut resolutions = Vec::new();
        if types.contains(&ICScannerFunctionalUnitType::Flatbed.0) {
            let unit = self.select_unit(ICScannerFunctionalUnitType::Flatbed)?;
            caps.sources.push(Source::Flatbed);
            resolutions.extend(unit.resolutions);
        }
        if types.contains(&ICScannerFunctionalUnitType::DocumentFeeder.0) {
            let unit = self.select_unit(ICScannerFunctionalUnitType::DocumentFeeder)?;
            caps.sources.push(Source::Feeder);
            if unit.duplex {
                caps.sources.push(Source::FeederDuplex);
            }
            resolutions.extend(unit.resolutions);
        }
        resolutions.sort_unstable();
        resolutions.dedup();
        caps.resolutions = resolutions;
        self.caps = Some(caps.clone());
        Ok(caps)
    }

    fn scan(
        &mut self,
        settings: &ScanSettings,
        cancel: &CancelToken,
        on_page: &mut dyn FnMut(ScannedImage) -> Result<()>,
    ) -> Result<usize> {
        self.ensure_session()?;
        let kind = if settings.source.is_feeder() {
            ICScannerFunctionalUnitType::DocumentFeeder
        } else {
            ICScannerFunctionalUnitType::Flatbed
        };
        let unit = self.select_unit(kind)?;
        if settings.source == Source::FeederDuplex && !unit.duplex {
            bail!("This scanner cannot scan both sides of a page.");
        }
        let dpi = nearest(&unit.resolutions, settings.dpi);

        let dir = std::env::temp_dir().join(format!(
            "scanpro-ica-{}-{}",
            std::process::id(),
            SCAN_SERIAL.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {dir:?}"))?;
        let _cleanup = RemoveOnDrop(dir.clone());

        // Drop stale events (e.g. a late completion from a cancelled scan).
        while self.rx.try_recv().is_ok() {}

        let key = self.key.clone();
        let start_settings = settings.clone();
        let start_dir = dir.clone();
        on_main(move || with_scanner(&key, |s| configure_and_start(s, &start_settings, dpi, &start_dir)))???;

        let mut pages = 0;
        let mut cancelled = false;
        let mut failure: Option<anyhow::Error> = None;
        let mut last_activity = Instant::now();
        loop {
            if cancel.is_cancelled() && !cancelled {
                cancelled = true;
                let key = self.key.clone();
                on_main(move || with_scanner(&key, |s| unsafe { s.cancelScan() }))??;
            }
            match recv(&self.rx, Duration::from_millis(200)) {
                Some(Event::Page(path)) => {
                    last_activity = Instant::now();
                    let bytes = std::fs::read(&path).with_context(|| format!("reading {path:?}"))?;
                    let _ = std::fs::remove_file(&path);
                    if cancelled || failure.is_some() {
                        continue;
                    }
                    let format = image::guess_format(&bytes).unwrap_or(image::ImageFormat::Png);
                    pages += 1;
                    if let Err(e) = on_page(ScannedImage::Encoded { bytes, format, dpi }) {
                        // Stop the scanner, but drain until it confirms.
                        failure = Some(e);
                        let key = self.key.clone();
                        on_main(move || with_scanner(&key, |s| unsafe { s.cancelScan() }))??;
                    }
                }
                Some(Event::Completed(error)) => {
                    if let Some(e) = failure {
                        return Err(e);
                    }
                    if cancelled {
                        return Ok(pages);
                    }
                    return match (pages, error) {
                        (0, Some(e)) => Err(anyhow!("Scan failed: {e}")),
                        (0, None) if settings.source.is_feeder() => {
                            Err(anyhow!("No pages were scanned. Is paper loaded in the feeder?"))
                        }
                        // Running out of paper mid-batch is the normal end.
                        (n, _) => Ok(n),
                    };
                }
                Some(Event::Failed(e)) => bail!("{e}"),
                Some(_) => {}
                None => {
                    if last_activity.elapsed() > PAGE_TIMEOUT {
                        bail!("The scanner stopped responding.");
                    }
                }
            }
        }
    }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_resolution() {
        assert_eq!(nearest(&[150, 300, 600], 400), 300);
        assert_eq!(nearest(&[150, 300, 600], 500), 600);
        assert_eq!(nearest(&[], 300), 300);
    }

    /// Runs on the test harness's worker thread with nobody spinning the main
    /// run loop, so this exercises the "main thread unresponsive" path: it
    /// must return (an error or a list), never hang.
    #[test]
    #[ignore]
    fn discovery_returns() {
        let started = Instant::now();
        let result = IcaBackend::new().discover();
        assert!(started.elapsed() < MAIN_TIMEOUT + DISCOVERY_MAX + Duration::from_secs(2));
        println!("{result:?}");
    }
}
