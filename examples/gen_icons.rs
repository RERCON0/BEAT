//! One-off generator for the BEAT icon: a dark rounded square with the accent
//! equalizer bars, matching the STRIKE/SNATCH terminal aesthetic. Renders at
//! 4x and downsamples for antialiasing, writes icons/beat-256.png and
//! icons/beat.ico (PNG-compressed ICO, Windows Vista+).
//!
//! Run with `cargo run --example gen_icons`.

use image::{Rgba, RgbaImage};

const SIZE: u32 = 256;
const SCALE: u32 = 4;
const BG: [u8; 4] = [0x14, 0x14, 0x18, 0xff];
const ACCENT: [u8; 4] = [0x3e, 0xe2, 0x7f, 0xff];

fn inside_rounded(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    let dx = x - cx;
    let dy = y - cy;
    dx * dx + dy * dy <= r * r
}

fn fill_rounded(img: &mut RgbaImage, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, color: [u8; 4]) {
    let (w, h) = img.dimensions();
    let rgba = Rgba(color);
    for py in 0..h {
        for px in 0..w {
            let x = px as f32 + 0.5;
            let y = py as f32 + 0.5;
            if inside_rounded(x, y, x0, y0, x1, y1, r) {
                img.put_pixel(px, py, rgba);
            }
        }
    }
}

fn main() {
    let big = SIZE * SCALE;
    let s = SCALE as f32;
    let mut img = RgbaImage::new(big, big);

    // Full-bleed rounded square, like the STRIKE icon.
    let margin = 6.0 * s;
    fill_rounded(&mut img, margin, margin, big as f32 - margin, big as f32 - margin, 44.0 * s, BG);

    // Equalizer bars, bottom-aligned, symmetric.
    let bar_w = 22.0 * s;
    let gap = 14.0 * s;
    let bars = 5usize;
    let heights = [0.42, 0.72, 1.0, 0.62, 0.34];
    let total = bars as f32 * bar_w + (bars - 1) as f32 * gap;
    let start = (big as f32 - total) / 2.0;
    let baseline = big as f32 * 0.78;
    let max_h = big as f32 * 0.46;
    for (i, fraction) in heights.iter().enumerate() {
        let x0 = start + i as f32 * (bar_w + gap);
        let h = max_h * fraction;
        fill_rounded(&mut img, x0, baseline - h, x0 + bar_w, baseline, bar_w / 2.0, ACCENT);
    }

    let small = image::imageops::resize(&img, SIZE, SIZE, image::imageops::FilterType::Lanczos3);
    small.save("icons/beat-256.png").expect("write beat-256.png");

    let mut png = Vec::new();
    small
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encode png");

    // ICONDIR + one entry (256x256 is encoded as 0) + the PNG payload.
    let mut ico = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.push(0);
    ico.push(0);
    ico.push(0);
    ico.push(0);
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&32u16.to_le_bytes());
    ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes());
    ico.extend_from_slice(&png);
    std::fs::write("icons/beat.ico", &ico).expect("write beat.ico");
    println!("wrote icons/beat-256.png ({} bytes PNG) and icons/beat.ico", png.len());
}
