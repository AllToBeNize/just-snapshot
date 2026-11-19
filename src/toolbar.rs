use crate::{
    annotation::{History, Tool},
    annotation_options::Options,
    hotkey::Binding,
    icons::{self, Icon},
    selection_geometry::contains,
    typography::{self, TextStyle},
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
        })
    }

    pub fn enabled(self, history: &History) -> bool {
        match self {
            Self::Undo => history.draft.is_none() && history.has_marks(),
            Self::Redo => history.draft.is_none() && history.can_redo(),
            _ => true,
        }
    }
}

// Snipaste-style order: drawing tools | history | output, ending with exit and copy.
pub const ACTIONS: [Action; 11] = [
    Action::Tool(Tool::Rectangle),
    Action::Tool(Tool::Text),
    Action::Tool(Tool::Brush),
    Action::Tool(Tool::Mosaic),
    Action::Undo,
    Action::Redo,
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
    w!("长截图"),
    w!("贴图"),
    w!("保存"),
    w!("退出"),
    w!("复制"),
];
/// Indexes of the first button in every group after the first.
const GROUP_STARTS: [usize; 2] = [4, 6];
const OFFSET: i32 = 6;
const GROUP_GAP: i32 = 9;
const PADDING: i32 = 3;
const HEIGHT: i32 = 38;
const MAX_CELL: i32 = 32;
const ICON: i32 = 20;
const DISABLED_ICON: u32 = 0x00b9_b9b9;
const ACCENT_ICON: u32 = 0x00d0_7a1e;
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
        // Below the selection, right-aligned; then above; a full-screen selection keeps it inside.
        let below = selection.bottom + OFFSET;
        let above = selection.top - OFFSET - HEIGHT;
        let (right, top) = if below + HEIGHT <= area.bottom {
            (selection.right, below)
        } else if above >= area.top {
            (selection.right, above)
        } else {
            (
                selection.right - OFFSET,
                (selection.bottom - OFFSET - HEIGHT).clamp(area.top, area.bottom - HEIGHT),
            )
        };
        let left = (right - width).clamp(area.left, area.right - width);
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

        SetDCBrushColor(dc, 0x00e4_e4e4);
        for start in GROUP_STARTS {
            let divider = self.button(start - 1).right + 1 + GROUP_GAP / 2;
            let line = RECT {
                left: divider,
                right: divider + 1,
                top: self.bounds.top + 9,
                bottom: self.bounds.bottom - 9,
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
        let width = 66;
        let height = 24;
        let left = ((button.left + button.right - width) / 2)
            .clamp(self.area.left, self.area.right - width);
        let above = self.bounds.top - height - 4;
        let top = if above >= self.area.top {
            above
        } else if self.bounds.bottom + 4 + height <= self.area.bottom {
            self.bounds.bottom + 4
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
    SelectObject(dc, GetStockObject(DC_BRUSH));
    SelectObject(dc, GetStockObject(DC_PEN));
    // A soft two-step shadow inside the bottom of the bounds, so repaints stay in place.
    let face = RECT {
        bottom: bounds.bottom - SHADOW,
        ..bounds
    };
    for (offset, color) in [(SHADOW, 0x00c8_c8c8), (1, 0x00b4_b4b4)] {
        SetDCBrushColor(dc, color);
        SetDCPenColor(dc, color);
        let shadow = RECT {
            left: face.left + 1,
            top: face.top + offset,
            right: face.right - 1,
            bottom: face.bottom + offset,
        };
        rounded(dc, shadow, 8);
    }
    SetDCBrushColor(dc, 0x00ff_ffff);
    SetDCPenColor(dc, 0x00d4_d4d4);
    rounded(dc, face, 8);
}

pub unsafe fn paint_button(
    dc: HDC,
    rect: RECT,
    action: Action,
    enabled: bool,
    selected: bool,
    hovered: bool,
) {
    let (background, foreground) = match (enabled, selected, hovered) {
        (false, _, _) => (None, DISABLED_ICON),
        (true, true, _) => (Some(0x00fc_eee4), 0x008b_4c22),
        (true, false, true) => (Some(0x00f8_f1eb), 0x0044_3328),
        (true, false, false) if action == Action::Copy => (None, ACCENT_ICON),
        (true, false, false) => (None, 0x0050_4945),
    };
    if let Some(background) = background {
        SelectObject(dc, GetStockObject(DC_BRUSH));
        SelectObject(dc, GetStockObject(NULL_PEN));
        SetDCBrushColor(dc, background);
        rounded(dc, rect, 6);
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
    paint_label(dc, bounds, label, 0x0034_302d, true);
}

pub unsafe fn paint_size_badge(dc: HDC, bounds: RECT, label: *const u16) {
    paint_label(dc, bounds, label, 0, false);
}

unsafe fn paint_label(dc: HDC, mut bounds: RECT, label: *const u16, fill: u32, pill: bool) {
    let saved = SaveDC(dc);
    if saved == 0 {
        return;
    }
    let brush = GetStockObject(DC_BRUSH);
    SetDCBrushColor(dc, fill);
    if pill {
        SelectObject(dc, brush);
        SelectObject(dc, GetStockObject(NULL_PEN));
        rounded(dc, bounds, 7);
    } else {
        FillRect(dc, &bounds, brush);
    }
    let style = if pill {
        TextStyle::Compact
    } else {
        TextStyle::Dimensions
    };
    SelectObject(dc, typography::font(style));
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, 0x00ff_ffff);
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
    fn panel_is_right_aligned_and_highlights_only_the_hovered_button() {
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
        assert_eq!(toolbar.bounds.right, selection.right);
        assert_eq!(toolbar.bounds.top, selection.bottom + OFFSET);
        let first = toolbar.button(0);
        let point = POINT {
            x: first.left + 9,
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
            assert_eq!(GetPixel(surface.dc, point.x, point.y), 0x00f8_f1eb);
            assert_eq!(
                GetPixel(surface.dc, point.x + toolbar.cell, point.y),
                0x00ff_ffff
            );
            assert_eq!(
                GetPixel(surface.dc, divider, toolbar.bounds.top + 12),
                0x00e4_e4e4
            );
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
        assert_eq!(toolbar.bounds.right, low.right);
        let toolbar = Toolbar::new(area, area).unwrap();
        assert_eq!(toolbar.bounds.bottom, area.bottom - OFFSET);
        assert_eq!(toolbar.bounds.right, area.right - OFFSET);
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
        history.undo();
        assert!(!Action::Undo.enabled(&history));
        assert!(Action::Redo.enabled(&history));
        history.redo();
        assert!(Action::Undo.enabled(&history));
        assert!(!Action::Redo.enabled(&history));
        history.undo();
        history.push(Mark::Rectangle(RECT::default()));
        assert!(!Action::Redo.enabled(&history));
    }
}
