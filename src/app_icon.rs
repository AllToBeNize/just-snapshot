use windows_sys::Win32::UI::WindowsAndMessaging::HICON;

use crate::{capture::Image, icons};

const SAMPLES: i32 = 4;
// BGR order, matching the premultiplied BGRA layout of `Image`.
const MARK: [f32; 3] = [255.0, 255.0, 255.0];
const BADGE: [f32; 3] = [31.0, 145.0, 214.0];

/// The app icon, pre-scaled from `assets/icon/master.png` (the same images
/// make up `assets/icon/snapshot.ico`, the executable's icon).
const SIZES: [(i32, &[u8]); 8] = [
    (16, include_bytes!("../assets/icon/png/icon-16.png")),
    (20, include_bytes!("../assets/icon/png/icon-20.png")),
    (24, include_bytes!("../assets/icon/png/icon-24.png")),
    (32, include_bytes!("../assets/icon/png/icon-32.png")),
    (40, include_bytes!("../assets/icon/png/icon-40.png")),
    (48, include_bytes!("../assets/icon/png/icon-48.png")),
    (64, include_bytes!("../assets/icon/png/icon-64.png")),
    (256, include_bytes!("../assets/icon/png/icon-256.png")),
];

/// Premultiplied BGRA pixels of an embedded RGBA PNG.
fn decode(bytes: &[u8]) -> Option<Image> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buffer).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let mut pixels = Vec::with_capacity(buffer.len());
    for rgba in buffer[..info.buffer_size()].chunks_exact(4) {
        let alpha = u32::from(rgba[3]);
        let premultiply = |channel: u8| ((u32::from(channel) * alpha + 127) / 255) as u8;
        pixels.extend([premultiply(rgba[2]), premultiply(rgba[1]), premultiply(rgba[0]), rgba[3]]);
    }
    Some(Image { width: info.width as i32, height: info.height as i32, pixels })
}

/// `image` shrunk to `size`×`size` by averaging the pixels each one covers.
fn shrink(image: &Image, size: i32) -> Image {
    if image.width == size {
        return image.clone();
    }
    let scale = image.width as f32 / size as f32;
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (x0, y0) = ((x as f32 * scale) as i32, (y as f32 * scale) as i32);
            let (x1, y1) = ((((x + 1) as f32 * scale).ceil() as i32).min(image.width), (((y + 1) as f32 * scale).ceil() as i32).min(image.height));
            let mut sum = [0u32; 4];
            let mut count = 0;
            for sy in y0..y1.max(y0 + 1) {
                for sx in x0..x1.max(x0 + 1) {
                    let index = ((sy * image.width + sx) * 4) as usize;
                    for channel in 0..4 {
                        sum[channel] += u32::from(image.pixels[index + channel]);
                    }
                    count += 1;
                }
            }
            pixels.extend(sum.map(|value| (value / count) as u8));
        }
    }
    Image { width: size, height: size, pixels }
}

/// The tray and window icon at `size` pixels. `warning` adds an amber "!"
/// badge, used while the capture shortcut is held by another program.
pub fn render(size: i32, warning: bool) -> Image {
    let source = SIZES
        .iter()
        .find(|(pixels, _)| *pixels >= size)
        .unwrap_or(&SIZES[SIZES.len() - 1]);
    let mut image = decode(source.1).map(|image| shrink(&image, size)).unwrap_or(Image {
        width: size,
        height: size,
        pixels: vec![0; (size * size * 4) as usize],
    });
    if warning {
        badge(&mut image);
    }
    image
}

/// Paints the warning badge, with a transparent ring cut around it.
fn badge(image: &mut Image) {
    let size = image.width;
    let unit = size as f32 / 32.0;
    for y in 0..size {
        for x in 0..size {
            let mut colored = [0.0f32; 4];
            let mut cleared = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / unit;
                    let py = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / unit;
                    match badge_sample(px, py) {
                        Some(Some(color)) => {
                            for channel in 0..3 {
                                colored[channel] += color[channel];
                            }
                            colored[3] += 255.0;
                        }
                        Some(None) => cleared += 1,
                        None => {}
                    }
                }
            }
            let total = (SAMPLES * SAMPLES) as f32;
            let touched = colored[3] / 255.0 + cleared as f32;
            if touched == 0.0 {
                continue;
            }
            // The icon keeps the share of samples the badge and its ring miss.
            let keep = 1.0 - touched / total;
            let index = ((y * size + x) * 4) as usize;
            for channel in 0..4 {
                let original = f32::from(image.pixels[index + channel]) * keep;
                image.pixels[index + channel] = (original + colored[channel] / total).round() as u8;
            }
        }
    }
}

/// In the 32×32 grid: `Some(Some(color))` on the badge, `Some(None)` on the
/// transparent ring around it, `None` elsewhere.
fn badge_sample(x: f32, y: f32) -> Option<Option<[f32; 3]>> {
    let (cx, cy, radius) = (24.0, 24.0, 8.0);
    let distance = (x - cx).hypot(y - cy);
    if distance <= radius {
        let bar = (x - cx).abs() <= 1.3 && (cy - 5.0..=cy + 1.2).contains(&y);
        let dot = (x - cx).hypot(y - (cy + 3.8)) <= 1.4;
        return Some(Some(if bar || dot { MARK } else { BADGE }));
    }
    (distance <= radius + 1.6).then_some(None)
}

pub fn create(size: i32, warning: bool) -> HICON {
    icons::create_icon(render(size, warning))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_size_decodes_and_the_badge_only_changes_its_corner() {
        for (size, bytes) in SIZES {
            let image = decode(bytes).expect("embedded icon decodes");
            assert_eq!((image.width, image.height), (size, size));
        }
        let normal = render(32, false);
        let warning = render(32, true);
        let pixel = |image: &Image, x: i32, y: i32| {
            let index = ((y * 32 + x) * 4) as usize;
            image.pixels[index..index + 4].to_vec()
        };
        assert_eq!(pixel(&normal, 4, 16), pixel(&warning, 4, 16));
        assert_eq!(pixel(&normal, 0, 0)[3], 0, "the rounded corner is transparent");
        assert!(pixel(&normal, 16, 16)[3] > 240, "the middle is opaque");
        assert_eq!(pixel(&warning, 27, 27)[..3], BADGE.map(|c| c as u8));
        // Sizes between the embedded ones are shrunk from the next larger one.
        assert_eq!(render(18, false).width, 18);
        assert!(!create(32, true).is_null());
    }
}
