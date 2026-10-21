# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Purpose

**ScanPro** is a cross-platform (macOS / Windows / Linux) desktop scanning
app in Rust with a Slint UI. It drives scanners through each platform's
scanning API, OCRs the pages, suggests a file name from the text, and saves a
searchable PDF (or TIFF/PNG/JPEG/text). It ships as native installers. There
is also a CLI for automation (`scanpro scan …`).

## Git identity

This repo commits as the **`mica-alex`** identity, the machine's global
default, so there is no local override:

```
user.name  = mica-alex
user.email = 83238954+mica-alex@users.noreply.github.com
```

It's hosted at `github.com/mica-alex/ScanPro` (HTTPS remote, like the user's
other repos). Check `git config user.email` before committing.

## Commands

```sh
cargo run                         # GUI
cargo run -- devices              # CLI: list scanners (any subcommand → CLI)
cargo test --lib                  # unit tests (fast, no hardware)
cargo test --lib -- --ignored     # + tests needing models/network/hardware
cargo clippy --all-targets        # keep at zero warnings
cargo fmt                         # rustfmt.toml: 120 cols
cargo run --example make_icons    # regenerate assets/icons/* (icns needs macOS)
scripts/macos/package.sh          # .app + .dmg (also linux/package.sh, windows/package.ps1)
```

Rust comes from Homebrew `rustup` (keg-only). If `cargo` isn't on PATH:
`export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"`.

`SCANPRO_CONFIG=/path/config.toml` points the app/CLI at a throwaway config;
use it for manual testing so the real one isn't touched.

## Architecture

- **`src/lib.rs`**: the core, with no UI. It's shared by the GUI, the CLI and
  the tests.
  - `scanner/`: the `Backend`/`Device` traits plus one backend per platform
    API: `sane` (dlopen'd libsane), `ica` (macOS ImageCaptureCore), `wia`
    (Windows WIA 2.0), `escl` (network AirScan). Device ids are
    `"<backend>:<id>"`.
  - `ocr/`: the `OcrEngine` trait. `vision` (macOS), `winrt` (Windows),
    `ocrs_engine` (pure Rust; models are downloaded on first use).
    `ocr::shared()` caches engines.
  - `pipeline.rs`: the `PageStore` (session directory of page files), blank
    detection, thumbnails, and OCR input.
  - `export.rs`: the PDF (pdf-writer), TIFF, image and text writers.
  - `naming.rs`: the name template language and name suggestions (heuristics
    or a local LLM, with the prompt and reply schema).
  - `llm.rs`: talking to local model servers (Ollama, LM Studio,
    OpenAI-compatible): list models with loaded/vision state, load on demand,
    and chat with a JSON-schema reply and optional page image.
  - `config.rs`: `Config` → `Profile`s (scan/processing/OCR/naming/output),
    stored as TOML.
  - `job.rs`: OCR → suggest → destination → export → after-save hooks. The
    GUI and CLI both save through here.
- **`src/app.rs`**: the GUI controller. State lives in a `thread_local` on
  the UI thread. Workers report back via `post()`
  (`slint::invoke_from_event_loop`).
- **`src/cli.rs`**: the clap CLI. Every option defaults to a profile.
- **`ui/app.slint`**: the whole UI. `build.rs` picks the widget style per
  target OS (`cupertino` on macOS, `fluent` elsewhere; override with
  `SCANPRO_SLINT_STYLE`).

### Rules that must not be broken

1. **OCR boxes are pixels with a top-left origin**, in the coordinates of the
   page *as oriented* (rotation applied) at its stored resolution.
   `pipeline::run_ocr` scales boxes back from the downscaled OCR input. The
   PDF writer is the only place that flips y.
2. **Rotation invalidates OCR.** `Page::rotate_by` clears `ocr`. A worker's
   OCR result is only applied if the page's rotation still matches.
   Auto-rotation (`run_ocr_oriented`) may return a different rotation, and
   the caller turns the page and its thumbnail to match. A manual rotation
   sets `Page::rotation_locked`, which turns auto-rotation off for that page;
   otherwise the re-OCR would undo the user's turn.
3. **Workers match pages by `key`, never by index.** Pages can be reordered
   or deleted while a scan, OCR or preview is in flight.
4. **Close scanner devices after every operation.** The epsonds backend
   (ES-300W) locks the scanner while it's open, and a lock left by a process
   that dies or quits without `sane_close` lasts until the scanner is
   power-cycled. That's why the GUI opens per operation (`device_with`), the
   CLI handles Ctrl+C, and nothing should `kill -9` a running scan. In
   testing, close the app normally.
5. **Never call `sane_get_devices` while a SANE device is open.** It
   re-probes USB and hung or wedged the ES-300W. `SaneBackend::open` looks up
   names *before* opening.
6. **ImageCaptureCore needs the main run loop.** In the GUI, call ICA from
   worker threads while the event loop runs. In the CLI, call it on the main
   thread (it spins the run loop itself). `discover_all` runs the first
   backend (ICA on macOS) on the calling thread for this reason.
7. **JPEG pages pass straight into the PDF** (DCTDecode) when unrotated. Don't
   add a decode/re-encode step to that path.
8. **The macOS bundle needs `disable-library-validation`.** cargo-packager
   signs with the hardened runtime, which otherwise refuses to `dlopen`
   Homebrew's libsane. SANE then vanishes silently: the installed app finds
   no scanners while `cargo run` works fine. Keep
   `assets/macos/entitlements.plist` wired up in `Cargo.toml`, and test
   scanner discovery from the *installed* app after packaging changes.

## Conventions

- Comments explain *why*, especially where code looks odd on purpose.
- Pure-logic code goes in the lib with unit tests. Hardware, model and network
  tests are `#[ignore]`.
- `docs/agent-plans/` is gitignored working scratch (the phase plan lives
  there).
- Windows code can only be type-checked from a Mac if `ring`
  (ureq/rustls) cross-compiles, and it doesn't. Real verification needs a
  Windows machine.
- No CI workflows unless asked: GitHub Actions compute isn't free on private
  repos.
