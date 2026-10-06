//! Converts the BEAT artwork (icons/beat-source.png) into the two icon assets
//! the app ships: icons/beat-256.png — decoded at runtime for the window and
//! taskbar icon — and icons/beat.ico, embedded into the executable's Windows
//! resources by build.rs.
//!
//! The source is trimmed to its opaque bounds, padded to a square (so square
//! artwork is not stretched) and resized with Lanczos3. The ICO holds
//! PNG-compressed entries for every common Explorer size (Windows Vista+).
//!
//! Run with `cargo run --example gen_icons`.

use image::{imageops::FilterType, RgbaImage};

const SOURCE: &str = "icons/beat-source.png";
const PNG_OUT: &str = "icons/beat-256.png";
const ICO_OUT: &str = "icons/beat.ico";
const SIZES: [u32; 6] = [16, 32, 48, 64, 128, 256];

/// Trim transparent margins, then center the artwork on a transparent square
/// so the resize preserves its aspect ratio.
fn to_square(src: &RgbaImage) -> RgbaImage {
    let (w, h) = src.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for (x, y, px) in src.enumerate_pixels() {
        if px[3] > 8 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x + 1);
            y1 = y1.max(y + 1);
        }
    }
    assert!(x0 < x1 && y0 < y1, "{SOURCE} has no opaque pixels");

    let (bw, bh) = (x1 - x0, y1 - y0);
    let side = bw.max(bh);
    let mut out = RgbaImage::new(side, side);
    let (ox, oy) = ((side - bw) / 2, (side - bh) / 2);
    for y in y0..y1 {
        for x in x0..x1 {
            out.put_pixel(x - x0 + ox, y - y0 + oy, *src.get_pixel(x, y));
        }
    }
    out
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).expect("encode png");
    png
}

fn main() {
    let src = image::open(SOURCE).unwrap_or_else(|e| panic!("read {SOURCE}: {e}")).into_rgba8();
    let base = to_square(&src);

    let frames: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&size| {
            let img = image::imageops::resize(&base, size, size, FilterType::Lanczos3);
            (size, encode_png(&img))
        })
        .collect();

    let (_, png256) = frames.iter().find(|(size, _)| *size == 256).expect("256 frame");
    std::fs::write(PNG_OUT, png256).expect("write beat-256.png");

    // ICONDIR + one 16-byte directory entry per size + the PNG payloads.
    let mut ico = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&(frames.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * frames.len() as u32;
    for (size, png) in &frames {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        ico.push(dim);
        ico.push(dim);
        ico.push(0);
        ico.push(0);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &frames {
        ico.extend_from_slice(png);
    }
    std::fs::write(ICO_OUT, &ico).expect("write beat.ico");

    println!("wrote {PNG_OUT} ({} bytes PNG) and {ICO_OUT} ({} bytes, sizes {:?})", png256.len(), ico.len(), SIZES);
}
