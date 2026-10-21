//! eSCL ("AirScan") backend: network scanners found over mDNS and driven over
//! plain HTTP(S) + XML. Works the same on every OS and needs no driver.
//!
//! The flow is the one every eSCL client uses:
//!
//! ```text
//! GET  {base}/ScannerCapabilities      → what to offer in the UI
//! POST {base}/ScanJobs  (ScanSettings) → 201 + Location: {job}
//! GET  {job}/NextDocument              → 200 page | 503 not ready yet | 404 no more pages
//! DELETE {job}                         → cancel
//! ```
//!
//! A device id is the base URL, e.g. `http://192.168.1.20:80/eSCL`.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use ureq::Agent;

use super::{Backend, CancelToken, Capabilities, ColorMode, Device, DeviceInfo, ScanSettings, ScannedImage, Source};

const SERVICE_TYPES: [&str; 2] = ["_uscan._tcp.local.", "_uscans._tcp.local."];
const DISCOVERY_TIME: Duration = Duration::from_millis(2500);
/// How long to keep polling `NextDocument` while the scanner answers 503
/// ("page not ready"). Generous: a 600 dpi colour page can take a while.
const NOT_READY_LIMIT: Duration = Duration::from_secs(120);
/// Scanned pages are big; ureq's default 10 MB body cap is far too small.
const MAX_PAGE_BYTES: u64 = 512 * 1024 * 1024;

pub struct EsclBackend;

impl EsclBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EsclBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for EsclBackend {
    fn id(&self) -> &'static str {
        "escl"
    }

    fn available(&self) -> bool {
        true
    }

    fn discover(&self) -> Result<Vec<DeviceInfo>> {
        let daemon = ServiceDaemon::new().context("starting mDNS")?;
        let receivers = SERVICE_TYPES
            .iter()
            .map(|ty| daemon.browse(ty))
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("browsing for eSCL scanners")?;

        let deadline = Instant::now() + DISCOVERY_TIME;
        let mut found = Vec::new();
        // A scanner usually advertises both _uscan and _uscans; keep the first.
        let mut seen = HashSet::new();
        while Instant::now() < deadline {
            for rx in &receivers {
                while let Ok(event) = rx.try_recv() {
                    let ServiceEvent::ServiceResolved(svc) = event else { continue };
                    let Some(url) = service_url(&svc) else { continue };
                    let key =
                        svc.get_property_val_str("UUID").map(|u| u.to_ascii_lowercase()).unwrap_or_else(|| url.clone());
                    if !seen.insert(key) {
                        continue;
                    }
                    let name =
                        svc.get_property_val_str("ty").filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(
                            || svc.get_fullname().split('.').next().unwrap_or("Network scanner").to_string(),
                        );
                    found.push(DeviceInfo { id: format!("escl:{url}"), name, backend: "eSCL".into() });
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for ty in SERVICE_TYPES {
            let _ = daemon.stop_browse(ty);
        }
        let _ = daemon.shutdown();
        Ok(found)
    }

    fn open(&self, id: &str) -> Result<Box<dyn Device>> {
        if !(id.starts_with("http://") || id.starts_with("https://")) {
            bail!("eSCL device id must be a URL, got {id:?}");
        }
        let base = id.trim_end_matches('/').to_string();
        let info = DeviceInfo { id: format!("escl:{base}"), name: base.clone(), backend: "eSCL".into() };
        let mut device = EsclDevice { info, base, agent: agent(), caps: None };
        // Fetch capabilities now so the device gets its real name and a dead
        // address fails at open, not mid-scan.
        let caps = device.fetch_caps()?;
        if let Some(name) = caps.make_and_model.clone() {
            device.info.name = name;
        }
        device.caps = Some(caps);
        Ok(Box::new(device))
    }
}

/// Builds `scheme://host:port/rs` from a resolved service. Prefers an IPv4
/// literal: `.local` hostnames do not resolve everywhere (Windows without
/// Bonjour, Linux without nss-mdns).
fn service_url(svc: &mdns_sd::ResolvedService) -> Option<String> {
    let scheme = if svc.ty_domain.starts_with("_uscans") { "https" } else { "http" };
    let host = if let Some(v4) = svc.get_addresses_v4().into_iter().min() {
        v4.to_string()
    } else if let Some(ip) = svc.get_addresses().iter().next() {
        match ip.to_ip_addr() {
            std::net::IpAddr::V6(v6) => format!("[{v6}]"),
            std::net::IpAddr::V4(v4) => v4.to_string(),
        }
    } else {
        let h = svc.get_hostname().trim_end_matches('.');
        if h.is_empty() {
            return None;
        }
        h.to_string()
    };
    let rs = svc.get_property_val_str("rs").unwrap_or("eSCL").trim_matches('/');
    let path = if rs.is_empty() { String::new() } else { format!("/{rs}") };
    Some(format!("{scheme}://{host}:{}{path}", svc.get_port()))
}

/// One HTTP agent per device. eSCL scanners serve HTTPS with self-signed
/// certificates, so verification is off — for these connections only.
fn agent() -> Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_recv_response(Some(Duration::from_secs(120)))
        .timeout_recv_body(Some(Duration::from_secs(300)))
        .tls_config(ureq::tls::TlsConfig::builder().disable_verification(true).build())
        .build()
        .into()
}

struct EsclDevice {
    info: DeviceInfo,
    base: String,
    agent: Agent,
    caps: Option<EsclCaps>,
}

impl EsclDevice {
    fn fetch_caps(&self) -> Result<EsclCaps> {
        let url = format!("{}/ScannerCapabilities", self.base);
        let mut resp = self.agent.get(&url).call().with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            bail!("scanner refused capabilities request: HTTP {}", resp.status());
        }
        let xml = resp.body_mut().read_to_string()?;
        parse_capabilities(&xml)
    }

    fn adf_empty(&self) -> bool {
        let url = format!("{}/ScannerStatus", self.base);
        let Ok(mut resp) = self.agent.get(&url).call() else { return false };
        let Ok(xml) = resp.body_mut().read_to_string() else { return false };
        parse_adf_state(&xml).is_some_and(|s| s == "ScannerAdfEmpty")
    }

    /// Resolves a `Location` header, which may be absolute, host-relative or
    /// relative to the eSCL root depending on the firmware.
    fn resolve(&self, location: &str) -> String {
        if location.starts_with("http://") || location.starts_with("https://") {
            return location.trim_end_matches('/').to_string();
        }
        if location.starts_with('/') {
            let after_scheme = self.base.find("://").map(|i| i + 3).unwrap_or(0);
            let origin_end = self.base[after_scheme..].find('/').map(|i| i + after_scheme).unwrap_or(self.base.len());
            return format!("{}{}", &self.base[..origin_end], location.trim_end_matches('/'));
        }
        format!("{}/{}", self.base, location.trim_end_matches('/'))
    }

    fn delete_job(&self, job: &str) {
        if let Err(e) = self.agent.delete(job).call() {
            log::warn!("eSCL: cancelling {job} failed: {e}");
        }
    }
}

impl Device for EsclDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&mut self) -> Result<Capabilities> {
        if self.caps.is_none() {
            self.caps = Some(self.fetch_caps()?);
        }
        Ok(self.caps.as_ref().unwrap().to_capabilities())
    }

    fn scan(
        &mut self,
        settings: &ScanSettings,
        cancel: &CancelToken,
        on_page: &mut dyn FnMut(ScannedImage) -> Result<()>,
    ) -> Result<usize> {
        if self.caps.is_none() {
            self.caps = Some(self.fetch_caps()?);
        }
        let job_settings = self.caps.as_ref().unwrap().resolve(settings)?;
        let body = scan_settings_xml(&job_settings);

        let url = format!("{}/ScanJobs", self.base);
        let resp = self
            .agent
            .post(&url)
            .content_type("text/xml")
            .send(body.as_str())
            .with_context(|| format!("POST {url}"))?;
        match resp.status().as_u16() {
            201 => {}
            409 | 503 if settings.source.is_feeder() && self.adf_empty() => {
                bail!("Load paper into the feeder")
            }
            503 => bail!("The scanner is busy"),
            code => bail!("scanner rejected the scan job: HTTP {code}"),
        }
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("scanner accepted the job but gave no job URL"))?;
        let job = self.resolve(location);
        log::info!("eSCL job {job}");

        let mut pages = 0usize;
        let mut not_ready_since: Option<Instant> = None;
        loop {
            if cancel.is_cancelled() {
                self.delete_job(&job);
                return Ok(pages);
            }
            let next = format!("{job}/NextDocument");
            let mut resp = match self.agent.get(&next).call() {
                Ok(r) => r,
                Err(e) => {
                    self.delete_job(&job);
                    return Err(anyhow!(e).context(format!("GET {next}")));
                }
            };
            match resp.status().as_u16() {
                200 => {
                    not_ready_since = None;
                    let content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    let bytes = resp.body_mut().with_config().limit(MAX_PAGE_BYTES).read_to_vec()?;
                    let format = format_from_mime(&content_type)
                        .or_else(|| image::guess_format(&bytes).ok())
                        .ok_or_else(|| anyhow!("scanner sent an unrecognised page format {content_type:?}"))?;
                    pages += 1;
                    if let Err(e) = on_page(ScannedImage::Encoded { bytes, format, dpi: job_settings.dpi }) {
                        self.delete_job(&job);
                        return Err(e);
                    }
                    if settings.source == Source::Flatbed {
                        // One page per flatbed job; some firmwares never send the 404.
                        self.delete_job(&job);
                        return Ok(pages);
                    }
                }
                404 | 410 => {
                    if pages == 0 && settings.source.is_feeder() && self.adf_empty() {
                        bail!("Load paper into the feeder");
                    }
                    return Ok(pages);
                }
                503 => {
                    let since = *not_ready_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > NOT_READY_LIMIT {
                        self.delete_job(&job);
                        bail!("the scanner stopped responding");
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                code => {
                    self.delete_job(&job);
                    bail!("scanner error while fetching page {}: HTTP {code}", pages + 1);
                }
            }
        }
    }
}

fn format_from_mime(mime: &str) -> Option<image::ImageFormat> {
    let mime = mime.split(';').next().unwrap_or("").trim();
    match mime {
        "image/jpeg" | "image/jpg" => Some(image::ImageFormat::Jpeg),
        "image/png" => Some(image::ImageFormat::Png),
        "image/tiff" => Some(image::ImageFormat::Tiff),
        _ => None,
    }
}

// ─── Capabilities ──────────────────────────────────────────────────────────

/// Caps of one input source (platen, ADF simplex, ADF duplex).
#[derive(Clone, Debug, Default, PartialEq)]
struct InputCaps {
    /// Maximum scan area in 1/300 inch.
    max_width: u32,
    max_height: u32,
    /// eSCL names: `RGB24`, `Grayscale8`, `BlackAndWhite1`, …
    color_modes: Vec<String>,
    /// Ascending, deduplicated.
    resolutions: Vec<u32>,
    /// MIME types.
    formats: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct EsclCaps {
    make_and_model: Option<String>,
    platen: Option<InputCaps>,
    adf_simplex: Option<InputCaps>,
    /// Some firmwares only flag `<AdfOption>Duplex</AdfOption>` and reuse the
    /// simplex caps; [`parse_capabilities`] fills this in from those.
    adf_duplex: Option<InputCaps>,
}

/// Settings after matching the request against what the device offers.
#[derive(Clone, Debug, PartialEq)]
struct JobSettings {
    input_source: &'static str,
    duplex: bool,
    color_mode: String,
    dpi: u32,
    /// 1/300 inch.
    width: u32,
    height: u32,
    format: String,
}

fn local<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    node.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn descendants<'a>(node: roxmltree::Node<'a, 'a>, name: &'a str) -> impl Iterator<Item = roxmltree::Node<'a, 'a>> + 'a {
    node.descendants().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn text_u32(node: Option<roxmltree::Node>) -> Option<u32> {
    node.and_then(|n| n.text()).and_then(|t| t.trim().parse().ok())
}

fn parse_input_caps(node: roxmltree::Node) -> InputCaps {
    let mut caps = InputCaps {
        max_width: text_u32(local(node, "MaxWidth")).unwrap_or(2550),
        max_height: text_u32(local(node, "MaxHeight")).unwrap_or(3300),
        ..Default::default()
    };
    for mode in descendants(node, "ColorMode") {
        if let Some(t) = mode.text().map(str::trim)
            && !caps.color_modes.iter().any(|m| m == t)
        {
            caps.color_modes.push(t.to_string());
        }
    }
    for res in descendants(node, "DiscreteResolution") {
        if let Some(x) = text_u32(local(res, "XResolution")) {
            caps.resolutions.push(x);
        }
    }
    for range in descendants(node, "XResolutionRange") {
        if let (Some(min), Some(max)) = (text_u32(local(range, "Min")), text_u32(local(range, "Max"))) {
            caps.resolutions.extend(super::standard_dpi_in(min, max));
        }
    }
    caps.resolutions.sort_unstable();
    caps.resolutions.dedup();
    // Both `DocumentFormat` and `DocumentFormatExt` list MIME types.
    for fmt in node
        .descendants()
        .filter(|c| c.is_element() && matches!(c.tag_name().name(), "DocumentFormat" | "DocumentFormatExt"))
    {
        if let Some(t) = fmt.text().map(str::trim)
            && !caps.formats.iter().any(|f| f == t)
        {
            caps.formats.push(t.to_string());
        }
    }
    caps
}

fn parse_capabilities(xml: &str) -> Result<EsclCaps> {
    let doc = roxmltree::Document::parse(xml).context("parsing ScannerCapabilities")?;
    let root = doc.root_element();
    if root.tag_name().name() != "ScannerCapabilities" {
        bail!("not an eSCL ScannerCapabilities document");
    }
    let mut caps = EsclCaps {
        make_and_model: descendants(root, "MakeAndModel")
            .next()
            .and_then(|n| n.text())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
        platen: descendants(root, "PlatenInputCaps").next().map(parse_input_caps),
        adf_simplex: descendants(root, "AdfSimplexInputCaps").next().map(parse_input_caps),
        adf_duplex: descendants(root, "AdfDuplexInputCaps").next().map(parse_input_caps),
    };
    let duplex_option = descendants(root, "AdfOption").any(|n| n.text().map(str::trim) == Some("Duplex"));
    if caps.adf_duplex.is_none() && duplex_option {
        caps.adf_duplex = caps.adf_simplex.clone();
    }
    Ok(caps)
}

fn parse_adf_state(xml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    descendants(doc.root_element(), "AdfState").next()?.text().map(|t| t.trim().to_string())
}

fn escl_color_name(mode: ColorMode) -> &'static str {
    match mode {
        ColorMode::Color => "RGB24",
        ColorMode::Gray => "Grayscale8",
        ColorMode::BlackWhite => "BlackAndWhite1",
    }
}

impl EsclCaps {
    fn input(&self, source: Source) -> Option<&InputCaps> {
        match source {
            Source::Flatbed => self.platen.as_ref(),
            Source::Feeder => self.adf_simplex.as_ref(),
            Source::FeederDuplex => self.adf_duplex.as_ref(),
        }
    }

    fn to_capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        for source in Source::ALL {
            if self.input(source).is_some() {
                caps.sources.push(source);
            }
        }
        let inputs = [&self.platen, &self.adf_simplex, &self.adf_duplex];
        for mode in ColorMode::ALL {
            let name = escl_color_name(mode);
            if inputs.iter().flat_map(|i| i.iter()).any(|i| i.color_modes.iter().any(|m| m == name)) {
                caps.color_modes.push(mode);
            }
        }
        let mut dpi: Vec<u32> =
            inputs.iter().flat_map(|i| i.iter()).flat_map(|i| i.resolutions.iter().copied()).collect();
        dpi.sort_unstable();
        dpi.dedup();
        caps.resolutions = dpi;
        caps
    }

    /// Matches a request to the device: nearest supported resolution not
    /// above the request, the closest colour mode it has, a page size clamped
    /// to the scan area.
    fn resolve(&self, settings: &ScanSettings) -> Result<JobSettings> {
        let input = self
            .input(settings.source)
            .ok_or_else(|| anyhow!("this scanner has no {}", settings.source.label().to_lowercase()))?;

        let dpi = if input.resolutions.is_empty() {
            settings.dpi
        } else {
            input.resolutions.iter().copied().filter(|&r| r <= settings.dpi).max().unwrap_or(input.resolutions[0])
        };

        let wanted = escl_color_name(settings.color);
        let fallbacks: &[&str] = match settings.color {
            ColorMode::BlackWhite => &["BlackAndWhite1", "Grayscale8", "RGB24"],
            ColorMode::Gray => &["Grayscale8", "RGB24", "BlackAndWhite1"],
            ColorMode::Color => &["RGB24", "Grayscale8", "BlackAndWhite1"],
        };
        let color_mode = if input.color_modes.is_empty() {
            wanted.to_string()
        } else {
            fallbacks
                .iter()
                .find(|m| input.color_modes.iter().any(|c| c == *m))
                .map(|m| m.to_string())
                .unwrap_or_else(|| input.color_modes[0].clone())
        };

        let offers = |m: &str| input.formats.iter().any(|f| f == m);
        let format = if color_mode == "BlackAndWhite1" && offers("image/png") {
            "image/png"
        } else if offers("image/jpeg") || input.formats.is_empty() {
            "image/jpeg"
        } else if offers("image/png") {
            "image/png"
        } else {
            input.formats.iter().find(|f| f.starts_with("image/")).map(String::as_str).unwrap_or("image/jpeg")
        }
        .to_string();

        let (width, height) = match settings.paper.size_mm() {
            Some((w, h)) => (mm_to_300ths(w).min(input.max_width), mm_to_300ths(h).min(input.max_height)),
            // Auto and Maximum: eSCL has no portable auto-crop, so full area.
            None => (input.max_width, input.max_height),
        };

        Ok(JobSettings {
            input_source: if settings.source.is_feeder() { "Feeder" } else { "Platen" },
            duplex: settings.source == Source::FeederDuplex,
            color_mode,
            dpi,
            width,
            height,
            format,
        })
    }
}

fn mm_to_300ths(mm: f64) -> u32 {
    (mm / 25.4 * 300.0).round() as u32
}

fn scan_settings_xml(s: &JobSettings) -> String {
    let duplex =
        if s.input_source == "Feeder" { format!("\n  <scan:Duplex>{}</scan:Duplex>", s.duplex) } else { String::new() };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScanSettings xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03" xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm">
  <pwg:Version>2.6</pwg:Version>
  <scan:Intent>Document</scan:Intent>
  <pwg:ScanRegions pwg:MustHonor="false">
    <pwg:ScanRegion>
      <pwg:ContentRegionUnits>escl:ThreeHundredthsOfInches</pwg:ContentRegionUnits>
      <pwg:XOffset>0</pwg:XOffset>
      <pwg:YOffset>0</pwg:YOffset>
      <pwg:Width>{width}</pwg:Width>
      <pwg:Height>{height}</pwg:Height>
    </pwg:ScanRegion>
  </pwg:ScanRegions>
  <pwg:InputSource>{source}</pwg:InputSource>{duplex}
  <scan:ColorMode>{color}</scan:ColorMode>
  <scan:XResolution>{dpi}</scan:XResolution>
  <scan:YResolution>{dpi}</scan:YResolution>
  <pwg:DocumentFormat>{format}</pwg:DocumentFormat>
  <scan:DocumentFormatExt>{format}</scan:DocumentFormatExt>
</scan:ScanSettings>
"#,
        width = s.width,
        height = s.height,
        source = s.input_source,
        color = s.color_mode,
        dpi = s.dpi,
        format = s.format,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::PaperSize;

    /// Shape of an HP OfficeJet/LaserJet: discrete resolutions, ADF with
    /// separate duplex caps.
    const HP_CAPS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScannerCapabilities xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm" xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03">
  <pwg:Version>2.63</pwg:Version>
  <pwg:MakeAndModel>HP OfficeJet Pro 9010 series</pwg:MakeAndModel>
  <scan:Platen>
    <scan:PlatenInputCaps>
      <scan:MinWidth>8</scan:MinWidth><scan:MaxWidth>2550</scan:MaxWidth>
      <scan:MinHeight>8</scan:MinHeight><scan:MaxHeight>3508</scan:MaxHeight>
      <scan:SettingProfiles><scan:SettingProfile>
        <scan:ColorModes>
          <scan:ColorMode>BlackAndWhite1</scan:ColorMode>
          <scan:ColorMode>Grayscale8</scan:ColorMode>
          <scan:ColorMode>RGB24</scan:ColorMode>
        </scan:ColorModes>
        <scan:DocumentFormats>
          <pwg:DocumentFormat>application/pdf</pwg:DocumentFormat>
          <pwg:DocumentFormat>image/jpeg</pwg:DocumentFormat>
          <scan:DocumentFormatExt>application/pdf</scan:DocumentFormatExt>
          <scan:DocumentFormatExt>image/jpeg</scan:DocumentFormatExt>
        </scan:DocumentFormats>
        <scan:SupportedResolutions><scan:DiscreteResolutions>
          <scan:DiscreteResolution><scan:XResolution>75</scan:XResolution><scan:YResolution>75</scan:YResolution></scan:DiscreteResolution>
          <scan:DiscreteResolution><scan:XResolution>200</scan:XResolution><scan:YResolution>200</scan:YResolution></scan:DiscreteResolution>
          <scan:DiscreteResolution><scan:XResolution>300</scan:XResolution><scan:YResolution>300</scan:YResolution></scan:DiscreteResolution>
          <scan:DiscreteResolution><scan:XResolution>600</scan:XResolution><scan:YResolution>600</scan:YResolution></scan:DiscreteResolution>
        </scan:DiscreteResolutions></scan:SupportedResolutions>
      </scan:SettingProfile></scan:SettingProfiles>
    </scan:PlatenInputCaps>
  </scan:Platen>
  <scan:Adf>
    <scan:AdfSimplexInputCaps>
      <scan:MaxWidth>2550</scan:MaxWidth><scan:MaxHeight>4200</scan:MaxHeight>
      <scan:SettingProfiles><scan:SettingProfile>
        <scan:ColorModes><scan:ColorMode>Grayscale8</scan:ColorMode><scan:ColorMode>RGB24</scan:ColorMode></scan:ColorModes>
        <scan:DocumentFormats><pwg:DocumentFormat>image/jpeg</pwg:DocumentFormat></scan:DocumentFormats>
        <scan:SupportedResolutions><scan:DiscreteResolutions>
          <scan:DiscreteResolution><scan:XResolution>200</scan:XResolution><scan:YResolution>200</scan:YResolution></scan:DiscreteResolution>
          <scan:DiscreteResolution><scan:XResolution>300</scan:XResolution><scan:YResolution>300</scan:YResolution></scan:DiscreteResolution>
        </scan:DiscreteResolutions></scan:SupportedResolutions>
      </scan:SettingProfile></scan:SettingProfiles>
    </scan:AdfSimplexInputCaps>
    <scan:AdfDuplexInputCaps>
      <scan:MaxWidth>2550</scan:MaxWidth><scan:MaxHeight>3300</scan:MaxHeight>
      <scan:SettingProfiles><scan:SettingProfile>
        <scan:ColorModes><scan:ColorMode>RGB24</scan:ColorMode></scan:ColorModes>
        <scan:SupportedResolutions><scan:DiscreteResolutions>
          <scan:DiscreteResolution><scan:XResolution>300</scan:XResolution><scan:YResolution>300</scan:YResolution></scan:DiscreteResolution>
        </scan:DiscreteResolutions></scan:SupportedResolutions>
      </scan:SettingProfile></scan:SettingProfiles>
    </scan:AdfDuplexInputCaps>
    <scan:FeederCapacity>35</scan:FeederCapacity>
  </scan:Adf>
</scan:ScannerCapabilities>"#;

    /// Shape of an Epson: resolution *range*, ADF duplex only as an option,
    /// PNG offered.
    const EPSON_CAPS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScannerCapabilities xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm" xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03">
  <pwg:Version>2.6</pwg:Version>
  <pwg:MakeAndModel>EPSON ET-4850 Series</pwg:MakeAndModel>
  <scan:Adf>
    <scan:AdfSimplexInputCaps>
      <scan:MaxWidth>2550</scan:MaxWidth><scan:MaxHeight>4200</scan:MaxHeight>
      <scan:SettingProfiles><scan:SettingProfile>
        <scan:ColorModes>
          <scan:ColorMode>RGB24</scan:ColorMode><scan:ColorMode>Grayscale8</scan:ColorMode><scan:ColorMode>BlackAndWhite1</scan:ColorMode>
        </scan:ColorModes>
        <scan:DocumentFormats>
          <pwg:DocumentFormat>image/jpeg</pwg:DocumentFormat><pwg:DocumentFormat>image/png</pwg:DocumentFormat>
        </scan:DocumentFormats>
        <scan:SupportedResolutions><scan:ResolutionRange>
          <scan:XResolutionRange><scan:Min>50</scan:Min><scan:Max>600</scan:Max><scan:Step>1</scan:Step></scan:XResolutionRange>
          <scan:YResolutionRange><scan:Min>50</scan:Min><scan:Max>600</scan:Max><scan:Step>1</scan:Step></scan:YResolutionRange>
        </scan:ResolutionRange></scan:SupportedResolutions>
      </scan:SettingProfile></scan:SettingProfiles>
    </scan:AdfSimplexInputCaps>
    <scan:AdfOptions><scan:AdfOption>DetectPaperLoaded</scan:AdfOption><scan:AdfOption>Duplex</scan:AdfOption></scan:AdfOptions>
  </scan:Adf>
</scan:ScannerCapabilities>"#;

    #[test]
    fn parses_hp_capabilities() {
        let caps = parse_capabilities(HP_CAPS).unwrap();
        assert_eq!(caps.make_and_model.as_deref(), Some("HP OfficeJet Pro 9010 series"));
        let platen = caps.platen.as_ref().unwrap();
        assert_eq!(platen.max_height, 3508);
        assert_eq!(platen.resolutions, vec![75, 200, 300, 600]);
        assert_eq!(platen.formats, vec!["application/pdf", "image/jpeg"]);
        assert_eq!(caps.adf_duplex.as_ref().unwrap().resolutions, vec![300]);

        let generic = caps.to_capabilities();
        assert_eq!(generic.sources, vec![Source::Flatbed, Source::Feeder, Source::FeederDuplex]);
        assert_eq!(generic.color_modes, ColorMode::ALL.to_vec());
        assert_eq!(generic.resolutions, vec![75, 200, 300, 600]);
    }

    #[test]
    fn parses_epson_range_and_duplex_option() {
        let caps = parse_capabilities(EPSON_CAPS).unwrap();
        assert!(caps.platen.is_none());
        let adf = caps.adf_simplex.as_ref().unwrap();
        assert_eq!(adf.resolutions, vec![75, 100, 150, 200, 300, 400, 600]);
        assert_eq!(caps.adf_duplex.as_ref(), Some(adf), "duplex falls back to simplex caps");
        assert_eq!(caps.to_capabilities().sources, vec![Source::Feeder, Source::FeederDuplex]);
    }

    #[test]
    fn resolves_settings_against_caps() {
        let caps = parse_capabilities(HP_CAPS).unwrap();
        // Duplex only does RGB24 @ 300 here.
        let job = caps
            .resolve(&ScanSettings {
                source: Source::FeederDuplex,
                color: ColorMode::BlackWhite,
                dpi: 600,
                paper: PaperSize::Legal,
            })
            .unwrap();
        assert_eq!(job.dpi, 300);
        assert_eq!(job.color_mode, "RGB24");
        assert_eq!(job.format, "image/jpeg");
        assert_eq!((job.width, job.height), (2550, 3300), "legal clamps to the duplex max height");
        assert!(job.duplex);

        let caps = parse_capabilities(EPSON_CAPS).unwrap();
        let job = caps.resolve(&ScanSettings { color: ColorMode::BlackWhite, dpi: 250, ..Default::default() }).unwrap();
        assert_eq!(job.dpi, 200);
        assert_eq!(job.format, "image/png");
        assert!(caps.resolve(&ScanSettings { source: Source::Flatbed, ..Default::default() }).is_err());
    }

    #[test]
    fn builds_scan_settings_xml() {
        let job = JobSettings {
            input_source: "Feeder",
            duplex: true,
            color_mode: "Grayscale8".into(),
            dpi: 300,
            width: 2480,
            height: 3508,
            format: "image/jpeg".into(),
        };
        let xml = scan_settings_xml(&job);
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let root = doc.root_element();
        let get = |n: &str| descendants(root, n).next().and_then(|n| n.text()).unwrap().to_string();
        assert_eq!(root.tag_name().name(), "ScanSettings");
        assert_eq!(get("InputSource"), "Feeder");
        assert_eq!(get("Duplex"), "true");
        assert_eq!(get("ColorMode"), "Grayscale8");
        assert_eq!(get("XResolution"), "300");
        assert_eq!(get("Width"), "2480");
        assert_eq!(get("Height"), "3508");
        assert_eq!(get("DocumentFormatExt"), "image/jpeg");

        let platen = scan_settings_xml(&JobSettings { input_source: "Platen", duplex: false, ..job });
        assert!(!platen.contains("Duplex"), "platen jobs must not carry a Duplex element");
    }

    #[test]
    fn parses_adf_state_and_mime() {
        let status = r#"<?xml version="1.0"?><scan:ScannerStatus xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm" xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03"><pwg:State>Idle</pwg:State><scan:AdfState>ScannerAdfEmpty</scan:AdfState></scan:ScannerStatus>"#;
        assert_eq!(parse_adf_state(status).as_deref(), Some("ScannerAdfEmpty"));
        assert_eq!(format_from_mime("image/jpeg; charset=binary"), Some(image::ImageFormat::Jpeg));
        assert_eq!(format_from_mime("application/pdf"), None);
    }

    #[test]
    fn resolves_job_locations() {
        let dev = EsclDevice {
            info: DeviceInfo { id: String::new(), name: String::new(), backend: String::new() },
            base: "http://10.0.0.5:8080/eSCL".into(),
            agent: agent(),
            caps: None,
        };
        assert_eq!(dev.resolve("/eSCL/ScanJobs/42/"), "http://10.0.0.5:8080/eSCL/ScanJobs/42");
        assert_eq!(dev.resolve("ScanJobs/42"), "http://10.0.0.5:8080/eSCL/ScanJobs/42");
        assert_eq!(dev.resolve("https://10.0.0.5/eSCL/ScanJobs/42"), "https://10.0.0.5/eSCL/ScanJobs/42");
    }

    /// Browses the real network; prints whatever eSCL scanners answer.
    #[test]
    #[ignore = "uses the local network"]
    fn discovers_live() {
        let found = EsclBackend::new().discover().unwrap();
        eprintln!("eSCL devices: {found:#?}");
    }
}
