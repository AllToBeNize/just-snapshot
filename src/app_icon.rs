use windows_sys::Win32::UI::WindowsAndMessaging::HICON;

use crate::{capture::Image, icons};

const SAMPLES: i32 = 4;
// BGR order, matching the premultiplied BGRA layout of `Image`.
const TILE: [f32; 3] = [217.0, 111.0, 61.0];
const MARK: [f32; 3] = [255.0, 255.0, 255.0];
const BADGE: [f32; 3] = [31.0, 145.0, 214.0];

/// The tray icon: a blue tile with white viewfinder corners. `warning` adds an
/// amber "!" badge, used while the capture shortcut is held by another program.
pub fn render(size: i32, warning: bool) -> Image {
    let unit = size as f32 / 32.0;
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut sum = [0.0f32; 4];
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / unit;
                    let py = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / unit;
                    if let Some(color) = sample(px, py, warning) {
                        for channel in 0..3 {
                            sum[channel] += color[channel];
                        }
                        sum[3] += 255.0;
                    }
                }
            }
            let count = (SAMPLES * SAMPLES) as f32;
            // Average of opaque samples is already premultiplied by coverage.
            pixels.extend(sum.map(|value| (value / count).round() as u8));
        }
    }
    Image {
        width: size,
        height: size,
        pixels,
    }
}

pub fn create(size: i32, warning: bool) -> HICON {
    icons::create_icon(render(size, warning))
}

/// Color at a point in the 32×32 design grid, or `None` when transparent.
fn sample(x: f32, y: f32, warning: bool) -> Option<[f32; 3]> {
    if warning {
        let (cx, cy, radius) = (24.0, 24.0, 8.0);
        let distance = (x - cx).hypot(y - cy);
        if distance <= radius {
            let bar = (x - cx).abs() <= 1.3 && (cy - 5.0..=cy + 1.2).contains(&y);
            let dot = (x - cx).hypot(y - (cy + 3.8)) <= 1.4;
            return Some(if bar || dot { MARK } else { BADGE });
        }
        // A transparent ring separates the badge from the tile.
        if distance <= radius + 1.6 {
            return None;
        }
    }
    if !inside_round_rect(x, y, 1.0, 1.0, 31.0, 31.0, 7.0) {
        return None;
    }
    Some(if viewfinder(x, y) { MARK } else { TILE })
}

fn viewfinder(x: f32, y: f32) -> bool {
    let (near, far, arm, stroke) = (8.0, 24.0, 6.0, 2.6);
    let corner = |cx: f32, cy: f32, dx: f32, dy: f32| {
        let horizontal = (y - cy).abs() <= stroke / 2.0 && {
            let t = (x - cx) * dx;
            (-stroke / 2.0..=arm).contains(&t)
        };
        let vertical = (x - cx).abs() <= stroke / 2.0 && {
            let t = (y - cy) * dy;
            (-stroke / 2.0..=arm).contains(&t)
        };
        horizontal || vertical
    };
    let center = (x - 16.0).hypot(y - 16.0) <= 2.6;
    center
        || corner(near, near, 1.0, 1.0)
        || corner(far, near, -1.0, 1.0)
        || corner(near, far, 1.0, -1.0)
        || corner(far, far, -1.0, -1.0)
}

fn inside_round_rect(x: f32, y: f32, left: f32, top: f32, right: f32, bottom: f32, r: f32) -> bool {
    if x < left || x > right || y < top || y > bottom {
        return false;
    }
    let cx = x.clamp(left + r, right - r);
    let cy = y.clamp(top + r, bottom - r);
    (x - cx).hypot(y - cy) <= r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_badge_only_changes_the_bottom_right_corner() {
        let normal = render(32, false);
        let warning = render(32, true);
        let pixel = |image: &Image, x: i32, y: i32| {
            let index = ((y * 32 + x) * 4) as usize;
            image.pixels[index..index + 4].to_vec()
        };
        assert_eq!(pixel(&normal, 4, 16), pixel(&warning, 4, 16));
        assert_eq!(pixel(&normal, 0, 0)[3], 0, "rounded corner is transparent");
        assert_eq!(pixel(&normal, 16, 16), vec![255, 255, 255, 255]);
        assert_eq!(pixel(&warning, 27, 27)[..3], BADGE.map(|c| c as u8));
        assert!(!create(32, true).is_null());
    }
}
