//! WIA 2.0 backend (Windows). Nearly every scanner ships a WIA driver, so this
//! is the default path on Windows.
//!
//! COM objects are not `Send`, so [`WiaDevice`] keeps only the device id and
//! every call builds its COM objects on the thread that uses them. A scan runs
//! `IWiaTransfer::Download` on a dedicated MTA thread; the transfer callback
//! ships each finished page over a channel, and [`Device::scan`] hands it to
//! `on_page` on the caller's thread while the next sheet is still feeding.

use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::Sender;
use windows::Win32::Devices::ImageAcquisition::*;
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE};
use windows::Win32::System::Com::StructuredStorage::{
    PROPSPEC, PROPSPEC_0, PROPVARIANT, PRSPEC_PROPID, PropVariantClear,
};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, IStream,
    STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET, TYMED_FILE,
};
use windows::Win32::System::Variant::{VARENUM, VT_BSTR, VT_CLSID, VT_I4, VT_UI4, VT_VECTOR};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::core::{BSTR, GUID, HRESULT, Interface, implement};

use super::{
    Backend, CancelToken, Capabilities, ColorMode, Device, DeviceInfo, PaperSize, STANDARD_DPI, ScanSettings,
    ScannedImage, Source, standard_dpi_in,
};

/// `GET_STIDEVICE_TYPE(WIA_DIP_DEV_TYPE)` for a scanner (`StiDeviceTypeScanner`,
/// which lives in the Fax bindings; not worth a feature for one constant).
const STI_DEVICE_TYPE_SCANNER: i32 = 1;

pub struct WiaBackend;

impl WiaBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Backend for WiaBackend {
    fn id(&self) -> &'static str {
        "wia"
    }

    fn available(&self) -> bool {
        true
    }

    fn discover(&self) -> Result<Vec<DeviceInfo>> {
        let _com = ComGuard::init()?;
        let mut found = Vec::new();
        unsafe {
            let mgr = device_manager()?;
            let devices = mgr.EnumDeviceInfo(WIA_DEVINFO_ENUM_LOCAL as i32)?;
            loop {
                let mut slot = [None];
                let mut fetched = 0;
                // S_FALSE (end of enumeration) still comes back as Ok.
                devices.Next(1, slot.as_mut_ptr(), &mut fetched)?;
                let Some(props) = slot[0].take().filter(|_| fetched == 1) else { break };
                let dev_type = read_i32(&props, WIA_DIP_DEV_TYPE).unwrap_or(0);
                if (dev_type >> 16) != STI_DEVICE_TYPE_SCANNER {
                    continue;
                }
                let Some(id) = read_string(&props, WIA_DIP_DEV_ID) else { continue };
                let name = read_string(&props, WIA_DIP_DEV_NAME)
                    .or_else(|| read_string(&props, WIA_DIP_VEND_DESC))
                    .unwrap_or_else(|| "WIA scanner".to_string());
                found.push(DeviceInfo { id: format!("wia:{id}"), name, backend: "WIA".into() });
            }
        }
        Ok(found)
    }

    fn open(&self, id: &str) -> Result<Box<dyn Device>> {
        // Opening once up front turns "device unplugged" into an error here
        // rather than at the first scan.
        let _com = ComGuard::init()?;
        let name = unsafe {
            let root = create_device(id)?;
            root.cast::<IWiaPropertyStorage>()
                .ok()
                .and_then(|p| read_string(&p, WIA_DIP_DEV_NAME))
                .unwrap_or_else(|| "WIA scanner".to_string())
        };
        Ok(Box::new(WiaDevice {
            id: id.to_string(),
            info: DeviceInfo { id: format!("wia:{id}"), name, backend: "WIA".into() },
        }))
    }
}

pub struct WiaDevice {
    /// The WIA device id (`WIA_DIP_DEV_ID`), without our `"wia:"` prefix.
    id: String,
    info: DeviceInfo,
}

impl Device for WiaDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&mut self) -> Result<Capabilities> {
        let _com = ComGuard::init()?;
        let mut caps = Capabilities::default();
        unsafe {
            let root = create_device(&self.id)?;
            let items = DataItems::find(&root)?;
            // Resolution and colour modes are per item; read them from
            // whichever item exists (the feeder first, it is what we default to).
            let primary = items.feeder.as_ref().or(items.flatbed.as_ref());

            if items.flatbed.is_some() {
                caps.sources.push(Source::Flatbed);
            }
            if let Some(feeder) = &items.feeder {
                caps.sources.push(Source::Feeder);
                let props = feeder.cast::<IWiaPropertyStorage>()?;
                let duplex = read_i32(&props, WIA_DPS_DOCUMENT_HANDLING_CAPABILITIES)
                    .map(|c| c as u32 & DUP != 0)
                    .unwrap_or(false)
                    || valid_values(&props, WIA_IPS_DOCUMENT_HANDLING_SELECT)
                        .is_some_and(|v| v.contains(&(DUPLEX as i32)));
                if duplex {
                    caps.sources.push(Source::FeederDuplex);
                }
                caps.auto_crop |=
                    valid_values(&props, WIA_IPS_PAGE_SIZE).is_some_and(|v| v.contains(&(WIA_PAGE_AUTO as i32)));
            }
            if let Some(flatbed) = &items.flatbed {
                let props = flatbed.cast::<IWiaPropertyStorage>()?;
                caps.auto_crop |=
                    valid_values(&props, WIA_IPS_AUTO_CROP).is_some_and(|v| v.contains(&(WIA_AUTO_CROP_SINGLE as i32)));
            }

            if let Some(item) = primary {
                let props = item.cast::<IWiaPropertyStorage>()?;
                if let Some(v) = valid_values(&props, WIA_IPS_XRES) {
                    caps.resolutions = v.into_iter().filter(|d| *d > 0).map(|d| d as u32).collect();
                }
                if let Some(v) = valid_values(&props, WIA_IPA_DATATYPE) {
                    for (mode, wia) in [
                        (ColorMode::Color, WIA_DATA_COLOR),
                        (ColorMode::Gray, WIA_DATA_GRAYSCALE),
                        (ColorMode::BlackWhite, WIA_DATA_THRESHOLD),
                    ] {
                        if v.contains(&(wia as i32)) {
                            caps.color_modes.push(mode);
                        }
                    }
                }
            }
        }
        if caps.sources.is_empty() {
            bail!("{} exposes no flatbed or feeder", self.info.name);
        }
        if caps.resolutions.is_empty() {
            caps.resolutions = STANDARD_DPI[..7].to_vec();
        }
        caps.resolutions.sort_unstable();
        caps.resolutions.dedup();
        if caps.color_modes.is_empty() {
            caps.color_modes = ColorMode::ALL.to_vec();
        }
        Ok(caps)
    }

    fn scan(
        &mut self,
        settings: &ScanSettings,
        cancel: &CancelToken,
        on_page: &mut dyn FnMut(ScannedImage) -> Result<()>,
    ) -> Result<usize> {
        let (tx, rx) = crossbeam_channel::unbounded();
        // Set when `on_page` fails, so the transfer stops feeding paper
        // without marking the caller's own token as user-cancelled.
        let abort = CancelToken::default();
        let worker = {
            let (id, settings, cancel, abort) = (self.id.clone(), settings.clone(), cancel.clone(), abort.clone());
            std::thread::Builder::new().name("wia-transfer".into()).spawn(
                move || -> std::result::Result<(), HRESULT> {
                    let _com = ComGuard::init().map_err(|_| RPC_E_CHANGED_MODE)?;
                    unsafe { transfer(&id, &settings, &cancel, &abort, tx) }
                },
            )?
        };

        let dpi = settings.dpi;
        let mut pages = 0;
        let mut failure: Option<anyhow::Error> = None;
        let mut device_status: Option<HRESULT> = None;
        // Ends when the worker drops its sender, i.e. when Download returns.
        for msg in rx {
            match msg {
                Msg::Page(bytes) => {
                    if failure.is_some() {
                        continue;
                    }
                    let delivered = decode_page(bytes, dpi, settings.color).and_then(|img| on_page(img));
                    match delivered {
                        Ok(()) => pages += 1,
                        Err(e) => {
                            abort.cancel();
                            failure = Some(e);
                        }
                    }
                }
                Msg::DeviceStatus(hr) => device_status = Some(hr),
            }
        }
        let outcome = worker.join().map_err(|_| anyhow!("WIA transfer thread panicked"))?;
        if let Some(e) = failure {
            return Err(e);
        }
        if cancel.is_cancelled() {
            return Ok(pages);
        }

        let hr = match outcome {
            Err(hr) => Some(hr),
            Ok(()) => device_status.filter(|hr| hr.is_err()),
        };
        match hr {
            // Running out of paper is how every feeder batch ends; it is only
            // an error when nothing was fed at all.
            Some(hr) if hr == WIA_ERROR_PAPER_EMPTY || hr == WIA_STATUS_END_OF_MEDIA => {
                if pages == 0 {
                    bail!("Load paper into the feeder");
                }
                Ok(pages)
            }
            Some(hr) => Err(wia_error(hr)),
            None if pages == 0 && settings.source.is_feeder() => bail!("Load paper into the feeder"),
            None => Ok(pages),
        }
    }
}

enum Msg {
    /// One page's encoded bytes (PNG or BMP, whichever the driver accepted).
    Page(Vec<u8>),
    /// A device status reported mid-transfer (paper empty/jam, cover open…).
    DeviceStatus(HRESULT),
}

/// Runs on the transfer thread: configure the item, then `Download`, which
/// blocks until the batch ends and calls back into [`TransferSink`] per page.
unsafe fn transfer(
    id: &str,
    settings: &ScanSettings,
    cancel: &CancelToken,
    abort: &CancelToken,
    tx: Sender<Msg>,
) -> std::result::Result<(), HRESULT> {
    unsafe {
        let root = create_device(id).map_err(|e| hresult_of(&e))?;
        let items = DataItems::find(&root).map_err(|e| hresult_of(&e))?;
        let item = match settings.source {
            Source::Flatbed => items.flatbed.or(items.feeder),
            Source::Feeder | Source::FeederDuplex => items.feeder.or(items.flatbed),
        }
        .ok_or(WIA_ERROR_OFFLINE)?;
        configure(&item, settings).map_err(|e| hresult_of(&e))?;

        let sink: IWiaTransferCallback =
            TransferSink { tx, cancel: cancel.clone(), abort: abort.clone(), current: Mutex::new(None) }.into();
        let xfer = item.cast::<IWiaTransfer>().map_err(|e| e.code())?;
        let result = xfer.Download(0, &sink);
        // Drivers that never send END_OF_STREAM for the last page still left
        // it in the sink; flush it before reporting.
        let inner = sink.cast_object_ref::<TransferSink>().map_err(|e| e.code())?;
        inner.flush();
        result.map_err(|e| e.code())
    }
}

/// Apply `settings` to a data item. Required properties fail the scan;
/// optional refinements (auto-crop, page size) are best-effort.
unsafe fn configure(item: &IWiaItem2, settings: &ScanSettings) -> Result<()> {
    unsafe {
        let props = item.cast::<IWiaPropertyStorage>()?;
        let is_feeder = item.GetItemCategory().is_ok_and(|c| c == WIA_CATEGORY_FEEDER);

        if is_feeder {
            let wanted: &[u32] = if settings.source == Source::FeederDuplex {
                &[FEEDER | DUPLEX | FRONT_FIRST, DUPLEX | FRONT_FIRST, FEEDER | DUPLEX, DUPLEX]
            } else {
                &[FEEDER | FRONT_ONLY, FRONT_ONLY, FEEDER]
            };
            // Drivers disagree on which flag combination they accept on the
            // WIA 2.0 feeder item; take the first one that sticks.
            let accepted =
                wanted.iter().any(|v| write_i32(&props, WIA_IPS_DOCUMENT_HANDLING_SELECT, *v as i32).is_ok());
            if !accepted && settings.source == Source::FeederDuplex {
                bail!("This scanner does not support double-sided scanning");
            }
            let _ = write_i32(&props, WIA_IPS_PAGES, ALL_PAGES as i32);
        }

        let (data_type, depth) = match settings.color {
            ColorMode::Color => (WIA_DATA_COLOR, 24),
            ColorMode::Gray => (WIA_DATA_GRAYSCALE, 8),
            ColorMode::BlackWhite => (WIA_DATA_THRESHOLD, 1),
        };
        write_i32(&props, WIA_IPA_DATATYPE, data_type as i32).context("setting colour mode")?;
        write_i32(&props, WIA_IPA_DEPTH, depth).context("setting bit depth")?;
        let dpi = settings.dpi as i32;
        write_i32(&props, WIA_IPS_XRES, dpi).context("setting resolution")?;
        write_i32(&props, WIA_IPS_YRES, dpi).context("setting resolution")?;

        // Extents are in pixels at the current resolution; the maxima are in
        // thousandths of an inch.
        let max_w = read_i32(&props, WIA_IPS_MAX_HORIZONTAL_SIZE).filter(|v| *v > 0);
        let max_h = read_i32(&props, WIA_IPS_MAX_VERTICAL_SIZE).filter(|v| *v > 0);
        let to_px = |thou: i32| (thou as i64 * dpi as i64 / 1000) as i32;
        let extent = match settings.paper {
            PaperSize::Auto => {
                let auto = if is_feeder {
                    write_i32(&props, WIA_IPS_PAGE_SIZE, WIA_PAGE_AUTO as i32)
                } else {
                    write_i32(&props, WIA_IPS_AUTO_CROP, WIA_AUTO_CROP_SINGLE as i32)
                };
                // No auto-detect: fall back to the whole area.
                if auto.is_ok() { None } else { max_w.zip(max_h).map(|(w, h)| (to_px(w), to_px(h))) }
            }
            PaperSize::Maximum => max_w.zip(max_h).map(|(w, h)| (to_px(w), to_px(h))),
            paper => paper.size_mm().map(|(w, h)| {
                let px = |mm: f64| (mm / 25.4 * dpi as f64).round() as i32;
                let (w, h) = (px(w), px(h));
                // Clamp to what the hardware can do; a Legal request on an
                // A4 flatbed scans the full bed rather than failing.
                (max_w.map_or(w, |m| w.min(to_px(m))), max_h.map_or(h, |m| h.min(to_px(m))))
            }),
        };
        if let Some((w, h)) = extent {
            let _ = write_i32(&props, WIA_IPS_XPOS, 0);
            let _ = write_i32(&props, WIA_IPS_YPOS, 0);
            write_i32(&props, WIA_IPS_XEXTENT, w).context("setting scan width")?;
            write_i32(&props, WIA_IPS_YEXTENT, h).context("setting scan height")?;
        }

        // TYMED_FILE is what IWiaTransfer streams use. PNG keeps 1-bit and
        // 8-bit pages small; BMP is the format every driver must support.
        let _ = write_i32(&props, WIA_IPA_TYMED, TYMED_FILE.0);
        if write_guid(&props, WIA_IPA_FORMAT, &WiaImgFmt_PNG).is_err() {
            write_guid(&props, WIA_IPA_FORMAT, &WiaImgFmt_BMP).context("setting image format")?;
        }
        Ok(())
    }
}

#[implement(IWiaTransferCallback)]
struct TransferSink {
    tx: Sender<Msg>,
    cancel: CancelToken,
    abort: CancelToken,
    /// The stream the driver is currently writing a page into.
    current: Mutex<Option<IStream>>,
}

impl TransferSink {
    fn flush(&self) {
        let stream = self.current.lock().ok().and_then(|mut s| s.take());
        if let Some(stream) = stream {
            match unsafe { read_stream(&stream) } {
                Ok(bytes) if !bytes.is_empty() => {
                    let _ = self.tx.send(Msg::Page(bytes));
                }
                Ok(_) => {}
                Err(e) => log::warn!("WIA: could not read page stream: {e}"),
            }
        }
    }

    fn stop_requested(&self) -> bool {
        self.cancel.is_cancelled() || self.abort.is_cancelled()
    }
}

impl IWiaTransferCallback_Impl for TransferSink_Impl {
    fn TransferCallback(&self, _flags: i32, params: *const WiaTransferParams) -> windows::core::Result<()> {
        if let Some(params) = unsafe { params.as_ref() } {
            match params.lMessage as u32 {
                WIA_TRANSFER_MSG_END_OF_STREAM => self.flush(),
                WIA_TRANSFER_MSG_DEVICE_STATUS if params.hrErrorStatus.is_err() => {
                    let _ = self.tx.send(Msg::DeviceStatus(params.hrErrorStatus));
                }
                _ => {}
            }
        }
        // S_FALSE from the callback is WIA's "cancel the transfer".
        if self.stop_requested() { Err(S_FALSE.into()) } else { Ok(()) }
    }

    fn GetNextStream(&self, _flags: i32, _item_name: &BSTR, _full_item_name: &BSTR) -> windows::core::Result<IStream> {
        if self.stop_requested() {
            return Err(S_FALSE.into());
        }
        // A new page starting means the previous one is complete, even if
        // the driver skipped END_OF_STREAM.
        self.flush();
        let stream = unsafe { SHCreateMemStream(None) }
            .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY))?;
        if let Ok(mut current) = self.current.lock() {
            *current = Some(stream.clone());
        }
        Ok(stream)
    }
}

unsafe fn read_stream(stream: &IStream) -> windows::core::Result<Vec<u8>> {
    unsafe {
        let mut stat = STATSTG::default();
        stream.Stat(&mut stat, STATFLAG_NONAME)?;
        stream.Seek(0, STREAM_SEEK_SET, None)?;
        let mut bytes = vec![0u8; stat.cbSize as usize];
        let mut filled = 0;
        while filled < bytes.len() {
            let mut read = 0u32;
            let want = (bytes.len() - filled).min(u32::MAX as usize) as u32;
            stream.Read(bytes[filled..].as_mut_ptr().cast(), want, Some(&mut read)).ok()?;
            if read == 0 {
                break;
            }
            filled += read as usize;
        }
        bytes.truncate(filled);
        Ok(bytes)
    }
}

/// PNG passes through as-is; BMP is decoded here (the `image` build has no
/// BMP codec, and WIA's DIBs need special-casing anyway).
fn decode_page(bytes: Vec<u8>, dpi: u32, color: ColorMode) -> Result<ScannedImage> {
    if bytes.starts_with(b"\x89PNG") {
        return Ok(ScannedImage::Encoded { bytes, format: image::ImageFormat::Png, dpi });
    }
    let image = decode_dib(&bytes)?;
    Ok(ScannedImage::Pixels { image, dpi, bilevel: color == ColorMode::BlackWhite })
}

/// Decode an uncompressed BMP/DIB (1, 8, 24 or 32 bpp), with or without the
/// 14-byte file header. Feeder drivers may write a height of 0 when the page
/// length was unknown up front; the height is then derived from the data.
fn decode_dib(bytes: &[u8]) -> Result<image::DynamicImage> {
    let (header, file_offset) = if bytes.starts_with(b"BM") {
        let off = u32::from_le_bytes(bytes.get(10..14).context("truncated BMP")?.try_into()?);
        (&bytes[14..], Some(off as usize))
    } else {
        (bytes, None)
    };
    let le_u32 = |o: usize| -> Result<u32> {
        Ok(u32::from_le_bytes(header.get(o..o + 4).context("truncated DIB header")?.try_into()?))
    };
    let le_u16 = |o: usize| -> Result<u16> {
        Ok(u16::from_le_bytes(header.get(o..o + 2).context("truncated DIB header")?.try_into()?))
    };
    let header_size = le_u32(0)? as usize;
    let width = le_u32(4)? as i32;
    let raw_height = le_u32(8)? as i32;
    let bpp = le_u16(14)?;
    let compression = le_u32(16)?;
    if compression != 0 && compression != 3 {
        bail!("compressed BMP from scanner is not supported (compression {compression})");
    }
    if width <= 0 {
        bail!("BMP has invalid width {width}");
    }
    let colors_used = le_u32(32).unwrap_or(0) as usize;
    let palette_len = match bpp {
        1 | 8 => {
            if colors_used > 0 {
                colors_used
            } else {
                1 << bpp
            }
        }
        _ => 0,
    };
    let palette_start = header_size;
    let palette: Vec<[u8; 3]> = (0..palette_len)
        .filter_map(|i| header.get(palette_start + i * 4..palette_start + i * 4 + 3))
        .map(|bgr| [bgr[2], bgr[1], bgr[0]])
        .collect();
    let mut pixel_start = palette_start + palette_len * 4 + if compression == 3 && header_size == 40 { 12 } else { 0 };
    if let Some(off) = file_offset {
        pixel_start = off.saturating_sub(14);
    }
    let data = header.get(pixel_start..).context("BMP has no pixel data")?;
    let width = width as usize;
    let stride = (width * bpp as usize).div_ceil(32) * 4;
    let top_down = raw_height < 0;
    let height = match raw_height.unsigned_abs() as usize {
        0 => data.len() / stride,
        h => h.min(data.len() / stride),
    };
    if height == 0 {
        bail!("BMP has no rows");
    }
    let row = |y: usize| {
        let src_y = if top_down { y } else { height - 1 - y };
        &data[src_y * stride..src_y * stride + stride]
    };
    // Gray palettes (the usual 1- and 8-bit case) decode to Luma8 so the
    // pipeline keeps the smaller representation.
    let gray_palette = palette.iter().all(|c| c[0] == c[1] && c[1] == c[2]);

    let image = match bpp {
        1 | 8 => {
            let index = |r: &[u8], x: usize| -> usize {
                if bpp == 8 { r[x] as usize } else { ((r[x / 8] >> (7 - x % 8)) & 1) as usize }
            };
            if gray_palette {
                let mut buf = Vec::with_capacity(width * height);
                for y in 0..height {
                    let r = row(y);
                    buf.extend((0..width).map(|x| palette.get(index(r, x)).map_or(0, |c| c[0])));
                }
                image::DynamicImage::ImageLuma8(
                    image::GrayImage::from_raw(width as u32, height as u32, buf).context("BMP size")?,
                )
            } else {
                let mut buf = Vec::with_capacity(width * height * 3);
                for y in 0..height {
                    let r = row(y);
                    for x in 0..width {
                        buf.extend_from_slice(&palette.get(index(r, x)).copied().unwrap_or([0; 3]));
                    }
                }
                image::DynamicImage::ImageRgb8(
                    image::RgbImage::from_raw(width as u32, height as u32, buf).context("BMP size")?,
                )
            }
        }
        24 | 32 => {
            let step = bpp as usize / 8;
            let mut buf = Vec::with_capacity(width * height * 3);
            for y in 0..height {
                for px in row(y)[..width * step].chunks_exact(step) {
                    buf.extend_from_slice(&[px[2], px[1], px[0]]);
                }
            }
            image::DynamicImage::ImageRgb8(
                image::RgbImage::from_raw(width as u32, height as u32, buf).context("BMP size")?,
            )
        }
        other => bail!("unsupported BMP bit depth {other}"),
    };
    Ok(image)
}

fn wia_error(hr: HRESULT) -> anyhow::Error {
    let message = match hr {
        WIA_ERROR_PAPER_JAM => "Paper jam — clear the feeder and try again",
        WIA_ERROR_PAPER_PROBLEM => "The scanner reported a paper feed problem",
        WIA_ERROR_COVER_OPEN => "The scanner cover is open",
        WIA_ERROR_BUSY => "The scanner is busy",
        WIA_ERROR_OFFLINE => "The scanner is offline or disconnected",
        _ => return anyhow!("WIA scan failed: {}", windows::core::Error::from(hr)),
    };
    anyhow!(message)
}

fn hresult_of(e: &anyhow::Error) -> HRESULT {
    e.downcast_ref::<windows::core::Error>().map_or(WIA_ERROR_OFFLINE, |e| e.code())
}

/// The scanner's flatbed and feeder data items (either may be absent).
#[derive(Default)]
struct DataItems {
    flatbed: Option<IWiaItem2>,
    feeder: Option<IWiaItem2>,
}

impl DataItems {
    unsafe fn find(root: &IWiaItem2) -> Result<Self> {
        let mut items = DataItems::default();
        unsafe {
            let children = root.EnumChildItems(None)?;
            loop {
                let mut slot = [None];
                let mut fetched = 0;
                children.Next(1, slot.as_mut_ptr(), &mut fetched)?;
                let Some(child) = slot[0].take().filter(|_| fetched == 1) else { break };
                let category = child.GetItemCategory().unwrap_or_default();
                if category == WIA_CATEGORY_FLATBED && items.flatbed.is_none() {
                    items.flatbed = Some(child);
                } else if category == WIA_CATEGORY_FEEDER && items.feeder.is_none() {
                    items.feeder = Some(child);
                }
            }
            // Some drivers only categorise via the item property.
            if items.flatbed.is_none() && items.feeder.is_none() {
                let children = root.EnumChildItems(None)?;
                loop {
                    let mut slot = [None];
                    let mut fetched = 0;
                    children.Next(1, slot.as_mut_ptr(), &mut fetched)?;
                    let Some(child) = slot[0].take().filter(|_| fetched == 1) else { break };
                    let flags = child.GetItemType().unwrap_or(0) as u32;
                    if flags & (WiaItemTypeTransfer | WiaItemTypeProgrammableDataSource) != 0 {
                        items.flatbed = Some(child);
                        break;
                    }
                }
            }
        }
        Ok(items)
    }
}

unsafe fn device_manager() -> Result<IWiaDevMgr2> {
    unsafe {
        CoCreateInstance(&WiaDevMgr2, None, CLSCTX_LOCAL_SERVER)
            .context("WIA is not available (is the Windows Image Acquisition service running?)")
    }
}

unsafe fn create_device(id: &str) -> Result<IWiaItem2> {
    unsafe {
        let mgr = device_manager()?;
        mgr.CreateDevice(0, &BSTR::from(id)).with_context(|| format!("could not open WIA device {id}"))
    }
}

/// Initialises COM (MTA) for the current thread and uninitialises on drop.
/// A thread that is already STA keeps its apartment; COM calls still work.
pub(crate) struct ComGuard {
    uninit: bool,
}

impl ComGuard {
    pub(crate) fn init() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Self { uninit: false });
        }
        hr.ok().context("CoInitializeEx failed")?;
        Ok(Self { uninit: true })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            unsafe { CoUninitialize() };
        }
    }
}

// ─── Property storage helpers ─────────────────────────────────────────────

fn propspec(id: u32) -> PROPSPEC {
    PROPSPEC { ulKind: PRSPEC_PROPID, Anonymous: PROPSPEC_0 { propid: id } }
}

/// A PROPVARIANT that WIA allocated, freed on drop.
struct OwnedVariant(PROPVARIANT);

impl OwnedVariant {
    fn vt(&self) -> VARENUM {
        unsafe { self.0.Anonymous.Anonymous.vt }
    }
}

impl Drop for OwnedVariant {
    fn drop(&mut self) {
        unsafe {
            let _ = PropVariantClear(&mut self.0);
        }
    }
}

unsafe fn read_variant(props: &IWiaPropertyStorage, id: u32) -> Option<OwnedVariant> {
    let spec = [propspec(id)];
    let mut var = OwnedVariant(PROPVARIANT::default());
    unsafe { props.ReadMultiple(1, spec.as_ptr(), &mut var.0) }.ok()?;
    Some(var)
}

unsafe fn read_i32(props: &IWiaPropertyStorage, id: u32) -> Option<i32> {
    let var = unsafe { read_variant(props, id)? };
    let inner = unsafe { &var.0.Anonymous.Anonymous.Anonymous };
    match var.vt() {
        VT_I4 => Some(unsafe { inner.lVal }),
        VT_UI4 => Some(unsafe { inner.ulVal } as i32),
        _ => None,
    }
}

unsafe fn read_string(props: &IWiaPropertyStorage, id: u32) -> Option<String> {
    let var = unsafe { read_variant(props, id)? };
    if var.vt() != VT_BSTR {
        return None;
    }
    let s = unsafe { var.0.Anonymous.Anonymous.Anonymous.bstrVal.to_string() };
    Some(s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Valid values of a property: a LIST's entries, a RANGE expanded to the
/// standard dpi stops, or a FLAG's individual bits.
unsafe fn valid_values(props: &IWiaPropertyStorage, id: u32) -> Option<Vec<i32>> {
    let spec = [propspec(id)];
    let mut flags = 0u32;
    let mut var = OwnedVariant(PROPVARIANT::default());
    unsafe { props.GetPropertyAttributes(1, spec.as_ptr(), &mut flags, &mut var.0) }.ok()?;
    let vt = var.vt();
    if vt.0 & VT_VECTOR.0 == 0 {
        return None;
    }
    let elem = VARENUM(vt.0 & !VT_VECTOR.0);
    if elem != VT_I4 && elem != VT_UI4 {
        return None;
    }
    // CAL and CAUL share a layout; read as i32.
    let cal = unsafe { &var.0.Anonymous.Anonymous.Anonymous.cal };
    if cal.pElems.is_null() {
        return None;
    }
    let raw = unsafe { std::slice::from_raw_parts(cal.pElems, cal.cElems as usize) };
    if flags & WIA_PROP_LIST != 0 {
        raw.get(WIA_LIST_VALUES as usize..).map(|v| v.to_vec())
    } else if flags & WIA_PROP_RANGE != 0 {
        let min = *raw.get(WIA_RANGE_MIN as usize)?;
        let max = *raw.get(WIA_RANGE_MAX as usize)?;
        if id == WIA_IPS_XRES || id == WIA_IPS_YRES {
            Some(standard_dpi_in(min.max(0) as u32, max.max(0) as u32).into_iter().map(|d| d as i32).collect())
        } else {
            Some((min..=max).take(4096).collect())
        }
    } else if flags & WIA_PROP_FLAG != 0 {
        let bits = *raw.get(WIA_FLAG_VALUES as usize)? as u32;
        Some((0..32).map(|b| 1u32 << b).filter(|b| bits & b != 0).map(|b| b as i32).collect())
    } else {
        None
    }
}

unsafe fn write_i32(props: &IWiaPropertyStorage, id: u32, value: i32) -> windows::core::Result<()> {
    let spec = [propspec(id)];
    let mut var = PROPVARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_I4;
        inner.Anonymous.lVal = value;
        props.WriteMultiple(1, spec.as_ptr(), &var, WIA_IPA_FIRST)
    }
}

unsafe fn write_guid(props: &IWiaPropertyStorage, id: u32, value: &GUID) -> windows::core::Result<()> {
    let spec = [propspec(id)];
    let mut guid = *value;
    // Points at our stack GUID; WriteMultiple copies it, and this PROPVARIANT
    // must never be PropVariantClear'd (that would CoTaskMemFree the stack).
    let mut var = PROPVARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_CLSID;
        inner.Anonymous.puuid = &mut guid;
        props.WriteMultiple(1, spec.as_ptr(), &var, WIA_IPA_FIRST)
    }
}

// `Device` must be `Send`; holding COM pointers would silently break that.
const _: fn() = || {
    fn is_send<T: Send>() {}
    is_send::<WiaDevice>();
};
