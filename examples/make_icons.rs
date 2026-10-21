//! Regenerates the app icon set in `assets/icons/` (PNG sizes, `.ico`, and on
//! macOS `.icns` via `iconutil`). The icon is drawn in code so it can be
//! tweaked without an image editor:
//!
//! ```sh
//! cargo run --example make_icons
//! ```

use std::path::Path;

use image::{Rgba, RgbaImage};

/// Draw at `size` with 4× supersampling for smooth edges.
fn draw(size: u32) -> RgbaImage {
    const SS: u32 = 4;
    let n = size * SS;
    let f = n as f32;
    let mut big = RgbaImage::new(n, n);
    let rounded = |x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32| {
        let cx = x.clamp(x0 + r, x1 - r);
        let cy = y.clamp(y0 + r, y1 - r);
        x >= x0 && x <= x1 && y >= y0 && y <= y1 && ((x - cx).powi(2) + (y - cy).powi(2)) <= r * r
    };
    for (px, py, p) in big.enumerate_pixels_mut() {
        let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
        // Background tile: blue gradient, macOS-style rounded square.
        let m = f * 0.06;
        if !rounded(x, y, m, m, f - m, f - m, f * 0.2) {
            continue;
        }
        let t = y / f;
        let mut c = [(38.0 + 30.0 * t) as u8, (110.0 - 40.0 * t) as u8, (235.0 - 50.0 * t) as u8, 255];
        // Sheet of paper.
        let (sx0, sy0, sx1, sy1) = (f * 0.27, f * 0.17, f * 0.73, f * 0.83);
        if rounded(x, y, sx0, sy0, sx1, sy1, f * 0.035) {
            c = [250, 251, 253, 255];
            // Text lines.
            for i in 0..6 {
                let ly = f * (0.29 + i as f32 * 0.075);
                let lx1 = if i % 3 == 2 { f * 0.55 } else { f * 0.65 };
                if y >= ly && y <= ly + f * 0.028 && x >= f * 0.35 && x <= lx1 {
                    c = [150, 160, 178, 255];
                }
            }
        }
        // Scan beam across the sheet, with a soft glow.
        let beam = f * 0.52;
        let d = (y - beam).abs();
        if x >= f * 0.18 && x <= f * 0.82 && d < f * 0.06 {
            let a = if d < f * 0.012 { 1.0 } else { (1.0 - d / (f * 0.06)).powi(2) * 0.55 };
            let glow = [64.0, 230.0, 190.0];
            for k in 0..3 {
                c[k] = (c[k] as f32 * (1.0 - a) + glow[k] * a) as u8;
            }
        }
        *p = Rgba(c);
    }
    image::imageops::resize(&big, size, size, image::imageops::FilterType::Triangle)
}

fn main() -> anyhow::Result<()> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icons");
    std::fs::create_dir_all(&dir)?;
    for size in [32, 128, 256, 512] {
        draw(size).save(dir.join(format!("{size}x{size}.png")))?;
    }

    // .ico: PNG-compressed entries, which every Windows since Vista reads.
    let mut ico = Vec::new();
    let sizes = [16u32, 24, 32, 48, 64, 128, 256];
    let pngs: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| {
            let mut b = Vec::new();
            draw(s).write_to(&mut std::io::Cursor::new(&mut b), image::ImageFormat::Png).unwrap();
            b
        })
        .collect();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, png) in sizes.iter().zip(&pngs) {
        let dim = if s >= 256 { 0 } else { s as u8 };
        ico.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for png in &pngs {
        ico.extend_from_slice(png);
    }
    std::fs::write(dir.join("icon.ico"), ico)?;

    if cfg!(target_os = "macos") {
        let set = std::env::temp_dir().join("ScanPro.iconset");
        std::fs::create_dir_all(&set)?;
        for s in [16u32, 32, 128, 256, 512] {
            draw(s).save(set.join(format!("icon_{s}x{s}.png")))?;
            draw(s * 2).save(set.join(format!("icon_{s}x{s}@2x.png")))?;
        }
        let status = std::process::Command::new("iconutil")
            .args(["-c", "icns", "-o"])
            .arg(dir.join("icon.icns"))
            .arg(&set)
            .status()?;
        anyhow::ensure!(status.success(), "iconutil failed");
        std::fs::remove_dir_all(&set)?;
    } else {
        eprintln!("icon.icns is only regenerated on macOS (needs iconutil)");
    }
    println!("icons written to {}", dir.display());
    Ok(())
}
