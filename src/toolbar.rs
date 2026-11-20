use crate::{
    annotation::{History, Tool},
    annotation_options::Options,
    hotkey::Binding,
    icons::{self, Icon},
    selection_geometry::contains,
    gdiplus::Graphics,
    theme::{self, argb},
    typography::{self, TextStyle},
    ui::{inset, to_f},
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{POINT, RECT},
        Graphics::Gdi::*,
    },
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Tool(Tool),
    Undo,
    Redo,
    ScrollCapture,
    Copy,
    Save,
    Pin,
    ExtractText,
    Translate,
    Cancel,
}

impl Action {
    pub fn from_binding(binding: Binding) -> Option<Self> {
        Some(match binding {
            Binding::Capture => return None,
            Binding::Rectangle => Self::Tool(Tool::Rectangle),
            Binding::Text => Self::Tool(Tool::Text),
            Binding::Brush => Self::Tool(Tool::Brush),
            Binding::Mosaic => Self::Tool(Tool::Mosaic),
            Binding::Undo => Self::Undo,
            Binding::Redo => Self::Redo,
            Binding::LongCapture => Self::ScrollCapture,
            Binding::Copy => Self::Copy,
            Binding::Save => Self::Save,
            Binding::Pin => Self::Pin,
            Binding::ExtractText => Self::ExtractText,
            Binding::Translate => Self::Translate,
        })
    }

    pub fn enabled(self, history: &History) -> bool {
        match self {
            Self::Undo => history.draft.is_none() && history.can_undo(),
            Self::Redo => history.draft.is_none() && history.can_redo(),
            _ => true,
        }
    }
}

// Snipaste-style order: drawing tools | history | text | output, ending with exit and copy.
pub const ACTIONS: [Action; 13] = [
    Action::Tool(Tool::Rectangle),
    Action::Tool(Tool::Text),
    Action::Tool(Tool::Brush),
    Action::Tool(Tool::Mosaic),
    Action::Undo,
    Action::Redo,
    Action::ExtractText,
    Action::Translate,
    Action::ScrollCapture,
    Action::Pin,
    Action::Save,
    Action::Cancel,
    Action::Copy,
];
const LABELS: [*const u16; ACTIONS.len()] = [
    w!("矩形"),
    w!("文本"),
    w!("画笔"),
    w!("马赛克"),
    w!("撤回"),
    w!("重做"),
    w!("提取文本"),
    w!("翻译"),
    w!("长截图"),
    w!("贴图"),
    w!("保存"),
    w!("退出"),
    w!("复制"),
];
/// Indexes of the first button in every group after the first.
const GROUP_STARTS: [usize; 3] = [4, 6, 8];
const OFFSET: i32 = 8;
const GROUP_GAP: i32 = 11;
const PADDING: i32 = 4;
const HEIGHT: i32 = 46;
const MAX_CELL: i32 = 38;
/// Lucide's native size, so strokes land on whole pixels.
const ICON: i32 = 24;
const RADIUS: f32 = 10.0;
/// Height of the panel shadow, included at the bottom of every panel's bounds.
pub const SHADOW: i32 = 2;

#[derive(Clone, Copy)]
pub struct Toolbar {
    pub bounds: RECT,
    cell: i32,
    area: RECT,
}

impl Toolbar {
    pub fn options(self, tool: Tool, selection: RECT) -> Option<Options> {
        Options::new(tool, self.bounds, self.area, selection)
    }

    pub fn new(selection: RECT, area: RECT) -> Option<Self> {
        let spacing = GROUP_GAP * GROUP_STARTS.len() as i32 + PADDING * 2;
        let count = ACTIONS.len() as i32;
        let cell = ((area.right - area.left - spacing) / count).min(MAX_CELL);
        if cell < 12 || area.bottom - area.top < HEIGHT {
            return None;
        }
        let width = cell * count + spacing;
        // Centered below the selection; then above; a full-screen selection keeps it inside.
        let below = selection.bottom + OFFSET;
        let above = selection.top - OFFSET - HEIGHT;
        let top = if below + HEIGHT <= area.bottom {
            below
        } else if above >= area.top {
            above
        } else {
            (selection.bottom - OFFSET - HEIGHT).clamp(area.top, area.bottom - HEIGHT)
        };
        let center = (selection.left + selection.right) / 2;
        let left = (center - width / 2).clamp(area.left, area.right - width);
        Some(Self {
            bounds: RECT {
                left,
                top,
                right: left + width,
                bottom: top + HEIGHT,
            },
            cell,
            area,
        })
    }

    fn button(self, index: usize) -> RECT {
        let groups = GROUP_STARTS.iter().filter(|&&start| index >= start).count() as i32;
        let left = self.bounds.left + PADDING + index as i32 * self.cell + groups * GROUP_GAP;
        RECT {
            left,
            right: left + self.cell - 1,
            top: self.bounds.top + PADDING,
            bottom: self.bounds.bottom - PADDING - SHADOW,
        }
    }

    pub fn hit(self, point: POINT) -> Option<Action> {
        ACTIONS
            .iter()
            .copied()
            .enumerate()
            .find_map(|(index, action)| contains(self.button(index), point).then_some(action))
    }

    pub unsafe fn paint(self, dc: HDC, history: &History, hovered: Option<Action>) {
        let saved = SaveDC(dc);
        if saved == 0 {
            return;
        }
        paint_panel(dc, self.bounds);

        SetDCBrushColor(dc, theme::palette().divider);
        for start in GROUP_STARTS {
            let divider = self.button(start - 1).right + 1 + GROUP_GAP / 2;
            let line = RECT {
                left: divider,
                right: divider + 1,
                top: self.bounds.top + 12,
                bottom: self.bounds.bottom - 12 - SHADOW,
            };
            FillRect(dc, &line, GetStockObject(DC_BRUSH));
        }

        for (index, action) in ACTIONS.iter().enumerate() {
            let rect = self.button(index);
            let selected = history
                .tool
                .is_some_and(|tool| *action == Action::Tool(tool));
            paint_button(
                dc,
                rect,
                *action,
                action.enabled(history),
                selected,
                hovered == Some(*action),
            );
        }
        RestoreDC(dc, saved);
    }

    pub fn tooltip(self, action: Action) -> Option<(RECT, *const u16)> {
        let index = ACTIONS.iter().position(|&candidate| candidate == action)?;
        let button = self.button(index);
        let width = 70;
        let height = 28;
        let left = ((button.left + button.right - width) / 2)
            .clamp(self.area.left, self.area.right - width);
        let above = self.bounds.top - height - 6;
        let top = if above >= self.area.top {
            above
        } else if self.bounds.bottom + 6 + height <= self.area.bottom {
            self.bounds.bottom + 6
        } else {
            return None;
        };
        Some((
            RECT {
                left,
                top,
                right: left + width,
                bottom: top + height,
            },
            LABELS[index],
        ))
    }
}

pub unsafe fn paint_panel(dc: HDC, bounds: RECT) {
    let palette = theme::palette();
    // The shadow sits inside the bottom of the bounds, so repaints stay in place.
    let face = RECT {
        bottom: bounds.bottom - SHADOW,
        ..bounds
    };
    let Some(graphics) = Graphics::new(dc) else {
        SelectObject(dc, GetStockObject(DC_BRUSH));
        SelectObject(dc, GetStockObject(DC_PEN));
        SetDCBrushColor(dc, palette.card);
        SetDCPenColor(dc, palette.card_border);
        rounded(dc, face, RADIUS as i32);
        return;
    };
    let shape = to_f(face);
    for (offset, alpha) in [(SHADOW as f32, 60), (1.0, 90)] {
        graphics.fill_round(
            [shape[0], shape[1] + offset, shape[2], shape[3] + offset],
            RADIUS,
            argb(palette.shadow, alpha),
        );
    }
    graphics.fill_round(shape, RADIUS, argb(palette.card, 255));
    graphics.stroke_round(inset(shape, 0.5), RADIUS, argb(palette.card_border, 255), 1.0);
}

pub unsafe fn paint_button(
    dc: HDC,
    rect: RECT,
    action: Action,
    enabled: bool,
    selected: bool,
    hovered: bool,
) {
    let palette = theme::palette();
    let (background, foreground) = match (enabled, selected, hovered) {
        (false, _, _) => (None, palette.disabled),
        (true, true, _) => (Some(palette.accent_soft), palette.accent),
        (true, false, true) => (Some(palette.hover), palette.text),
        (true, false, false) if action == Action::Copy => (None, palette.accent),
        (true, false, false) => (None, palette.icon),
    };
    if let Some(background) = background {
        match Graphics::new(dc) {
            Some(graphics) => graphics.fill_round(to_f(rect), 8.0, argb(background, 255)),
            None => {
                SelectObject(dc, GetStockObject(DC_BRUSH));
                SelectObject(dc, GetStockObject(NULL_PEN));
                SetDCBrushColor(dc, background);
                rounded(dc, rect, 8);
            }
        }
    }
    let size = (rect.right - rect.left - 4).min(ICON);
    let left = (rect.left + rect.right - size) / 2;
    let top = (rect.top + rect.bottom - size) / 2;
    icons::paint(
        dc,
        RECT {
            left,
            top,
            right: left + size,
            bottom: top + size,
        },
        action_icon(action),
        foreground,
    );
}

pub unsafe fn paint_tooltip(dc: HDC, bounds: RECT, label: *const u16) {
    let palette = theme::palette();
    paint_label(dc, bounds, label, palette.tooltip, palette.tooltip_text, true);
}

pub unsafe fn paint_size_badge(dc: HDC, bounds: RECT, label: *const u16) {
    paint_label(dc, bounds, label, 0, 0x00ff_ffff, false);
}

unsafe fn paint_label(
    dc: HDC,
    mut bounds: RECT,
    label: *const u16,
    fill: u32,
    text: u32,
    pill: bool,
) {
    if pill {
        if let Some(graphics) = Graphics::new(dc) {
            graphics.fill_round(to_f(bounds), 7.0, argb(fill, 240));
        }
    }
    let saved = SaveDC(dc);
    if saved == 0 {
        return;
    }
    let brush = GetStockObject(DC_BRUSH);
    SetDCBrushColor(dc, fill);
    if !pill {
        FillRect(dc, &bounds, brush);
    }
    let style = if pill {
        TextStyle::Compact
    } else {
        TextStyle::Dimensions
    };
    SelectObject(dc, typography::font(style));
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, text);
    DrawTextW(
        dc,
        label,
        -1,
        &mut bounds,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    RestoreDC(dc, saved);
}

fn action_icon(action: Action) -> Icon {
    match action {
        Action::Tool(Tool::Rectangle) => Icon::Rectangle,
        Action::Tool(Tool::Text) => Icon::Text,
        Action::Tool(Tool::Brush) => Icon::Brush,
        Action::Tool(Tool::Mosaic) => Icon::Mosaic,
        Action::Undo => Icon::Undo,
        Action::Redo => Icon::Redo,
        Action::ScrollCapture => Icon::LongCapture,
        Action::Copy => Icon::Copy,
        Action::Save => Icon::Save,
        Action::Pin => Icon::Pin,
        Action::ExtractText => Icon::ExtractText,
        Action::Translate => Icon::Translate,
        Action::Cancel => Icon::Close,
    }
}

unsafe fn rounded(dc: HDC, rect: RECT, diameter: i32) {
    RoundRect(
        dc,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        diameter,
        diameter,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Surface;
    use std::ptr::null_mut;

    #[test]
    fn panel_is_centered_and_highlights_only_the_hovered_button() {
        let area = RECT {
            right: 900,
            bottom: 600,
            ..Default::default()
        };
        let selection = RECT {
            left: 380,
            top: 120,
            right: 820,
            bottom: 390,
        };
        let toolbar = Toolbar::new(selection, area).unwrap();
        let center = |rect: RECT| (rect.left + rect.right) / 2;
        assert!((center(toolbar.bounds) - center(selection)).abs() <= 1, "centered");
        assert_eq!(toolbar.bounds.top, selection.bottom + OFFSET);
        let first = toolbar.button(0);
        // The inner margin of the button, clear of its icon.
        let point = POINT {
            x: first.left + 3,
            y: first.top + 3,
        };
        let hovered = toolbar.hit(point);
        assert_eq!(hovered.map(|action| action == ACTIONS[0]), Some(true));
        let divider = toolbar.button(GROUP_STARTS[0] - 1).right + 1 + GROUP_GAP / 2;
        assert!(toolbar
            .hit(POINT {
                x: divider,
                y: point.y
            })
            .is_none());
        unsafe {
            let surface = Surface::new(null_mut(), area.right, area.bottom).unwrap();
            toolbar.paint(surface.dc, &History::default(), hovered);
            let palette = theme::palette();
            assert_eq!(GetPixel(surface.dc, point.x, point.y + 6), palette.hover);
            assert_eq!(
                GetPixel(surface.dc, point.x + toolbar.cell, point.y + 6),
                palette.card
            );
            assert_eq!(
                GetPixel(surface.dc, divider, toolbar.bounds.top + 20),
                palette.divider
            );
        }
    }

    /// Renders the toolbar and brush options in every theme over a fake
    /// screenshot to `%TEMP%\\snapshot-preview`. Run with `preview --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        use crate::{annotation::Style, capture::Desktop, theme::THEMES};
        let folder = std::env::temp_dir().join("snapshot-preview");
        std::fs::create_dir_all(&folder).unwrap();
        let area = RECT {
            right: 620,
            bottom: 210,
            ..Default::default()
        };
        let selection = RECT {
            left: 40,
            top: 10,
            right: 580,
            bottom: 90,
        };
        unsafe {
            for theme in THEMES {
                theme::set(theme);
                let surface = Surface::new(null_mut(), area.right, area.bottom).unwrap();
                // A light "screenshot" outside, the selection on top of it.
                for y in 0..area.bottom {
                    let shade = 0x00d8_d8d8 + ((y as u32 / 6) % 2) * 0x0006_0606;
                    SetDCBrushColor(surface.dc, shade);
                    FillRect(
                        surface.dc,
                        &RECT {
                            top: y,
                            bottom: y + 1,
                            ..area
                        },
                        GetStockObject(DC_BRUSH),
                    );
                }
                SetDCBrushColor(surface.dc, 0x00ff_ffff);
                FillRect(surface.dc, &selection, GetStockObject(DC_BRUSH));
                let toolbar = Toolbar::new(selection, area).unwrap();
                let mut history = History::default();
                history.tool = Some(Tool::Brush);
                toolbar.paint(surface.dc, &history, Some(Action::Save));
                if let Some((bounds, label)) = toolbar.tooltip(Action::Save) {
                    paint_tooltip(surface.dc, bounds, label);
                }
                let options = toolbar.options(Tool::Brush, selection).unwrap();
                options.paint(surface.dc, Style::default(), None);
                GdiFlush();
                let mut image = Desktop {
                    bounds: area,
                    surface,
                }
                .crop(area)
                .unwrap();
                for pixel in image.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                image
                    .save_png(&folder.join(format!("toolbar-{}.png", theme.key())))
                    .unwrap();
            }
            theme::set(theme::Theme::default());
        }
    }

    #[test]
    fn panel_flips_above_and_falls_back_inside_a_full_screen_selection() {
        let area = RECT {
            right: 900,
            bottom: 600,
            ..Default::default()
        };
        let low = RECT {
            left: 100,
            top: 200,
            right: 800,
            bottom: 590,
        };
        let toolbar = Toolbar::new(low, area).unwrap();
        assert_eq!(toolbar.bounds.bottom, low.top - OFFSET);
        let toolbar = Toolbar::new(area, area).unwrap();
        assert_eq!(toolbar.bounds.bottom, area.bottom - OFFSET);
        assert!(toolbar.bounds.left > area.left && toolbar.bounds.right < area.right);
    }

    #[test]
    fn undo_and_redo_follow_the_annotation_history() {
        use crate::annotation::Mark;

        let mut history = History::default();
        assert!(!Action::Undo.enabled(&history));
        assert!(!Action::Redo.enabled(&history));
        history.push(Mark::Rectangle(RECT::default()));
        assert!(Action::Undo.enabled(&history));
        assert!(!Action::Redo.enabled(&history));
        history.undo(&mut RECT::default());
        assert!(!Action::Undo.enabled(&history));
        assert!(Action::Redo.enabled(&history));
        history.redo(&mut RECT::default());
        assert!(Action::Undo.enabled(&history));
        assert!(!Action::Redo.enabled(&history));
        history.undo(&mut RECT::default());
        history.push(Mark::Rectangle(RECT::default()));
        assert!(!Action::Redo.enabled(&history));
    }
}
