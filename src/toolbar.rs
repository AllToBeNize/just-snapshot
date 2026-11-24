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
    /// Recording mode: start recording the selected area.
    Record,
    /// Recording mode: record what the computer plays.
    SystemAudio,
    /// Recording mode: record the microphone.
    Microphone,
    /// Recording mode: show the key and mouse panel inside the area.
    KeysPanel,
    /// Recording mode: show the gamepad panel inside the area.
    PadPanel,
}

/// The toolbar of the recording area picker.
pub const RECORD_ACTIONS: [Action; 6] =
    [Action::Record, Action::SystemAudio, Action::Microphone, Action::KeysPanel, Action::PadPanel, Action::Cancel];
const RECORD_GROUPS: [usize; 3] = [1, 3, 5];
/// The record toolbar's switches, in the order `paint` takes their states.
pub const SWITCHES: [Action; 4] = [Action::SystemAudio, Action::Microphone, Action::KeysPanel, Action::PadPanel];

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
            Binding::Arrow => Self::Tool(Tool::Arrow),
            Binding::Number => Self::Tool(Tool::Number),
            Binding::Magnify => Self::Tool(Tool::Magnify),
            Binding::Select => Self::Tool(Tool::Select),
            Binding::CaptureWindow
            | Binding::LastCapture
            | Binding::Record
            | Binding::RecordPause
            | Binding::RecordScreen
            | Binding::ShowPointer
            | Binding::ShowHints
            | Binding::ShowKeys
            | Binding::ShowPad => return None,
        })
    }

    /// The binding that triggers this action, for tooltips and hints.
    pub fn binding(self) -> Option<Binding> {
        Some(match self {
            Self::Tool(Tool::Rectangle) => Binding::Rectangle,
            Self::Tool(Tool::Text) => Binding::Text,
            Self::Tool(Tool::Brush) => Binding::Brush,
            Self::Tool(Tool::Mosaic) => Binding::Mosaic,
            Self::Tool(Tool::Arrow) => Binding::Arrow,
            Self::Tool(Tool::Number) => Binding::Number,
            Self::Tool(Tool::Magnify) => Binding::Magnify,
            Self::Tool(Tool::Select) => Binding::Select,
            Self::Undo => Binding::Undo,
            Self::Redo => Binding::Redo,
            Self::ScrollCapture => Binding::LongCapture,
            Self::Copy => Binding::Copy,
            Self::Save => Binding::Save,
            Self::Pin => Binding::Pin,
            Self::ExtractText => Binding::ExtractText,
            Self::Translate => Binding::Translate,
            Self::Cancel | Self::Record | Self::SystemAudio | Self::Microphone | Self::KeysPanel | Self::PadPanel => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Record => "开始录制",
            Self::SystemAudio => "录制系统声音",
            Self::Microphone => "录制麦克风",
            Self::KeysPanel => "显示键鼠",
            Self::PadPanel => "显示手柄",
            _ => {
                let index = ACTIONS.iter().position(|&candidate| candidate == self).unwrap_or(0);
                LABEL_TEXT[index]
            }
        }
    }

    /// "矩形  Ctrl+1": the name and its current shortcut.
    pub fn tip(self) -> String {
        let name = self.label();
        let key = match self {
            Self::Record => "Enter".into(),
            Self::SystemAudio | Self::Microphone | Self::KeysPanel | Self::PadPanel => "单击开 / 关".into(),
            _ => match self.binding() {
                Some(binding) => crate::hotkey::current().get(binding).label(),
                None => "Esc".into(),
            },
        };
        format!("{name}  {key}")
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
pub const ACTIONS: [Action; 17] = [
    Action::Tool(Tool::Rectangle),
    Action::Tool(Tool::Text),
    Action::Tool(Tool::Brush),
    Action::Tool(Tool::Mosaic),
    Action::Tool(Tool::Arrow),
    Action::Tool(Tool::Number),
    Action::Tool(Tool::Magnify),
    Action::Tool(Tool::Select),
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
const LABEL_TEXT: [&str; ACTIONS.len()] = [
    "矩形",
    "文本",
    "画笔",
    "马赛克",
    "箭头",
    "序号",
    "放大镜",
    "选择",
    "撤回",
    "重做",
    "提取文本",
    "翻译",
    "长截图",
    "贴图",
    "保存",
    "退出",
    "复制",
];
/// Indexes of the first button in every group after the first.
const GROUP_STARTS: [usize; 3] = [8, 10, 12];
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
    /// The recording area picker's short toolbar.
    record: bool,
}

impl Toolbar {
    /// The monitor area the toolbar keeps inside.
    pub fn area(self) -> RECT {
        self.area
    }

    pub fn options(self, tool: Tool, selection: RECT) -> Option<Options> {
        Options::new(tool, self.bounds, self.area, selection)
    }

    fn actions(self) -> &'static [Action] {
        if self.record { &RECORD_ACTIONS } else { &ACTIONS }
    }

    fn groups(self) -> &'static [usize] {
        if self.record { &RECORD_GROUPS } else { &GROUP_STARTS }
    }

    pub fn new(selection: RECT, area: RECT) -> Option<Self> {
        Self::with(selection, area, false)
    }

    /// The toolbar for picking a recording area.
    pub fn recording(selection: RECT, area: RECT) -> Option<Self> {
        Self::with(selection, area, true)
    }

    fn with(selection: RECT, area: RECT, record: bool) -> Option<Self> {
        let probe = Self { bounds: RECT::default(), cell: 0, area, record };
        let spacing = GROUP_GAP * probe.groups().len() as i32 + PADDING * 2;
        let count = probe.actions().len() as i32;
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
            record,
        })
    }

    fn button(self, index: usize) -> RECT {
        let groups = self.groups().iter().filter(|&&start| index >= start).count() as i32;
        let left = self.bounds.left + PADDING + index as i32 * self.cell + groups * GROUP_GAP;
        RECT {
            left,
            right: left + self.cell - 1,
            top: self.bounds.top + PADDING,
            bottom: self.bounds.bottom - PADDING - SHADOW,
        }
    }

    pub fn hit(self, point: POINT) -> Option<Action> {
        self.actions()
            .iter()
            .copied()
            .enumerate()
            .find_map(|(index, action)| contains(self.button(index), point).then_some(action))
    }

    /// `switches` is whether each of `SWITCHES` is on, for the recording toolbar.
    pub unsafe fn paint(self, dc: HDC, history: &History, hovered: Option<Action>, switches: [bool; 4]) {
        let saved = SaveDC(dc);
        if saved == 0 {
            return;
        }
        paint_panel(dc, self.bounds);

        SetDCBrushColor(dc, theme::palette().divider);
        for &start in self.groups() {
            let divider = self.button(start - 1).right + 1 + GROUP_GAP / 2;
            let line = RECT {
                left: divider,
                right: divider + 1,
                top: self.bounds.top + 12,
                bottom: self.bounds.bottom - 12 - SHADOW,
            };
            FillRect(dc, &line, GetStockObject(DC_BRUSH));
        }

        for (index, action) in self.actions().iter().enumerate() {
            let rect = self.button(index);
            let selected = match SWITCHES.iter().position(|switch| switch == action) {
                Some(index) => switches[index],
                None => history.tool.is_some_and(|tool| *action == Action::Tool(tool)),
            };
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

    #[cfg(test)]
    pub fn tooltip(self, action: Action) -> Option<(RECT, Vec<u16>)> {
        self.tooltip_clear_of(action, &[])
    }

    /// The tooltip above the button, or below the toolbar, kept clear of
    /// the bars stacked against the toolbar (`obstacles`): when both sides
    /// are taken it moves past the whole stack.
    pub fn tooltip_clear_of(self, action: Action, obstacles: &[RECT]) -> Option<(RECT, Vec<u16>)> {
        let index = self.actions().iter().position(|&candidate| candidate == action)?;
        let button = self.button(index);
        let label = crate::wide(&action.tip());
        let width = unsafe { measure_tip(&label) } + 22;
        let height = 28;
        let left = ((button.left + button.right - width) / 2)
            .clamp(self.area.left, self.area.right - width);
        let stack = obstacles.iter().fold(self.bounds, |stack, rect| RECT {
            left: stack.left.min(rect.left),
            top: stack.top.min(rect.top),
            right: stack.right.max(rect.right),
            bottom: stack.bottom.max(rect.bottom),
        });
        let at = |top: i32| RECT { left, top, right: left + width, bottom: top + height };
        let candidates = [
            at(self.bounds.top - height - 6),
            at(self.bounds.bottom + 6),
            at(stack.top - height - 6),
            at(stack.bottom + 6),
        ];
        let inside = |rect: &RECT| rect.top >= self.area.top && rect.bottom <= self.area.bottom;
        let clear = |rect: &RECT| {
            obstacles.iter().all(|obstacle| {
                rect.right <= obstacle.left
                    || obstacle.right <= rect.left
                    || rect.bottom <= obstacle.top
                    || obstacle.bottom <= rect.top
            })
        };
        let bounds = candidates
            .iter()
            .find(|rect| inside(rect) && clear(rect))
            .or_else(|| candidates.iter().find(|rect| inside(rect)))?;
        Some((*bounds, label))
    }
}

/// Width of a tooltip label in the tooltip font, at least wide enough for a short name.
unsafe fn measure_tip(label: &[u16]) -> i32 {
    let dc = GetDC(std::ptr::null_mut());
    let previous = SelectObject(dc, typography::font(TextStyle::Compact));
    let mut size = windows_sys::Win32::Foundation::SIZE::default();
    GetTextExtentPoint32W(dc, label.as_ptr(), label.len().saturating_sub(1) as i32, &mut size);
    SelectObject(dc, previous);
    ReleaseDC(std::ptr::null_mut(), dc);
    size.cx.max(48)
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
        // The record button is red, like every record button.
        (true, false, false) if action == Action::Record => (None, 0x0040_40e8),
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
        Action::Tool(Tool::Arrow) => Icon::Arrow,
        Action::Tool(Tool::Number) => Icon::Number,
        Action::Tool(Tool::Magnify) => Icon::Magnify,
        Action::Tool(Tool::Select) => Icon::Select,
        Action::Undo => Icon::Undo,
        Action::Redo => Icon::Redo,
        Action::ScrollCapture => Icon::LongCapture,
        Action::Copy => Icon::Copy,
        Action::Save => Icon::Save,
        Action::Pin => Icon::Pin,
        Action::ExtractText => Icon::ExtractText,
        Action::Translate => Icon::Translate,
        Action::Cancel => Icon::Close,
        Action::Record => Icon::Record,
        Action::SystemAudio => Icon::Sound,
        Action::Microphone => Icon::Microphone,
        Action::KeysPanel => Icon::Keyboard,
        Action::PadPanel => Icon::Gamepad,
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
    fn record_tooltip_stays_clear_of_the_panel_bar() {
        let area = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        let apart = |a: RECT, b: RECT| a.right <= b.left || b.right <= a.left || a.bottom <= b.top || b.bottom <= a.top;
        // The whole screen selected: the toolbar sits inside at the bottom and
        // the panel bar goes above it, where the tooltip used to be.
        for selection in [area, RECT { left: 300, top: 100, right: 800, bottom: 590 }, RECT { left: 300, top: 500, right: 800, bottom: 1075 }] {
            let toolbar = Toolbar::recording(selection, area).unwrap();
            let bar = crate::panel_options::PanelBar::new(crate::recorder::Options { show_keys: true, show_pad: true, ..Default::default() }, toolbar.bounds, area, selection).unwrap();
            let (tip, _) = toolbar.tooltip_clear_of(Action::Record, &[bar.bounds]).unwrap();
            assert!(apart(tip, bar.bounds), "tooltip at {} under the panel bar at {}..{}", tip.top, bar.bounds.top, bar.bounds.bottom);
            assert!(apart(tip, toolbar.bounds));
            assert!(tip.top >= area.top && tip.bottom <= area.bottom);
        }
    }

    #[test]
    fn panel_is_centered_and_highlights_only_the_hovered_button() {
        let area = RECT {
            right: 900,
            bottom: 600,
            ..Default::default()
        };
        let selection = RECT {
            left: 200,
            top: 120,
            right: 700,
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
            toolbar.paint(surface.dc, &History::default(), hovered, [false; 4]);
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
                toolbar.paint(surface.dc, &history, Some(Action::Save), [false; 4]);
                if let Some((bounds, label)) = toolbar.tooltip(Action::Save) {
                    paint_tooltip(surface.dc, bounds, label.as_ptr());
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
        history.push(Mark::Rectangle { rect: RECT::default(), width: 3, color: 0, round: false, dashed: false });
        assert!(Action::Undo.enabled(&history));
        assert!(!Action::Redo.enabled(&history));
        history.undo(&mut RECT::default());
        assert!(!Action::Undo.enabled(&history));
        assert!(Action::Redo.enabled(&history));
        history.redo(&mut RECT::default());
        assert!(Action::Undo.enabled(&history));
        assert!(!Action::Redo.enabled(&history));
        history.undo(&mut RECT::default());
        history.push(Mark::Rectangle { rect: RECT::default(), width: 3, color: 0, round: false, dashed: false });
        assert!(!Action::Redo.enabled(&history));
    }
}
