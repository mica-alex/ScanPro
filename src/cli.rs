//! Headless use: `scanpro scan …` for scripts, hotkeys, cron jobs and
//! scanner-button hooks; `scanpro convert …` to OCR existing images.
//! Every option defaults to the chosen profile, so the GUI's profiles are
//! the single place settings live.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use scanpro::config::{Config, OutputFormat, Profile, SaveMode};
use scanpro::job;
use scanpro::pipeline::{Page, PageStore};
use scanpro::scanner::{self, CancelToken, ColorMode, PaperSize, Source};

#[derive(Parser)]
#[command(name = "scanpro", version, about = "Scan to searchable PDF and more. Run without arguments for the app.")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// List connected scanners.
    Devices,
    /// List profiles (or print one as TOML with --show).
    Profiles {
        #[arg(long)]
        show: Option<String>,
    },
    /// Scan a document and save it.
    Scan(ScanArgs),
    /// Turn existing images into a document (OCR, naming and all).
    Convert {
        /// Image files, in page order.
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Resolution to assume when an image doesn't say.
        #[arg(long, default_value_t = 300)]
        assume_dpi: u32,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Print where the config file lives.
    ConfigPath,
}

#[derive(Args)]
pub struct ScanArgs {
    /// Device id from `scanpro devices` (default: the profile's, then the last used, then the first found).
    #[arg(short, long)]
    device: Option<String>,
    #[arg(long, value_enum)]
    source: Option<SourceArg>,
    #[arg(long, value_enum)]
    color: Option<ColorArg>,
    #[arg(long)]
    dpi: Option<u32>,
    #[arg(long, value_enum)]
    paper: Option<PaperArg>,
    /// Keep blank pages.
    #[arg(long)]
    keep_blank: bool,
    /// After each document, wait for paper and scan the next one, until Ctrl+C.
    #[arg(long)]
    repeat: bool,
    #[command(flatten)]
    out: OutputArgs,
}

#[derive(Args)]
pub struct OutputArgs {
    /// Profile to start from (default: the app's active profile).
    #[arg(short, long)]
    profile: Option<String>,
    #[arg(short, long, value_enum)]
    format: Option<FormatArg>,
    /// Output folder, or a full file path (extension optional).
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// File name template, e.g. "{doc_date} {suggested}".
    #[arg(short, long)]
    name: Option<String>,
    /// Skip OCR (and therefore name suggestions).
    #[arg(long)]
    no_ocr: bool,
    /// OCR languages, e.g. en-US,de-DE.
    #[arg(long, value_delimiter = ',')]
    lang: Vec<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum SourceArg {
    Flatbed,
    Feeder,
    Duplex,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorArg {
    Color,
    Gray,
    Bw,
}

#[derive(Clone, Copy, ValueEnum)]
enum PaperArg {
    Auto,
    Letter,
    Legal,
    A4,
    A5,
    Max,
}

#[derive(Clone, Copy, ValueEnum)]
enum FormatArg {
    Pdf,
    ImagePdf,
    Tiff,
    Png,
    Jpeg,
    Txt,
}

pub fn run(command: Command) -> Result<()> {
    let mut config = Config::load();
    match command {
        Command::Devices => {
            let devices = scanner::discover_all();
            if devices.is_empty() {
                eprintln!("No scanners found.");
            }
            for d in devices {
                println!("{}\t{}\t[{}]", d.id, d.name, d.backend);
            }
        }
        Command::Profiles { show: None } => {
            for p in &config.profiles {
                let mark = if p.name == config.active_profile { "*" } else { " " };
                println!("{mark} {}", p.name);
            }
        }
        Command::Profiles { show: Some(name) } => {
            let p = config.profile(&name).with_context(|| format!("no profile named {name:?}"))?;
            print!("{}", toml::to_string_pretty(p)?);
        }
        Command::ConfigPath => {
            println!("{}", Config::path().map(|p| p.display().to_string()).unwrap_or_default());
        }
        Command::Scan(args) => scan(&mut config, args)?,
        Command::Convert { inputs, assume_dpi, out } => {
            let profile = apply_output(&config, &out)?;
            let store = PageStore::new()?;
            let mut pages = Vec::new();
            for path in &inputs {
                let (page, _) = store
                    .import(path, assume_dpi, &profile.processing)
                    .with_context(|| format!("reading {}", path.display()))?;
                pages.push(page);
            }
            finish(&mut config, &profile, &out, pages, "")?;
        }
    }
    Ok(())
}

fn apply_output(config: &Config, out: &OutputArgs) -> Result<Profile> {
    let mut p = match &out.profile {
        Some(name) => config.profile(name).cloned().with_context(|| format!("no profile named {name:?}"))?,
        None => config.active(),
    };
    if let Some(f) = out.format {
        p.output.format = match f {
            FormatArg::Pdf => OutputFormat::SearchablePdf,
            FormatArg::ImagePdf => OutputFormat::Pdf,
            FormatArg::Tiff => OutputFormat::Tiff,
            FormatArg::Png => OutputFormat::Png,
            FormatArg::Jpeg => OutputFormat::Jpeg,
            FormatArg::Txt => OutputFormat::Text,
        };
    }
    if let Some(n) = &out.name {
        p.output.name_template = n.clone();
    }
    if out.no_ocr {
        p.ocr.enabled = false;
        if p.output.format == OutputFormat::SearchablePdf {
            p.output.format = OutputFormat::Pdf;
        }
    }
    if !out.lang.is_empty() {
        p.ocr.languages = out.lang.clone();
    }
    // The CLI never shows dialogs.
    p.output.save_mode = SaveMode::Auto;
    Ok(p)
}

fn scan(config: &mut Config, args: ScanArgs) -> Result<()> {
    let mut profile = apply_output(config, &args.out)?;
    let s = &mut profile.scan;
    if let Some(v) = args.source {
        s.source = match v {
            SourceArg::Flatbed => Source::Flatbed,
            SourceArg::Feeder => Source::Feeder,
            SourceArg::Duplex => Source::FeederDuplex,
        };
    }
    if let Some(v) = args.color {
        s.color = match v {
            ColorArg::Color => ColorMode::Color,
            ColorArg::Gray => ColorMode::Gray,
            ColorArg::Bw => ColorMode::BlackWhite,
        };
    }
    if let Some(v) = args.dpi {
        s.dpi = v;
    }
    if let Some(v) = args.paper {
        s.paper = match v {
            PaperArg::Auto => PaperSize::Auto,
            PaperArg::Letter => PaperSize::Letter,
            PaperArg::Legal => PaperSize::Legal,
            PaperArg::A4 => PaperSize::A4,
            PaperArg::A5 => PaperSize::A5,
            PaperArg::Max => PaperSize::Maximum,
        };
    }
    if args.keep_blank {
        profile.processing.skip_blank = false;
    }

    let device_id = match args.device.clone().or(profile.device.clone()).or(config.last_device.clone()) {
        Some(id) => id,
        None => scanner::discover_all().into_iter().next().map(|d| d.id).context("no scanners found")?,
    };
    eprintln!("Using {device_id}");
    let mut device = scanner::open(&device_id)?;
    let device_name = device.info().name.clone();

    // Ctrl+C cancels the scan so the device gets closed properly — killing
    // the process mid-scan can leave some scanners (the ES-300W) locked until
    // they're power-cycled. A second Ctrl+C exits immediately.
    let cancel = CancelToken::default();
    let on_interrupt = cancel.clone();
    ctrlc::set_handler(move || {
        if on_interrupt.is_cancelled() {
            std::process::exit(130);
        }
        eprintln!("Stopping… (Ctrl+C again to force quit)");
        on_interrupt.cancel();
    })?;

    loop {
        let store = PageStore::new()?;
        let mut pages: Vec<Page> = Vec::new();
        let mut skipped = 0;
        let trim = profile.scan.paper == PaperSize::Auto && profile.scan.source.is_feeder();
        let result = device.scan(&profile.scan, &cancel, &mut |img| {
            let img = if trim { scanpro::pipeline::trim_feeder_length(img) } else { img };
            let (page, _) = store.ingest(img, &profile.processing)?;
            if profile.processing.skip_blank && page.blank {
                store.remove(&page);
                skipped += 1;
            } else {
                pages.push(page);
                eprintln!("  page {}", pages.len());
            }
            Ok(())
        });
        if cancel.is_cancelled() {
            eprintln!("Stopped; {} scanned page(s) discarded", pages.len());
            break;
        }
        match result {
            Ok(_) => {}
            // In repeat mode an empty feeder just means "not yet".
            Err(e) if args.repeat && e.to_string().contains("Load paper") => {
                std::thread::sleep(std::time::Duration::from_secs(2));
                continue;
            }
            Err(e) => return Err(e),
        }
        if skipped > 0 {
            eprintln!("  skipped {skipped} blank page(s)");
        }
        if pages.is_empty() {
            if args.repeat {
                continue;
            }
            bail!("every page was blank — nothing to save");
        }
        finish(config, &profile, &args.out, pages, &device_name)?;
        if !args.repeat {
            break;
        }
        eprintln!("Waiting for the next document… (Ctrl+C to stop)");
    }
    Ok(())
}

fn finish(config: &mut Config, profile: &Profile, out: &OutputArgs, mut pages: Vec<Page>, scanner: &str) -> Result<()> {
    if profile.ocr.enabled && (profile.output.format.needs_ocr() || profile.naming.suggest) {
        eprintln!("Recognizing text…");
        job::ensure_ocr(&mut pages, &profile.ocr)?;
    }
    let suggestion = job::suggest(&pages, profile);
    let name =
        job::file_name(&profile.output, &profile.name, scanner, suggestion.as_ref(), config.counter, pages.len());
    let dest = match &out.output {
        // An explicit file path is used as-is (the extension is replaced).
        Some(p) if p.extension().is_some() && !p.is_dir() => p.clone(),
        Some(folder) => job::destination(folder, &name, &profile.output),
        None => job::destination(&profile.output.resolved_folder(), &name, &profile.output),
    };
    let title = suggestion.as_ref().map(|s| s.name.clone());
    let saved = job::save(&pages, profile, &dest, title)?;
    config.counter += 1;
    let _ = config.save();
    for w in saved.warnings {
        eprintln!("warning: {w}");
    }
    for f in saved.files {
        println!("{}", f.display());
    }
    Ok(())
}
