# ScanPro

Scan paper into searchable PDFs (and TIFF, PNG, JPEG or plain text) with any
scanner, on macOS, Windows and Linux.

- **Any scanner.** USB and network scanners through each OS's own scanning
  system: ImageCaptureCore on macOS, WIA on Windows, SANE on Linux (and on
  macOS via Homebrew), plus driverless network scanners (AirScan/eSCL)
  everywhere.
- **Fast double-sided scanning.** Feeder and duplex support. Pages show up as
  they come off the scanner, and the blank backs of single-sided sheets are
  dropped automatically.
- **Searchable PDFs.** OCR adds an invisible text layer, so text can be
  selected, searched and copied in any viewer. It uses the OS's own engine
  (Apple Vision, Windows OCR) when there is one, and the built-in ocrs engine
  otherwise. The original JPEG data goes into the PDF as-is, without
  re-compression.
- **Suggested file names.** "2026-09-14 Northwind Traders Invoice" is built
  from the document's own date, sender and type, all worked out on your
  computer. You can optionally ask a local AI model instead, through Ollama,
  LM Studio or any OpenAI-compatible server:
  - pick the model from a list the server provides, which shows which models
    are loaded and which can see images;
  - ScanPro can load the model for you if it isn't loaded;
  - choose what the model reads: the OCR text (fastest), or a picture of
    page 1 with or without the text, for vision models.
- **Fits how you work.** Named profiles cover device, scan settings, OCR,
  format, output folder and name template. You can:
  - pick a save folder and a naming template (`{type}/{doc_date} {title}`
    files documents into subfolders);
  - save without a dialog, or save as soon as a scan finishes;
  - run a command after each save;
  - do everything from the command line for scripts and automation.
- **Fix mistakes in place.** Preview any page, rotate it, delete it, **drag it
  to reorder**, **rescan just that page**, or **insert a scan after it**.
- **Portrait and landscape in one PDF.** Each page keeps its own orientation:
  switch a page between portrait and landscape with one click. Pages fed
  upside down or sideways are turned upright automatically, based on their
  text. Rotating a page by hand always wins over the automatic turn.
- **Native look.** Fluent styling on Windows and Cupertino on macOS. Both
  follow the system's light/dark setting and use the native menu bar and file
  dialogs.

## Install

Download the installer for your OS from Releases, or build one yourself (see
below):

| OS | Installer | Notes |
|---|---|---|
| macOS 12+ | `ScanPro_<ver>_<arch>.dmg` | Scanners with a macOS driver (Image Capture) work directly. For SANE-only scanners: `brew install sane-backends`. |
| Windows 10/11 | `ScanPro_<ver>_x64-setup.exe` or `.msi` | Uses the scanner's WIA driver. OCR needs a Windows OCR language (Settings → Time & language → Language). |
| Linux | `.deb`, `.AppImage`, pacman | Needs `libsane1` (Debian/Ubuntu) / `sane-backends-libs` (Fedora). For Epson ES/DS models, the `epsonds` backend or Epson's `epsonscan2`. |

## Use

1. Pick your scanner and a profile. Load paper and press **Scan**.
2. Pages appear as they're scanned and OCR runs alongside. Select a page to
   preview, rotate, rescan or delete it; drag pages to reorder them.
3. Check the suggested name, choose a format and press **Save**.

For back-to-back documents, turn on **Output → Save without asking** (and
optionally **Save as soon as a scan finishes**). Every document then goes
straight into the output folder under its template name. The "Batch to folder"
profile is set up this way.

### Name templates

`{suggested}` `{title}` `{type}` `{doc_date}` `{date}` `{time}` `{datetime}`
`{counter}` `{pages}` `{profile}` `{scanner}`. Dates take a strftime format
(`{doc_date:%d.%m.%Y}`), the counter takes a width (`{counter:4}` → `0007`),
and `/` makes subfolders.

### Command line

```sh
scanpro page1.jpg page2.png                      # open the app with these images as pages
scanpro devices                                  # list scanners
scanpro scan                                     # scan with the active profile
scanpro scan -p "Double-sided document" -o ~/Inbox
scanpro scan --source duplex --color gray --dpi 300 -f pdf -n "{doc_date} {suggested}"
scanpro scan --repeat                            # wait for paper, scan, save, repeat (Ctrl+C stops)
scanpro convert page1.jpg page2.jpg -o ~/Docs    # OCR existing images into a searchable PDF
scanpro profiles [--show NAME]                   # list profiles / print one as TOML
scanpro config-path                              # where settings live
```

Settings live in one TOML file (`scanpro config-path`). Profiles made in the
app are available to the CLI by name.

## Build

Requires Rust 1.88+ ([rustup](https://rustup.rs)).

```sh
cargo run                      # run the app
cargo test                     # unit tests
cargo run -- scan --help       # the CLI
```

Installers come from [cargo-packager](https://github.com/crabnebula-dev/cargo-packager)
(`cargo install cargo-packager --locked`). Each OS builds its own:

```sh
scripts/macos/package.sh       # → dist/ScanPro.app, dist/*.dmg
scripts/linux/package.sh       # → dist/*.deb, *.AppImage, *.pkg.tar.zst
scripts/windows/package.ps1    # → dist/*-setup.exe, *.msi
```

Linux builds need `pkg-config libfontconfig1-dev libxkbcommon-dev` (Debian
names). Windows builds need the MSVC toolchain.

## License

GPL-3.0-or-later.
