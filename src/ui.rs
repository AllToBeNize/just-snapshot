//! Drawing helpers shared by the custom-drawn settings window and dialogs.

use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HWND, RECT, SIZE},
        Graphics::{
            Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND},
            Gdi::*,
            GdiPlus::PointF,
        },
    },
};

use crate::{
    gdiplus::Graphics,
    theme::{argb, Palette},
    wide,
};

pub struct Fonts {
    pub title: HFONT,
    pub heading: HFONT,
    pub body: HFONT,
    pub small: HFONT,
    pub key: HFONT,
}

impl Fonts {
    pub unsafe fn new(scale: f32) -> Self {
        let font = |size: f32, weight: i32, face: *const u16| {
            CreateFontW(
                -(size * scale).round() as i32,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                face,
            )
        };
        let ui = w!("Microsoft YaHei UI");
        Self {
            title: font(20.0, FW_SEMIBOLD as i32, ui),
            heading: font(14.0, FW_SEMIBOLD as i32, ui),
            body: font(14.0, FW_NORMAL as i32, ui),
            small: font(12.0, FW_NORMAL as i32, ui),
            key: font(12.5, FW_SEMIBOLD as i32, w!("Segoe UI")),
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for font in [self.title, self.heading, self.body, self.small, self.key] {
            if !font.is_null() {
                unsafe { DeleteObject(font) };
            }
        }
    }
}

/// Rounded corners on Windows 11; ignored on Windows 10.
pub unsafe fn round_corners(window: HWND) {
    let preference = DWMWCP_ROUND;
    DwmSetWindowAttribute(
        window,
        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
        (&preference as *const i32).cast(),
        size_of::<i32>() as u32,
    );
}

pub unsafe fn fill(dc: HDC, rect: RECT, color: u32) {
    SetDCBrushColor(dc, color);
    FillRect(dc, &rect, GetStockObject(DC_BRUSH));
}

pub unsafe fn text(dc: HDC, mut rect: RECT, value: &str, font: HFONT, color: u32, format: u32) {
    let value = wide(value);
    SelectObject(dc, font);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, color);
    DrawTextW(dc, value.as_ptr(), -1, &mut rect, format | DT_NOPREFIX);
}

/// Height of `value` wrapped to `width` with the given `DrawText` wrap flags.
pub unsafe fn wrapped_height(dc: HDC, value: &str, font: HFONT, width: i32, wrap: u32) -> i32 {
    let value = wide(value);
    let previous = SelectObject(dc, font);
    let mut rect = RECT {
        right: width,
        ..Default::default()
    };
    DrawTextW(
        dc,
        value.as_ptr(),
        -1,
        &mut rect,
        DT_CALCRECT | wrap | DT_NOPREFIX,
    );
    SelectObject(dc, previous);
    rect.bottom
}

pub unsafe fn measure(dc: HDC, value: &str, font: HFONT) -> i32 {
    let units: Vec<u16> = value.encode_utf16().collect();
    let previous = SelectObject(dc, font);
    let mut size = SIZE::default();
    GetTextExtentPoint32W(dc, units.as_ptr(), units.len() as i32, &mut size);
    SelectObject(dc, previous);
    size.cx
}

pub fn to_f(rect: RECT) -> [f32; 4] {
    [
        rect.left as f32,
        rect.top as f32,
        rect.right as f32,
        rect.bottom as f32,
    ]
}

pub fn inset([left, top, right, bottom]: [f32; 4], amount: f32) -> [f32; 4] {
    [left + amount, top + amount, right - amount, bottom - amount]
}

pub fn inflate(rect: RECT, amount: i32) -> RECT {
    RECT {
        left: rect.left - amount,
        top: rect.top - amount,
        right: rect.right + amount,
        bottom: rect.bottom + amount,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Check,
    Exclamation,
    /// A download arrow, for an available update.
    Arrow,
}

/// A filled circle with a white check, exclamation mark or download arrow.
pub unsafe fn badge(dc: HDC, rect: RECT, color: u32, kind: Badge, on: u32) {
    let Some(graphics) = Graphics::new(dc) else {
        return;
    };
    let bounds = to_f(rect);
    graphics.fill_ellipse(bounds, argb(color, 255));
    let size = bounds[2] - bounds[0];
    let (cx, cy) = ((bounds[0] + bounds[2]) / 2.0, (bounds[1] + bounds[3]) / 2.0);
    let width = (size * 0.13).max(1.4);
    match kind {
        Badge::Arrow => {
            let color = argb(on, 255);
            graphics.line((cx, cy - size * 0.24), (cx, cy + size * 0.12), color, width);
            graphics.line((cx - size * 0.17, cy - size * 0.03), (cx, cy + size * 0.14), color, width);
            graphics.line((cx + size * 0.17, cy - size * 0.03), (cx, cy + size * 0.14), color, width);
            graphics.line((cx - size * 0.22, cy + size * 0.26), (cx + size * 0.22, cy + size * 0.26), color, width);
        }
        Badge::Check => {
            if let Some(pen) = graphics.pen(on, width) {
                graphics.lines(
                    &pen,
                    &[
                        PointF {
                            X: cx - size * 0.24,
                            Y: cy + size * 0.01,
                        },
                        PointF {
                            X: cx - size * 0.06,
                            Y: cy + size * 0.19,
                        },
                        PointF {
                            X: cx + size * 0.25,
                            Y: cy - size * 0.17,
                        },
                    ],
                );
            }
        }
        Badge::Exclamation => {
            graphics.fill_round(
                [
                    cx - width / 2.0,
                    cy - size * 0.28,
                    cx + width / 2.0,
                    cy + size * 0.07,
                ],
                width / 2.0,
                argb(on, 255),
            );
            let dot = cy + size * 0.22;
            graphics.fill_ellipse(
                [
                    cx - width * 0.6,
                    dot - width * 0.6,
                    cx + width * 0.6,
                    dot + width * 0.6,
                ],
                argb(on, 255),
            );
        }
    }
}

/// Filled or outlined pill button.
pub unsafe fn button(
    dc: HDC,
    rect: RECT,
    label: &str,
    font: HFONT,
    palette: &Palette,
    primary: bool,
    face: u32,
    radius: f32,
) {
    if let Some(graphics) = Graphics::new(dc) {
        graphics.fill_round(to_f(rect), radius, argb(face, 255));
        if !primary {
            graphics.stroke_round(
                inset(to_f(rect), 0.5),
                radius,
                argb(palette.key_border, 255),
                1.0,
            );
        }
    }
    let color = if primary { palette.on_accent } else { palette.text };
    text(
        dc,
        rect,
        label,
        font,
        color,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}
