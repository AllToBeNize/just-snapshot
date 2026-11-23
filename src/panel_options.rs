//! The option bar under the recording toolbar for the key and gamepad
//! panels switched on there: one row per panel with the nine places (inside
//! the recording area) and its opacity. It hangs under the toolbar the way
//! the drawing tools' option bar does.

use windows_sys::Win32::{
    Foundation::{POINT, RECT},
    Graphics::Gdi::*,
};

use crate::{
    gdiplus::Graphics,
    input_overlay::{self, Panel, ANCHORS, PANELS},
    selection_geometry::contains,
    theme::{self, argb},
    toolbar::{paint_panel, SHADOW},
    typography::{self, TextStyle},
    ui::to_f,
    wide,
};

const ROW: i32 = 42;
const GAP: i32 = 4;
const PADDING: i32 = 8;
const NAME: i32 = 40;
const CELL: (i32, i32) = (16, 8);
const CELL_GAP: i32 = 3;
const OPACITY_LABEL: i32 = 60;
const BUTTON: i32 = 26;
const VALUE: i32 = 44;
/// Opacity changes by this much per click.
pub const STEP: i32 = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelChoice {
    Anchor(Panel, usize),
    /// Fainter (-1) or stronger (+1).
    Opacity(Panel, i32),
}

#[derive(Clone, Copy)]
pub struct PanelBar {
    pub bounds: RECT,
    /// The panels with a row, top to bottom.
    rows: [Option<Panel>; 2],
}

impl PanelBar {
    /// Under the toolbar (above it when the toolbar sits above or inside the
    /// selection, or there is no room below), for the panels in `shown`.
    pub fn new(shown: [bool; 2], toolbar: RECT, area: RECT, selection: RECT) -> Option<Self> {
        let mut rows = [None; 2];
        for (slot, panel) in rows.iter_mut().zip(PANELS.into_iter().filter(|panel| shown[*panel as usize])) {
            *slot = Some(panel);
        }
        let count = rows.iter().flatten().count() as i32;
        if count == 0 {
            return None;
        }
        let width = Self::width();
        let height = ROW * count + PADDING + SHADOW;
        if width > area.right - area.left {
            return None;
        }
        let center = (toolbar.left + toolbar.right) / 2;
        let left = (center - width / 2).clamp(area.left, area.right - width);
        let below = toolbar.bottom + GAP;
        let above = toolbar.top - GAP - height;
        let fits_below = below + height <= area.bottom;
        let top = if toolbar.top >= selection.bottom && fits_below {
            below
        } else if above >= area.top {
            above
        } else if fits_below {
            below
        } else {
            return None;
        };
        Some(Self { bounds: RECT { left, top, right: left + width, bottom: top + height }, rows })
    }

    fn width() -> i32 {
        let grid = CELL.0 * 3 + CELL_GAP * 2;
        PADDING * 2 + NAME + grid + 16 + OPACITY_LABEL + BUTTON * 2 + VALUE
    }

    /// The row of the `index`th panel.
    fn row(self, index: usize) -> RECT {
        let top = self.bounds.top + PADDING / 2 + ROW * index as i32;
        RECT { left: self.bounds.left + PADDING, top, right: self.bounds.right - PADDING, bottom: top + ROW }
    }

    fn panels(self) -> impl Iterator<Item = (usize, Panel)> {
        self.rows.into_iter().flatten().enumerate()
    }

    /// Every button with what it does.
    fn buttons(self) -> Vec<(RECT, PanelChoice)> {
        let mut buttons = Vec::new();
        for (index, panel) in self.panels() {
            let row = self.row(index);
            let grid_left = row.left + NAME;
            let grid_top = (row.top + row.bottom) / 2 - (CELL.1 * 3 + CELL_GAP * 2) / 2;
            for anchor in 0..9 {
                let (column, line) = ((anchor % 3) as i32, (anchor / 3) as i32);
                let left = grid_left + column * (CELL.0 + CELL_GAP);
                let top = grid_top + line * (CELL.1 + CELL_GAP);
                buttons.push((RECT { left, top, right: left + CELL.0, bottom: top + CELL.1 }, PanelChoice::Anchor(panel, anchor)));
            }
            let minus = row.right - BUTTON * 2 - VALUE;
            let middle = (row.top + row.bottom) / 2;
            let button = |left: i32| RECT { left, top: middle - BUTTON / 2, right: left + BUTTON, bottom: middle + BUTTON / 2 };
            buttons.push((button(minus), PanelChoice::Opacity(panel, -1)));
            buttons.push((button(row.right - BUTTON), PanelChoice::Opacity(panel, 1)));
        }
        buttons
    }

    pub fn hit(self, point: POINT) -> Option<PanelChoice> {
        self.buttons().into_iter().find_map(|(rect, choice)| contains(rect, point).then_some(choice))
    }

    pub unsafe fn paint(self, dc: HDC, hovered: Option<PanelChoice>) {
        let saved = SaveDC(dc);
        if saved == 0 {
            return;
        }
        paint_panel(dc, self.bounds);
        SelectObject(dc, typography::font(TextStyle::HintDescription));
        SetBkMode(dc, TRANSPARENT as i32);
        let palette = theme::palette();
        let config = input_overlay::config();
        for (index, panel) in self.panels() {
            let row = self.row(index);
            let name = if panel == Panel::Keys { "键鼠" } else { "手柄" };
            SetTextColor(dc, palette.text);
            label(dc, RECT { right: row.left + NAME - 6, ..row }, name, DT_LEFT);
            let minus = row.right - BUTTON * 2 - VALUE;
            SetTextColor(dc, palette.secondary);
            label(dc, RECT { left: minus - OPACITY_LABEL, right: minus - 4, ..row }, "不透明度", DT_RIGHT);
            SetTextColor(dc, palette.text);
            let opacity = config.panels[panel as usize].opacity;
            label(dc, RECT { left: minus + BUTTON, right: row.right - BUTTON, ..row }, &format!("{opacity}%"), DT_CENTER);
        }
        if let Some(graphics) = Graphics::new(dc) {
            for (rect, choice) in self.buttons() {
                let hover = hovered == Some(choice);
                match choice {
                    PanelChoice::Anchor(panel, anchor) => {
                        let lit = input_overlay::current_anchor(panel) == Some(ANCHORS[anchor]);
                        let fill = if lit {
                            argb(palette.accent, 255)
                        } else {
                            argb(palette.secondary, if hover { 200 } else { 110 })
                        };
                        graphics.fill_round(to_f(rect), 2.5, fill);
                    }
                    PanelChoice::Opacity(_, step) => {
                        let fill = if hover { palette.hover } else { palette.card };
                        graphics.fill_round(to_f(rect), 6.0, argb(fill, 255));
                        graphics.stroke_round(to_f(rect), 6.0, argb(palette.card_border, 255), 1.0);
                        let (cx, cy) = ((rect.left + rect.right) as f32 / 2.0, (rect.top + rect.bottom) as f32 / 2.0);
                        let ink = argb(palette.text, 255);
                        graphics.line((cx - 5.0, cy), (cx + 5.0, cy), ink, 1.6);
                        if step > 0 {
                            graphics.line((cx, cy - 5.0), (cx, cy + 5.0), ink, 1.6);
                        }
                    }
                }
            }
        }
        RestoreDC(dc, saved);
    }
}

unsafe fn label(dc: HDC, mut rect: RECT, text: &str, align: u32) {
    let text = wide(text);
    DrawTextW(dc, text.as_ptr(), -1, &mut rect, align | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_row_per_shown_panel_and_no_overlapping_buttons() {
        let toolbar = RECT { left: 400, top: 600, right: 700, bottom: 640 };
        let area = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        let selection = RECT { left: 300, top: 100, right: 800, bottom: 590 };
        assert!(PanelBar::new([false, false], toolbar, area, selection).is_none());
        let one = PanelBar::new([false, true], toolbar, area, selection).unwrap();
        let both = PanelBar::new([true, true], toolbar, area, selection).unwrap();
        assert!(both.bounds.bottom - both.bounds.top > one.bounds.bottom - one.bounds.top);
        assert_eq!(one.bounds.top, toolbar.bottom + GAP, "hangs under a toolbar below the selection");
        let buttons = both.buttons();
        assert_eq!(buttons.len(), 2 * (9 + 2));
        for (index, (a, _)) in buttons.iter().enumerate() {
            assert!(contains(both.bounds, POINT { x: (a.left + a.right) / 2, y: (a.top + a.bottom) / 2 }));
            for (b, _) in &buttons[index + 1..] {
                let apart = a.right <= b.left || b.right <= a.left || a.bottom <= b.top || b.bottom <= a.top;
                assert!(apart, "({}, {}) and ({}, {}) overlap", a.left, a.top, b.left, b.top);
            }
        }
        assert_eq!(both.hit(POINT { x: buttons[0].0.left + 1, y: buttons[0].0.top + 1 }), Some(buttons[0].1));
    }

    /// The bar with both rows, hovering one place. Run with `<test binary> panel_options --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        unsafe {
            let toolbar = RECT { left: 0, top: 0, right: 320, bottom: 0 };
            let area = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
            let bar = PanelBar::new([true, true], toolbar, area, RECT::default()).unwrap();
            let (width, height) = (bar.bounds.right + 20, bar.bounds.bottom + 20);
            let surface = crate::capture::Surface::new(std::ptr::null_mut(), width, height).unwrap();
            surface.fill(width, height, 0x0040_4040);
            bar.paint(surface.dc, Some(PanelChoice::Anchor(Panel::Keys, 6)));
            GdiFlush();
            let folder = std::env::temp_dir().join("snapshot-preview");
            std::fs::create_dir_all(&folder).unwrap();
            surface.image(width, height).unwrap().save_png(&folder.join("panel-bar.png")).unwrap();
        }
    }
}
