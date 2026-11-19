use windows_sys::{
    core::w,
    Win32::{
        Foundation::{POINT, RECT},
        Graphics::Gdi::*,
    },
};

use crate::{
    annotation::{Style, Tool, INK},
    selection_geometry::contains,
    text_render::Graphics,
    theme::{self, argb},
    toolbar::{paint_panel, SHADOW},
    typography::{self, TextStyle},
    wide,
};

const HEIGHT: i32 = 40;
const GAP: i32 = 4;
const PADDING: i32 = 5;
const FIELD: i32 = 104;
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
            (Tool::Text, Self::Color(color)) => style.text_color = color,
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
        let available = area.right - area.left;
        let swatch = ((available - FIELD - PADDING * 2) / COLORS.len() as i32).min(26);
        let width = match tool {
            Tool::Rectangle => return None,
            Tool::Mosaic => FIELD * 2,
            _ if swatch >= 12 => FIELD + swatch * COLORS.len() as i32,
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
        let colors = COLORS.into_iter().enumerate().map(move |(index, color)| {
            (
                self.cell(FIELD + self.swatch * index as i32, self.swatch),
                Choice::Color(color),
            )
        });
        let mosaic = self.tool == Tool::Mosaic;
        primary
            .into_iter()
            .chain(block.into_iter().filter(move |_| mosaic))
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
            Tool::Rectangle => unreachable!(),
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
                Choice::Primary(step) | Choice::Block(step) => {
                    if hover {
                        if let Some(graphics) = &graphics {
                            let bounds = [rect.left as f32, rect.top as f32, rect.right as f32, rect.bottom as f32];
                            graphics.fill_round(bounds, 6.0, argb(palette.hover, 255));
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

unsafe fn label(dc: HDC, mut rect: RECT, text: *const u16) {
    DrawTextW(
        dc,
        text,
        -1,
        &mut rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
}
