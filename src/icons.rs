mod lucide;

use std::{cell::RefCell, collections::HashMap, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::RECT,
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::{
        CreateIconIndirect, DestroyCursor, GetCursor, SetCursor, HCURSOR, HICON, ICONINFO,
    },
};

use crate::{
    capture::{Image, Surface},
    gdiplus,
    theme::argb,
};

/// One segment of a vector icon in a 24×24 grid.
#[derive(Clone, Copy, Debug)]
pub enum Seg {
    M(f32, f32),
    L(f32, f32),
    C(f32, f32, f32, f32, f32, f32),
    Z,
}

/// Stroked outline plus optional solid fills; `accent` parts use the accent color.
struct Glyph {
    stroke: &'static [Seg],
    fill: &'static [Seg],
    accent: &'static [Seg],
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Rectangle,
    Text,
    Brush,
    Mosaic,
    Arrow,
    Number,
    Undo,
    Redo,
    LongCapture,
    Copy,
    Save,
    Pin,
    ExtractText,
    Translate,
    Settings,
    ChevronDown,
    Close,
    MouseLeft,
    MouseRight,
    Drag,
    Move,
    Resize,
}

pub struct Cursor(pub HCURSOR);

impl Cursor {
    pub fn brush() -> Option<Self> {
        let mut image = tinted_icon(Icon::Brush, 24, 0x001a_1a1a);
        let ink = image.pixels.clone();
        for y in 0..24_usize {
            for x in 0..24_usize {
                let offset = (y * 24 + x) * 4;
                if ink[offset + 3] == 0
                    && (y.saturating_sub(1)..=(y + 1).min(23)).any(|row| {
                        (x.saturating_sub(1)..=(x + 1).min(23))
                            .any(|col| ink[(row * 24 + col) * 4 + 3] > 100)
                    })
                {
                    image.pixels[offset..offset + 4].fill(255);
                }
            }
        }
        Self::new(image, 4, 20)
    }

    pub fn mosaic(diameter: i32) -> Option<Self> {
        let size = diameter + 4;
        let center = size / 2;
        let radius = f64::from(diameter) / 2.0;
        let mut pixels = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            for x in 0..size {
                let distance = (f64::from(x - center).hypot(f64::from(y - center)) - radius).abs();
                let pixel = if distance <= 0.65 {
                    [20, 20, 20, 255]
                } else if distance <= 1.65 {
                    [255; 4]
                } else {
                    [0; 4]
                };
                pixels.extend_from_slice(&pixel);
            }
        }
        Self::new(
            Image {
                width: size,
                height: size,
                pixels,
            },
            center as u32,
            center as u32,
        )
    }

    fn new(image: Image, x: u32, y: u32) -> Option<Self> {
        let cursor = unsafe { indirect(image, false, x, y) };
        (!cursor.is_null()).then_some(Self(cursor))
    }
}

/// Builds an icon from premultiplied BGRA pixels. Returns null on failure.
pub fn create_icon(image: Image) -> HICON {
    unsafe { indirect(image, true, 0, 0) }
}

unsafe fn indirect(image: Image, icon: bool, x: u32, y: u32) -> HICON {
    let Ok((surface, width, height)) = image.into_surface() else {
        return null_mut();
    };
    let mask_stride = (width as usize).div_ceil(16) * 2;
    let mask_bits = vec![0_u8; mask_stride * height as usize];
    let mask = CreateBitmap(width, height, 1, 1, mask_bits.as_ptr().cast());
    if mask.is_null() {
        return null_mut();
    }
    let handle = CreateIconIndirect(&ICONINFO {
        fIcon: i32::from(icon),
        xHotspot: x,
        yHotspot: y,
        hbmMask: mask,
        hbmColor: surface.bitmap(),
    });
    DeleteObject(mask);
    handle
}

impl Drop for Cursor {
    fn drop(&mut self) {
        unsafe {
            if GetCursor() == self.0 {
                SetCursor(null_mut());
            }
            DestroyCursor(self.0);
        }
    }
}

const MAX_CACHED_TILES: usize = 96;
const ACCENT: u32 = 0x00ff_b969;
type TileKey = (Icon, i32, u32);

thread_local! {
    static TILES: RefCell<HashMap<TileKey, Surface>> = RefCell::new(HashMap::new());
}

pub unsafe fn paint(dc: HDC, bounds: RECT, icon: Icon, color: u32) {
    let size = (bounds.right - bounds.left).min(bounds.bottom - bounds.top);
    if size <= 0 {
        return;
    }
    let key = (icon, size, color);
    TILES.with_borrow_mut(|tiles| {
        if tiles.len() >= MAX_CACHED_TILES && !tiles.contains_key(&key) {
            GdiFlush();
            tiles.clear();
        }
        let tile = match tiles.entry(key) {
            std::collections::hash_map::Entry::Occupied(tile) => tile.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let Ok((surface, _, _)) = tinted_icon(icon, size, color).into_surface() else {
                    return;
                };
                entry.insert(surface)
            }
        };
        AlphaBlend(
            dc,
            bounds.left + (bounds.right - bounds.left - size) / 2,
            bounds.top + (bounds.bottom - bounds.top - size) / 2,
            size,
            size,
            tile.dc,
            0,
            0,
            size,
            size,
            BLENDFUNCTION {
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
                ..Default::default()
            },
        );
    });
}

fn glyph(icon: Icon) -> Glyph {
    use lucide::*;
    let (stroke, fill, accent): (&'static [Seg], &'static [Seg], &'static [Seg]) = match icon {
        Icon::Rectangle => (SQUARE, &[], &[]),
        Icon::Text => (TYPE, &[], &[]),
        Icon::Brush => (BRUSH, &[], &[]),
        Icon::Mosaic => (MOSAIC, MOSAIC_FILL, &[]),
        Icon::Arrow => (MOVE_UP_RIGHT, &[], &[]),
        Icon::Number => (STEP_NUMBER, &[], &[]),
        Icon::Undo => (UNDO_2, &[], &[]),
        Icon::Redo => (REDO_2, &[], &[]),
        Icon::LongCapture => (LONG_CAPTURE, &[], &[]),
        Icon::Copy => (COPY, &[], &[]),
        Icon::Save => (SAVE, &[], &[]),
        Icon::Pin => (PIN, &[], &[]),
        Icon::ExtractText => (SCAN_TEXT, &[], &[]),
        Icon::Translate => (LANGUAGES, &[], &[]),
        Icon::Settings => (SETTINGS, &[], &[]),
        Icon::ChevronDown => (CHEVRON_DOWN, &[], &[]),
        Icon::Close => (X, &[], &[]),
        Icon::MouseLeft => (MOUSE_BUTTONS, &[], MOUSE_LEFT_FILL),
        Icon::MouseRight => (MOUSE_BUTTONS, &[], MOUSE_RIGHT_FILL),
        Icon::Drag => (SQUARE_DASHED_MOUSE_POINTER, &[], &[]),
        Icon::Move => (MOVE, &[], &[]),
        Icon::Resize => (MOVE_DIAGONAL_2, &[], &[]),
    };
    Glyph {
        stroke,
        fill,
        accent,
    }
}

/// Renders the vector icon at exactly `size` pixels, so it stays sharp at any scale.
fn tinted_icon(icon: Icon, size: i32, color: u32) -> Image {
    let glyph = glyph(icon);
    let scale = size as f32 / 24.0;
    // Lucide strokes are 2 units; keep at least 1.5 px so small icons stay legible.
    let width = (2.0 * scale).max(1.5);
    let rendered = unsafe {
        gdiplus::rasterize(size, |graphics| {
            graphics.fill_segments(glyph.fill, scale, argb(color, 255));
            graphics.fill_segments(glyph.accent, scale, argb(ACCENT, 255));
            graphics.stroke_segments(glyph.stroke, scale, argb(color, 255), width);
        })
    };
    rendered.unwrap_or_else(|| Image {
        width: size,
        height: size,
        pixels: vec![0; (size * size * 4) as usize],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::null_mut;

    #[test]
    fn native_icon_uses_the_requested_color_inside_its_cell() {
        let bounds = RECT {
            left: 10,
            top: 8,
            right: 28,
            bottom: 26,
        };
        unsafe {
            let surface = Surface::new(null_mut(), 38, 34).unwrap();
            let area = RECT {
                right: 38,
                bottom: 34,
                ..Default::default()
            };
            FillRect(surface.dc, &area, GetStockObject(WHITE_BRUSH));
            let color = 0x0030_7090;
            paint(surface.dc, bounds, Icon::Copy, color);
            let mut painted = 0;
            let mut softened = 0;
            for y in area.top..area.bottom {
                for x in area.left..area.right {
                    let pixel = GetPixel(surface.dc, x, y);
                    if pixel != 0x00ff_ffff {
                        assert!((bounds.left..bounds.right).contains(&x));
                        assert!((bounds.top..bounds.bottom).contains(&y));
                        painted += 1;
                        if pixel != color {
                            softened += 1;
                        }
                    }
                }
            }
            assert!(painted > 25);
            assert!(softened > 0);
        }
    }

    #[test]
    fn mosaic_has_two_dark_diagonal_tiles_and_two_open_tiles() {
        let image = tinted_icon(Icon::Mosaic, 24, 0);
        let alpha = |x: usize, y: usize| image.pixels[(y * 24 + x) * 4 + 3];
        assert_eq!(alpha(8, 8), 255);
        assert_eq!(alpha(15, 15), 255);
        assert_eq!(alpha(15, 8), 0);
        assert_eq!(alpha(8, 15), 0);
    }

    #[test]
    fn a_24_pixel_vertical_stroke_is_crisp() {
        // The "T" stem sits on x = 11..13 at 24 px: fully covered, no blur.
        let image = tinted_icon(Icon::Text, 24, 0);
        let alpha = |x: usize, y: usize| image.pixels[(y * 24 + x) * 4 + 3];
        assert_eq!((alpha(11, 12), alpha(12, 12)), (255, 255));
        assert_eq!((alpha(10, 12), alpha(13, 12)), (0, 0));
    }
}
#[test]
fn creates_native_cursors_for_the_brush_and_mosaic_size_limits() {
    assert!(Cursor::brush().is_some());
    assert!(Cursor::mosaic(4).is_some());
    assert!(Cursor::mosaic(128).is_some());
}
