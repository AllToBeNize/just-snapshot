use windows_sys::{
    core::w,
    Win32::{
        Foundation::{POINT, RECT},
        Graphics::Gdi::*,
    },
};

use crate::{
    annotation::{Style, Tool, INK},
    text_mark::{Align, Look},
    selection_geometry::contains,
    gdiplus::Graphics,
    theme::{self, argb},
    toolbar::{paint_panel, SHADOW},
    ui::to_f,
    typography::{self, TextStyle},
    wide,
};

const HEIGHT: i32 = 40;
const GAP: i32 = 4;
const PADDING: i32 = 5;
const FIELD: i32 = 104;
/// A labeled on/off button such as 圆角 or 底色.
const TOGGLE: i32 = 42;
const COLORS: [u32; 8] = [
    INK,
    0x0031_91ff,
    0x0000_c8f3,
    0x0061_a825,
    0x00ed_7a25,
    0x00c4_4e84,
    0x0022_2222,
    0x00ff_ffff,
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Primary(i32),
    Block(i32),
    Color(u32),
    Toggle(Flag),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flag {
    Round,
    Dashed,
    Outline,
    Fill,
    Left,
    Center,
    Right,
}

impl Flag {
    fn label(self) -> *const u16 {
        match self {
            Self::Round => w!("圆角"),
            Self::Dashed => w!("虚线"),
            Self::Outline => w!("描边"),
            Self::Fill => w!("底色"),
            Self::Left => w!("居左"),
            Self::Center => w!("居中"),
            Self::Right => w!("居右"),
        }
    }

    fn on(self, style: &Style) -> bool {
        match self {
            Self::Round => style.rect_round,
            Self::Dashed => style.rect_dashed,
            Self::Outline => style.text_look == Look::Outline,
            Self::Fill => style.text_look == Look::Fill,
            Self::Left => style.text_align == Align::Left,
            Self::Center => style.text_align == Align::Center,
            Self::Right => style.text_align == Align::Right,
        }
    }
}

/// The on/off buttons a tool offers.
fn flags(tool: Tool) -> &'static [Flag] {
    match tool {
        Tool::Rectangle => &[Flag::Round, Flag::Dashed],
        Tool::Text => &[Flag::Left, Flag::Center, Flag::Right, Flag::Outline, Flag::Fill],
        _ => &[],
    }
}

impl Choice {
    pub fn adjust(self, tool: Tool, style: &mut Style) {
        match (tool, self) {
            (Tool::Brush, Self::Primary(step)) => {
                style.brush_width = (style.brush_width + step).clamp(1, 64)
            }
            (Tool::Mosaic, Self::Primary(step)) => {
                style.mosaic_width = (style.mosaic_width + step * 4).clamp(4, 128)
            }
            (Tool::Mosaic, Self::Block(step)) => {
                style.mosaic_block = (style.mosaic_block + step * 2).clamp(2, 48)
            }
            (Tool::Text, Self::Primary(step)) => {
                style.text_size = (style.text_size + step * 2).clamp(8, 128)
            }
            (Tool::Arrow, Self::Primary(step)) => {
                style.arrow_width = (style.arrow_width + step).clamp(1, 32)
            }
            (Tool::Number, Self::Primary(step)) => {
                style.number_size = (style.number_size + step * 4).clamp(16, 96)
            }
            (Tool::Rectangle, Self::Primary(step)) => {
                style.rect_width = (style.rect_width + step).clamp(1, 32)
            }
            (Tool::Magnify, Self::Primary(step)) => {
                style.magnify_zoom = (style.magnify_zoom + step).clamp(2, 6)
            }
            (Tool::Rectangle, Self::Toggle(Flag::Round)) => style.rect_round = !style.rect_round,
            (Tool::Rectangle, Self::Toggle(Flag::Dashed)) => style.rect_dashed = !style.rect_dashed,
            // Outline and fill exclude each other; clicking the active one turns it off.
            (Tool::Text, Self::Toggle(flag @ (Flag::Outline | Flag::Fill))) => {
                let look = if flag == Flag::Outline { Look::Outline } else { Look::Fill };
                style.text_look = if style.text_look == look { Look::Plain } else { look };
            }
            (Tool::Text, Self::Toggle(Flag::Left)) => style.text_align = Align::Left,
            (Tool::Text, Self::Toggle(Flag::Center)) => style.text_align = Align::Center,
            (Tool::Text, Self::Toggle(Flag::Right)) => style.text_align = Align::Right,
            (Tool::Rectangle, Self::Color(color)) => style.rect_color = color,
            (Tool::Magnify, Self::Color(color)) => style.magnify_color = color,
            (Tool::Text, Self::Color(color)) => style.text_color = color,
            (Tool::Arrow, Self::Color(color)) => style.arrow_color = color,
            (Tool::Number, Self::Color(color)) => style.number_color = color,
            (Tool::Brush, Self::Color(color)) => style.brush_color = color,
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
pub struct Options {
    pub bounds: RECT,
    tool: Tool,
    swatch: i32,
}

impl Options {
    pub fn new(tool: Tool, toolbar: RECT, area: RECT, selection: RECT) -> Option<Self> {
        // No wider than the toolbar it hangs under; swatches shrink first.
        let available = (area.right - area.left).min(toolbar.right - toolbar.left);
        let toggles = TOGGLE * flags(tool).len() as i32;
        let swatch = ((available - FIELD - toggles - PADDING * 2) / COLORS.len() as i32).min(26);
        let width = match tool {
            Tool::Select => return None,
            Tool::Mosaic => FIELD * 2,
            _ if swatch >= 12 => FIELD + toggles + swatch * COLORS.len() as i32,
            _ => return None,
        } + PADDING * 2;
        if width > available {
            return None;
        }
        // Hang under the toolbar, centered with it. When the toolbar sits above or
        // inside the selection, open upward instead so the selection stays visible.
        let center = (toolbar.left + toolbar.right) / 2;
        let left = (center - width / 2).clamp(area.left, area.right - width);
        let below = toolbar.bottom + GAP;
        let above = toolbar.top - GAP - HEIGHT;
        let fits_below = below + HEIGHT <= area.bottom;
        let top = if toolbar.top >= selection.bottom && fits_below {
            below
        } else if above >= area.top {
            above
        } else if fits_below {
            below
        } else {
            return None;
        };
        Some(Self {
            bounds: RECT {
                left,
                top,
                right: left + width,
                bottom: top + HEIGHT,
            },
            tool,
            swatch,
        })
    }

    fn cell(self, left: i32, width: i32) -> RECT {
        let left = self.bounds.left + PADDING + left;
        RECT {
            left,
            top: self.bounds.top + 5,
            right: left + width,
            bottom: self.bounds.bottom - 5 - SHADOW,
        }
    }

    fn buttons(self) -> impl Iterator<Item = (RECT, Choice)> {
        let primary = [
            (self.cell(31, 22), Choice::Primary(-1)),
            (self.cell(81, 22), Choice::Primary(1)),
        ];
        let block = [
            (self.cell(FIELD + 31, 22), Choice::Block(-1)),
            (self.cell(FIELD + 81, 22), Choice::Block(1)),
        ];
        let flags = flags(self.tool);
        let toggles = flags.iter().enumerate().map(move |(index, &flag)| {
            (
                self.cell(FIELD + TOGGLE * index as i32 + 2, TOGGLE - 4),
                Choice::Toggle(flag),
            )
        });
        let offset = FIELD + TOGGLE * flags.len() as i32;
        let colors = COLORS.into_iter().enumerate().map(move |(index, color)| {
            (
                self.cell(offset + self.swatch * index as i32, self.swatch),
                Choice::Color(color),
            )
        });
        let mosaic = self.tool == Tool::Mosaic;
        primary
            .into_iter()
            .chain(block.into_iter().filter(move |_| mosaic))
            .chain(toggles)
            .chain(colors.filter(move |_| !mosaic))
    }

    pub fn hit(self, point: POINT) -> Option<Choice> {
        self.buttons()
            .find_map(|(rect, choice)| contains(rect, point).then_some(choice))
    }

    pub unsafe fn paint(self, dc: HDC, style: Style, hovered: Option<Choice>) {
        let saved = SaveDC(dc);
        if saved == 0 {
            return;
        }
        paint_panel(dc, self.bounds);
        SelectObject(dc, typography::font(TextStyle::HintDescription));
        SetBkMode(dc, TRANSPARENT as i32);
        let palette = theme::palette();
        SetTextColor(dc, palette.text);
        let (name, value, color) = match self.tool {
            Tool::Brush => (w!("粗细"), style.brush_width, style.brush_color),
            Tool::Mosaic => (w!("粗细"), style.mosaic_width, 0),
            Tool::Text => (w!("字号"), style.text_size, style.text_color),
            Tool::Arrow => (w!("粗细"), style.arrow_width, style.arrow_color),
            Tool::Number => (w!("大小"), style.number_size, style.number_color),
            Tool::Rectangle => (w!("粗细"), style.rect_width, style.rect_color),
            Tool::Magnify => (w!("倍数"), style.magnify_zoom, style.magnify_color),
            Tool::Select => return,
        };
        label(dc, self.cell(0, 31), name);
        label(dc, self.cell(53, 28), wide(&value.to_string()).as_ptr());
        if self.tool == Tool::Mosaic {
            label(dc, self.cell(FIELD, 31), w!("像素"));
            label(
                dc,
                self.cell(FIELD + 53, 28),
                wide(&style.mosaic_block.to_string()).as_ptr(),
            );
        }
        let graphics = Graphics::new(dc);
        for (rect, choice) in self.buttons() {
            let hover = hovered == Some(choice);
            match choice {
                Choice::Color(swatch) => {
                    let Some(graphics) = &graphics else { continue };
                    let size = (rect.right - rect.left).min(rect.bottom - rect.top) as f32 - 8.0;
                    let cx = (rect.left + rect.right) as f32 / 2.0;
                    let cy = (rect.top + rect.bottom) as f32 / 2.0;
                    let r = size / 2.0;
                    if swatch == color || hover {
                        let ring = r + 3.0;
                        let tone = if swatch == color { palette.accent } else { palette.card_border };
                        graphics.fill_ellipse([cx - ring, cy - ring, cx + ring, cy + ring], argb(tone, 255));
                        let gap = r + 1.5;
                        graphics.fill_ellipse([cx - gap, cy - gap, cx + gap, cy + gap], argb(palette.card, 255));
                    }
                    graphics.fill_ellipse([cx - r, cy - r, cx + r, cy + r], argb(swatch, 255));
                    // A hairline keeps white and black swatches visible on any theme.
                    graphics.stroke_round([cx - r, cy - r, cx + r, cy + r], r, argb(palette.text, 40), 1.0);
                }
                Choice::Toggle(flag) => {
                    let on = flag.on(&style);
                    if let Some(graphics) = &graphics {
                        let fill = if on {
                            palette.accent_soft
                        } else if hover {
                            palette.hover
                        } else {
                            palette.card
                        };
                        graphics.fill_round(to_f(rect), 6.0, argb(fill, 255));
                        if on {
                            graphics.stroke_round(ui_inset(to_f(rect)), 6.0, argb(palette.accent, 255), 1.0);
                        }
                    }
                    SetTextColor(dc, if on { palette.accent } else { palette.text });
                    label(dc, rect, flag.label());
                    SetTextColor(dc, palette.text);
                }
                Choice::Primary(step) | Choice::Block(step) => {
                    if hover {
                        if let Some(graphics) = &graphics {
                            graphics.fill_round(to_f(rect), 6.0, argb(palette.hover, 255));
                        }
                    }
                    label(dc, rect, if step < 0 { w!("−") } else { w!("+") });
                }
            }
        }
        drop(graphics);
        RestoreDC(dc, saved);
    }
}

fn ui_inset(bounds: [f32; 4]) -> [f32; 4] {
    crate::ui::inset(bounds, 0.5)
}

unsafe fn label(dc: HDC, mut rect: RECT, text: *const u16) {
    DrawTextW(
        dc,
        text,
        -1,
        &mut rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{Desktop, Surface};
    use std::ptr::null_mut;

    #[test]
    fn toggles_flip_styles_and_outline_and_fill_exclude_each_other() {
        let mut style = Style::default();
        Choice::Toggle(Flag::Round).adjust(Tool::Rectangle, &mut style);
        assert!(style.rect_round && !style.rect_dashed);
        Choice::Toggle(Flag::Outline).adjust(Tool::Text, &mut style);
        assert_eq!(style.text_look, Look::Outline);
        Choice::Toggle(Flag::Fill).adjust(Tool::Text, &mut style);
        assert_eq!(style.text_look, Look::Fill);
        Choice::Toggle(Flag::Fill).adjust(Tool::Text, &mut style);
        assert_eq!(style.text_look, Look::Plain);
        Choice::Primary(10).adjust(Tool::Magnify, &mut style);
        assert_eq!(style.magnify_zoom, 6, "zoom stops at 6×");
        // Every button of the rectangle bar is hit at its center.
        let area = RECT { right: 800, bottom: 300, ..Default::default() };
        let toolbar = RECT { left: 100, top: 100, right: 700, bottom: 146 };
        let options = Options::new(Tool::Rectangle, toolbar, area, RECT { bottom: 90, ..area }).unwrap();
        let toggles = options.buttons().filter(|(_, choice)| matches!(choice, Choice::Toggle(_))).count();
        assert_eq!(toggles, 2);
        for (rect, choice) in options.buttons() {
            let center = POINT { x: (rect.left + rect.right) / 2, y: (rect.top + rect.bottom) / 2 };
            assert!(options.hit(center) == Some(choice));
        }
    }

    /// The option bars with toggles. Run with `<test binary> annotation_options --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        unsafe {
            let area = RECT { right: 620, bottom: 170, ..Default::default() };
            let surface = Surface::new(null_mut(), area.right, area.bottom).unwrap();
            SetDCBrushColor(surface.dc, 0x00d8_d8d8);
            FillRect(surface.dc, &area, GetStockObject(DC_BRUSH));
            let mut style = Style::default();
            style.rect_round = true;
            style.text_look = Look::Fill;
            style.text_align = Align::Center;
            for (index, tool) in [Tool::Rectangle, Tool::Text, Tool::Magnify].into_iter().enumerate() {
                let top = 10 + index as i32 * 52;
                let toolbar = RECT { left: 0, top: top - 50, right: 620, bottom: top - 4 };
                let options = Options::new(tool, toolbar, area, RECT { bottom: 0, ..area }).unwrap();
                options.paint(surface.dc, style, (index == 0).then_some(Choice::Toggle(Flag::Dashed)));
            }
            GdiFlush();
            let mut image = Desktop { bounds: area, surface }.crop(area).unwrap();
            for pixel in image.pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            let folder = std::env::temp_dir().join("snapshot-preview");
            std::fs::create_dir_all(&folder).unwrap();
            image.save_png(&folder.join("options-toggles.png")).unwrap();
        }
    }
}
