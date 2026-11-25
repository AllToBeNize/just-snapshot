use std::{io, mem::size_of};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{POINT, RECT},
        Graphics::Gdi::*,
    },
};

use crate::{
    annotation::Tool,
    theme::{self, mix},
    capture::Surface,
    hotkey::{self, Binding},
    icons::{self, Icon},
    selection_geometry::intersect,
    typography::{self, TextStyle},
};

const PANEL_WIDTH: i32 = 900;
const ROW_HEIGHT: i32 = 25;
const PANEL_INSET: i32 = 8;
/// Room for the longest column (nine rows) under its title.
const PANEL_HEIGHT: i32 = 10 * ROW_HEIGHT + TITLE_HEIGHT + 2 * PANEL_INSET;
const TITLE_HEIGHT: i32 = 22;
const SECTION_GAP: i32 = 20;
const PANEL_MARGIN: i32 = 12;
const KEYCAP_HEIGHT: i32 = 18;
const KEYCAP_GAP: i32 = 3;
const COLUMN_GAP: i32 = 11;
const ZOOM: i32 = 5;
const SAMPLE_WIDTH: i32 = 33;
const SAMPLE_HEIGHT: i32 = 25;
const BORDER: i32 = 2;
const LOUPE_WIDTH: i32 = SAMPLE_WIDTH * ZOOM + BORDER * 2;
/// The strip under the magnified pixels with the color under the pointer.
const COLOR_STRIP: i32 = 24;
const LOUPE_HEIGHT: i32 = SAMPLE_HEIGHT * ZOOM + BORDER * 2 + COLOR_STRIP;
const POINTER_GAP: i32 = 18;

pub unsafe fn monitor_info(cursor: POINT) -> Option<MONITORINFO> {
    let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    (GetMonitorInfoW(monitor, &mut info) != 0).then_some(info)
}

pub struct HintPanel {
    pub bounds: RECT,
    surface: Surface,
    max_width: i32,
    max_height: i32,
    bottom: i32,
    mode: Option<HintMode>,
    /// Folded to one line that says how to bring the panel back.
    hidden: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HintMode {
    Automatic,
    Selected,
    Annotate(Tool),
    SelectedText,
    /// A drawn shape picked up again.
    SelectedMark,
    /// A locked area to record.
    Record,
    EditText,
}

/// Every toolbar action with its binding, grouped like the toolbar.
const DRAWING: [HintRow; 10] = [
    (HintInput::Bound(Binding::Rectangle), w!("矩形")),
    (HintInput::Bound(Binding::Text), w!("文本")),
    (HintInput::Bound(Binding::Brush), w!("画笔")),
    (HintInput::Bound(Binding::Mosaic), w!("马赛克")),
    (HintInput::Bound(Binding::Arrow), w!("箭头")),
    (HintInput::Bound(Binding::Number), w!("序号")),
    (HintInput::Bound(Binding::Magnify), w!("放大镜")),
    (HintInput::Bound(Binding::Select), w!("选择")),
    (HintInput::Bound(Binding::Undo), w!("撤回")),
    (HintInput::Bound(Binding::Redo), w!("重做")),
];
const OUTPUT: [HintRow; 8] = [
    (HintInput::Bound(Binding::ExtractText), w!("提取文本")),
    (HintInput::Bound(Binding::Translate), w!("翻译")),
    (HintInput::Bound(Binding::SearchImage), w!("识图")),
    (HintInput::Bound(Binding::LongCapture), w!("长截图")),
    (HintInput::Bound(Binding::Pin), w!("贴图")),
    (HintInput::Bound(Binding::Save), w!("保存")),
    (HintInput::Bound(Binding::Copy), w!("复制")),
    (HintInput::Key("Enter"), w!("复制")),
];

/// A titled column of the hint panel.
type Section = (*const u16, Vec<HintRow>);

impl HintMode {
    /// Everything that works right now: what this mode adds first, then
    /// the toolbar's shortcuts whenever the toolbar is up.
    fn sections(self) -> Vec<Section> {
        use HintInput::{Key, Mouse, MouseOrKey};
        let toolbar = || [(w!("标注"), DRAWING.to_vec()), (w!("输出"), OUTPUT.to_vec())];
        let drawn = (Mouse(Icon::Move), w!("拖动已画的标注调整"));
        let wheel = (Key("滚轮"), w!("调整粗细 / 大小"));
        let leave = (Mouse(Icon::MouseRight), w!("退出当前工具"));
        let tool = |rows: &[HintRow]| {
            let mut rows = rows.to_vec();
            rows.extend([drawn, wheel, leave, (Key("Esc"), w!("退出截图"))]);
            std::iter::once((w!("当前工具"), rows)).chain(toolbar()).collect()
        };
        match self {
            Self::Automatic => vec![(
                w!("选区"),
                vec![
                    (Mouse(Icon::MouseLeft), w!("锁定自动选区")),
                    (Mouse(Icon::Drag), w!("手动拖出选区")),
                    (Key("侧键"), w!("切换选区层级（滚轮也行）")),
                    (Key("Ctrl+A"), w!("全选当前屏幕")),
                    (Key("C"), w!("拖动时复制颜色")),
                    (MouseOrKey(Icon::MouseRight, "Esc"), w!("退出截图")),
                ],
            )],
            Self::Selected => std::iter::once((
                w!("选区"),
                vec![
                    (Mouse(Icon::Move), w!("拖动框内移动")),
                    (Mouse(Icon::Resize), w!("拖动边线缩放")),
                    (Key("方向键"), w!("移动 1 像素")),
                    (Key("Shift+方向键"), w!("移动 10 像素")),
                    (Key("Ctrl+方向键"), w!("调整大小")),
                    (Mouse(Icon::MouseLeft), w!("单击尺寸输入宽高")),
                    (Key("C"), w!("复制鼠标处颜色")),
                    (HintInput::Bound(Binding::ShowPointer), w!("显示 / 隐藏指针")),
                    (Mouse(Icon::MouseRight), w!("返回自动框选")),
                    (Key("Esc"), w!("退出截图")),
                ],
            ))
            .chain(toolbar())
            .collect(),
            Self::Annotate(Tool::Rectangle) => tool(&[(Mouse(Icon::Drag), w!("绘制矩形"))]),
            Self::Annotate(Tool::Text) => tool(&[
                (Mouse(Icon::MouseLeft), w!("插入文本框")),
                (Mouse(Icon::Drag), w!("指定文本框大小")),
            ]),
            Self::Annotate(Tool::Brush) => tool(&[(Mouse(Icon::Drag), w!("自由绘制"))]),
            Self::Annotate(Tool::Mosaic) => tool(&[(Mouse(Icon::Drag), w!("涂抹马赛克"))]),
            Self::Annotate(Tool::Arrow) => tool(&[(Mouse(Icon::Drag), w!("从起点拖到箭头指向"))]),
            Self::Annotate(Tool::Number) => tool(&[(Mouse(Icon::MouseLeft), w!("放下下一个序号"))]),
            Self::Annotate(Tool::Magnify) => tool(&[
                (Mouse(Icon::Drag), w!("从中心拖出放大范围")),
                (Mouse(Icon::MouseLeft), w!("单击放大默认范围")),
            ]),
            Self::Annotate(Tool::Select) => std::iter::once((
                w!("选择"),
                vec![
                    (Mouse(Icon::MouseLeft), w!("单击选中标注")),
                    (Key("Ctrl"), w!("按住单击加选 / 取消")),
                    (Mouse(Icon::Drag), w!("拖出方框框选")),
                    (Mouse(Icon::Move), w!("拖动选中的一起移动")),
                    (Key("方向键"), w!("一起移动")),
                    (Key("Delete"), w!("删除选中的标注")),
                    (MouseOrKey(Icon::MouseRight, "Esc"), w!("取消选中")),
                ],
            ))
            .chain(toolbar())
            .collect(),
            Self::Record => vec![(
                w!("录像区域"),
                vec![
                    (Mouse(Icon::Move), w!("拖动框内移动")),
                    (Mouse(Icon::Resize), w!("拖动边线缩放")),
                    (Key("方向键"), w!("微调位置")),
                    (Mouse(Icon::MouseLeft), w!("单击尺寸输入宽高")),
                    (Key("Enter"), w!("开始录制")),
                    (Mouse(Icon::MouseRight), w!("返回自动框选")),
                    (Key("Esc"), w!("退出")),
                ],
            )],
            Self::SelectedMark => std::iter::once((
                w!("选中的标注"),
                vec![
                    (Mouse(Icon::Move), w!("拖动标注移动")),
                    (Mouse(Icon::Resize), w!("拖动控制点调整")),
                    (Key("方向键"), w!("移动标注")),
                    wheel,
                    (Key("Delete"), w!("删除标注")),
                    (MouseOrKey(Icon::MouseRight, "Esc"), w!("取消选中")),
                ],
            ))
            .chain(toolbar())
            .collect(),
            Self::SelectedText => std::iter::once((
                w!("选中的文本框"),
                vec![
                    (Mouse(Icon::MouseLeft), w!("双击文本框编辑")),
                    (Mouse(Icon::Move), w!("拖动文本框移动")),
                    (Mouse(Icon::Resize), w!("拖动四角缩放")),
                    (Mouse(Icon::Drag), w!("拖动旋转手柄")),
                    (Key("Delete"), w!("删除文本框")),
                ],
            ))
            .chain(toolbar())
            .collect(),
            Self::EditText => vec![(
                w!("输入文字"),
                vec![
                    (Key("Enter"), w!("换行")),
                    (Key("Ctrl+Enter"), w!("确认文本")),
                    (Key("Esc"), w!("取消文本输入")),
                ],
            )],
        }
    }
}

#[derive(Clone, Copy)]
enum HintInput {
    Mouse(Icon),
    MouseOrKey(Icon, &'static str),
    Key(&'static str),
    Bound(Binding),
}

type HintRow = (HintInput, *const u16);

enum Keycap {
    Mouse(Icon),
    Text(Vec<u16>),
    Separator,
}

impl HintInput {
    fn keycaps(self, bindings: hotkey::Bindings) -> Vec<Keycap> {
        let label;
        let chord = match self {
            Self::Mouse(icon) => return vec![Keycap::Mouse(icon)],
            Self::MouseOrKey(icon, key) => {
                return vec![
                    Keycap::Mouse(icon),
                    Keycap::Separator,
                    Keycap::Text(crate::wide(key)),
                ];
            }
            Self::Key(chord) => chord,
            Self::Bound(binding) => {
                label = bindings.get(binding).label();
                &label
            }
        };
        chord
            .split('+')
            .map(|part| Keycap::Text(crate::wide(part)))
            .collect()
    }
}

impl Keycap {
    unsafe fn width(&self, dc: HDC) -> i32 {
        match self {
            Self::Mouse(_) => KEYCAP_HEIGHT,
            Self::Text(text) => (text_width(dc, text.as_ptr()) + 8).max(KEYCAP_HEIGHT),
            Self::Separator => 6,
        }
    }

    unsafe fn paint(&self, dc: HDC, rect: RECT) {
        let palette = theme::palette();
        if !matches!(self, Self::Separator) {
            SetDCBrushColor(dc, mix(palette.tooltip, palette.tooltip_text, 0.6));
            FrameRect(dc, &rect, GetStockObject(DC_BRUSH));
        }
        match self {
            Self::Mouse(icon) => icons::paint(
                dc,
                RECT {
                    left: rect.left + 3,
                    top: rect.top + 2,
                    right: rect.right - 3,
                    bottom: rect.bottom - 2,
                },
                *icon,
                palette.tooltip_text,
            ),
            Self::Text(_) | Self::Separator => {
                let mut text_rect = rect;
                SetTextColor(dc, palette.tooltip_text);
                let label = match self {
                    Self::Text(text) => text.as_ptr(),
                    _ => w!("/"),
                };
                DrawTextW(
                    dc,
                    label,
                    -1,
                    &mut text_rect,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
            }
        }
    }
}

unsafe fn text_width(dc: HDC, text: *const u16) -> i32 {
    let mut bounds = RECT::default();
    DrawTextW(
        dc,
        text,
        -1,
        &mut bounds,
        DT_SINGLELINE | DT_CALCRECT | DT_NOPREFIX,
    );
    bounds.right
}

impl HintPanel {
    pub unsafe fn new(source: HDC, work_area: RECT) -> io::Result<Self> {
        let width = PANEL_WIDTH.min(work_area.right - work_area.left);
        let max_height = PANEL_HEIGHT.min(work_area.bottom - work_area.top);
        let left = (work_area.left + PANEL_MARGIN).min(work_area.right - width);
        let bottom = (work_area.bottom - PANEL_MARGIN).max(work_area.top + max_height);
        let surface = Surface::new(source, width, max_height)?;
        let mut hint = Self {
            bounds: RECT {
                left,
                right: left + width,
                top: bottom,
                bottom,
            },
            surface,
            max_width: width,
            max_height,
            bottom,
            mode: None,
            hidden: false,
        };
        hint.set_mode(source, HintMode::Automatic);
        Ok(hint)
    }

    /// Folds the panel to its one-line reminder, or opens it again.
    pub unsafe fn set_hidden(&mut self, source: HDC, hidden: bool) -> Option<[RECT; 2]> {
        if self.hidden == hidden {
            return None;
        }
        self.hidden = hidden;
        let mode = self.mode.take().unwrap_or(HintMode::Automatic);
        self.set_mode(source, mode)
    }

    pub fn hidden(&self) -> bool {
        self.hidden
    }

    pub unsafe fn set_mode(&mut self, source: HDC, mode: HintMode) -> Option<[RECT; 2]> {
        if self.mode == Some(mode) {
            return None;
        }
        let previous = self.bounds;
        self.mode = Some(mode);
        let hidden = self.hidden;
        let title_height = if hidden { 0 } else { TITLE_HEIGHT };
        let sections = if hidden {
            vec![(w!(""), vec![(HintInput::Bound(Binding::ShowHints), w!("显示快捷键提示"))])]
        } else {
            mode.sections()
        };
        let surface = &self.surface;
        let description_font = typography::font(TextStyle::HintDescription);
        let key_font = typography::font(TextStyle::Ui);
        let previous_font = SelectObject(surface.dc, key_font);
        let bindings = hotkey::current();
        // Measure each column: keycaps right-aligned, descriptions after them.
        struct Column {
            title: *const u16,
            lines: Vec<(Vec<(Keycap, i32)>, i32, *const u16)>,
            keys: i32,
            width: i32,
        }
        let mut columns = Vec::new();
        let mut width = PANEL_INSET;
        for (title, rows) in sections {
            SelectObject(surface.dc, key_font);
            let mut keys = 0;
            let lines: Vec<_> = rows
                .iter()
                .map(|&(input, description)| {
                    let caps: Vec<_> = input
                        .keycaps(bindings)
                        .into_iter()
                        .map(|cap| {
                            let width = cap.width(surface.dc);
                            (cap, width)
                        })
                        .collect();
                    let cap_width = caps.iter().map(|(_, width)| width + KEYCAP_GAP).sum::<i32>() - KEYCAP_GAP;
                    keys = keys.max(cap_width);
                    (caps, cap_width, description)
                })
                .collect();
            SelectObject(surface.dc, description_font);
            let descriptions = rows.iter().map(|&(_, description)| text_width(surface.dc, description)).max().unwrap_or(0);
            let column_width = (keys + COLUMN_GAP + descriptions).max(text_width(surface.dc, title));
            let gap = if columns.is_empty() { 0 } else { SECTION_GAP };
            // Columns that do not fit on this screen are left out, last first.
            if width + gap + column_width + PANEL_INSET > self.max_width && !columns.is_empty() {
                break;
            }
            width += gap + column_width;
            columns.push(Column { title, lines, keys, width: column_width });
        }
        // The open panel says how to fold it, right in the last column's title
        // line, so a key pressed by mistake is never a mystery. Typing text
        // takes the letter, so not while editing.
        let fold = (!hidden && mode != HintMode::EditText).then(|| {
            SelectObject(surface.dc, key_font);
            let caps: Vec<_> = HintInput::Bound(Binding::ShowHints)
                .keycaps(bindings)
                .into_iter()
                .map(|cap| {
                    let width = cap.width(surface.dc);
                    (cap, width)
                })
                .collect();
            SelectObject(surface.dc, description_font);
            let label = w!("隐藏提示");
            let label_width = text_width(surface.dc, label);
            (caps, label, label_width)
        });
        if let (Some((caps, _, label_width)), Some(last)) = (&fold, columns.last_mut()) {
            SelectObject(surface.dc, description_font);
            let needed = text_width(surface.dc, last.title)
                + COLUMN_GAP
                + caps.iter().map(|(_, width)| width + KEYCAP_GAP).sum::<i32>()
                + label_width;
            if needed > last.width {
                width += needed - last.width;
                last.width = needed;
            }
        }
        let width = (width + PANEL_INSET).min(self.max_width);
        let rows = columns.iter().map(|column| column.lines.len()).max().unwrap_or(0) as i32;
        let height = (rows * ROW_HEIGHT + title_height + 2 * PANEL_INSET).min(self.max_height);
        self.bounds.top = self.bottom - height;
        self.bounds.right = self.bounds.left + width;
        let panel = RECT {
            right: width,
            bottom: height,
            ..Default::default()
        };
        // A dark HUD in every theme: the theme's tooltip color over the screenshot.
        let palette = theme::palette();
        SetDCBrushColor(surface.dc, palette.tooltip);
        FillRect(surface.dc, &panel, GetStockObject(DC_BRUSH));
        let blend = BLENDFUNCTION {
            SourceConstantAlpha: 51,
            ..Default::default()
        };
        AlphaBlend(
            surface.dc,
            0,
            0,
            width,
            height,
            source,
            self.bounds.left,
            self.bounds.top,
            width,
            height,
            blend,
        );
        let brush = GetStockObject(DC_BRUSH);
        SetDCBrushColor(surface.dc, mix(palette.tooltip, palette.tooltip_text, 0.3));
        FrameRect(surface.dc, &panel, brush);
        SetBkMode(surface.dc, TRANSPARENT as i32);
        let mut left = PANEL_INSET;
        let count = columns.len();
        for (index, column) in columns.into_iter().enumerate() {
            if index > 0 {
                // A faint rule between columns.
                let x = left - SECTION_GAP / 2;
                SetDCBrushColor(surface.dc, mix(palette.tooltip, palette.tooltip_text, 0.18));
                let rule = RECT { left: x, top: PANEL_INSET + 4, right: x + 1, bottom: height - PANEL_INSET - 4 };
                FillRect(surface.dc, &rule, brush);
            }
            SelectObject(surface.dc, description_font);
            SetTextColor(surface.dc, mix(palette.tooltip_text, palette.tooltip, 0.4));
            let mut title = RECT { left, top: PANEL_INSET, right: left + column.width, bottom: PANEL_INSET + TITLE_HEIGHT - 4 };
            DrawTextW(surface.dc, column.title, -1, &mut title, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            if let Some((caps, label, label_width)) = fold.as_ref().filter(|_| index == count - 1) {
                let right = left + column.width;
                let mut text = RECT { left: right - label_width, ..title };
                DrawTextW(surface.dc, *label, -1, &mut text, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                let cap_top = (title.top + title.bottom - KEYCAP_HEIGHT) / 2;
                let mut x = text.left - KEYCAP_GAP - caps.iter().map(|(_, width)| width + KEYCAP_GAP).sum::<i32>() + KEYCAP_GAP;
                SelectObject(surface.dc, key_font);
                for (cap, width) in caps {
                    cap.paint(surface.dc, RECT { left: x, top: cap_top, right: x + width, bottom: cap_top + KEYCAP_HEIGHT });
                    x += width + KEYCAP_GAP;
                }
                SelectObject(surface.dc, description_font);
                SetTextColor(surface.dc, mix(palette.tooltip_text, palette.tooltip, 0.4));
            }
            let split = left + column.keys;
            for (row, (caps, cap_width, description)) in column.lines.into_iter().enumerate() {
                let top = PANEL_INSET + title_height + row as i32 * ROW_HEIGHT;
                if top + ROW_HEIGHT > height {
                    break;
                }
                let cap_top = top + (ROW_HEIGHT - KEYCAP_HEIGHT) / 2;
                let mut x = split - cap_width;
                SelectObject(surface.dc, key_font);
                for (cap, width) in caps {
                    cap.paint(
                        surface.dc,
                        RECT {
                            left: x,
                            top: cap_top,
                            right: x + width,
                            bottom: cap_top + KEYCAP_HEIGHT,
                        },
                    );
                    x += width + KEYCAP_GAP;
                }
                let mut detail = RECT {
                    left: split + COLUMN_GAP,
                    top,
                    right: left + column.width,
                    bottom: top + ROW_HEIGHT,
                };
                SelectObject(surface.dc, description_font);
                SetTextColor(surface.dc, palette.tooltip_text);
                DrawTextW(
                    surface.dc,
                    description,
                    -1,
                    &mut detail,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
            }
            left += column.width + SECTION_GAP;
        }
        SelectObject(surface.dc, previous_font);
        Some([previous, self.bounds])
    }

    pub unsafe fn paint(&self, dc: HDC, dirty: RECT) {
        if let Some(rect) = intersect(self.bounds, dirty) {
            BitBlt(
                dc,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                self.surface.dc,
                rect.left - self.bounds.left,
                rect.top - self.bounds.top,
                SRCCOPY,
            );
        }
    }
}

#[derive(Clone, Copy)]
pub struct Loupe {
    pub bounds: RECT,
    pub point: POINT,
    /// The color was just copied with C.
    pub copied: bool,
}

/// `#RRGGBB` for a GDI `COLORREF`.
pub fn hex(color: u32) -> String {
    format!("#{:02X}{:02X}{:02X}", color & 0xff, (color >> 8) & 0xff, (color >> 16) & 0xff)
}

impl Loupe {
    pub fn new(point: POINT, monitor: RECT) -> Option<Self> {
        if monitor.right - monitor.left < LOUPE_WIDTH || monitor.bottom - monitor.top < LOUPE_HEIGHT
        {
            return None;
        }
        let offset = |pointer: i32, start: i32, end: i32, size: i32| {
            let after = pointer + POINTER_GAP;
            if after + size <= end {
                after.max(start)
            } else {
                (pointer - POINTER_GAP - size).clamp(start, end - size)
            }
        };
        let left = offset(point.x, monitor.left, monitor.right, LOUPE_WIDTH);
        let top = offset(point.y, monitor.top, monitor.bottom, LOUPE_HEIGHT);
        Some(Self {
            bounds: RECT {
                left,
                top,
                right: left + LOUPE_WIDTH,
                bottom: top + LOUPE_HEIGHT,
            },
            point,
            copied: false,
        })
    }

    /// Swatch, hex value and the C shortcut under the magnified pixels.
    unsafe fn paint_color(self, dc: HDC, color: u32) {
        let strip = RECT {
            left: self.bounds.left + BORDER,
            top: self.bounds.bottom - BORDER - COLOR_STRIP,
            right: self.bounds.right - BORDER,
            bottom: self.bounds.bottom - BORDER,
        };
        let palette = theme::palette();
        SetDCBrushColor(dc, palette.tooltip);
        FillRect(dc, &strip, GetStockObject(DC_BRUSH));
        let swatch = RECT {
            left: strip.left + 5,
            top: strip.top + 5,
            right: strip.left + 5 + COLOR_STRIP - 10,
            bottom: strip.bottom - 5,
        };
        SetDCBrushColor(dc, color);
        FillRect(dc, &swatch, GetStockObject(DC_BRUSH));
        FrameRect(dc, &swatch, GetStockObject(WHITE_BRUSH));
        let previous = SelectObject(dc, typography::font(TextStyle::HintDescription));
        SetBkMode(dc, TRANSPARENT as i32);
        let mut value = RECT { left: swatch.right + 6, ..strip };
        SetTextColor(dc, palette.tooltip_text);
        let text: Vec<u16> = hex(color).encode_utf16().chain([0]).collect();
        DrawTextW(dc, text.as_ptr(), -1, &mut value, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        let mut hint = RECT { right: strip.right - 6, ..strip };
        SetTextColor(dc, if self.copied { palette.success } else { mix(palette.tooltip, palette.tooltip_text, 0.6) });
        DrawTextW(
            dc,
            if self.copied { w!("已复制") } else { w!("C 复制") },
            -1,
            &mut hint,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(dc, previous);
    }

    pub unsafe fn paint(self, dc: HDC, source: HDC, canvas: RECT) {
        let black = GetStockObject(BLACK_BRUSH);
        let white = GetStockObject(WHITE_BRUSH);
        FillRect(dc, &self.bounds, black);

        let sample = RECT {
            left: self.point.x - SAMPLE_WIDTH / 2,
            top: self.point.y - SAMPLE_HEIGHT / 2,
            right: self.point.x + SAMPLE_WIDTH / 2 + 1,
            bottom: self.point.y + SAMPLE_HEIGHT / 2 + 1,
        };
        let left = self.bounds.left + BORDER;
        let top = self.bounds.top + BORDER;
        let color = GetPixel(source, self.point.x, self.point.y);
        if let Some(visible) = intersect(sample, canvas) {
            SetStretchBltMode(dc, COLORONCOLOR);
            StretchBlt(
                dc,
                left + (visible.left - sample.left) * ZOOM,
                top + (visible.top - sample.top) * ZOOM,
                (visible.right - visible.left) * ZOOM,
                (visible.bottom - visible.top) * ZOOM,
                source,
                visible.left,
                visible.top,
                visible.right - visible.left,
                visible.bottom - visible.top,
                SRCCOPY,
            );
        }
        FrameRect(dc, &self.bounds, white);
        let x = left + SAMPLE_WIDTH / 2 * ZOOM;
        let y = top + SAMPLE_HEIGHT / 2 * ZOOM;
        let center_x = x + ZOOM / 2;
        let center_y = y + ZOOM / 2;
        // Leave the sampled pixel clear and outline the guides for contrast on any background.
        for (radius, brush) in [(1, black), (0, white)] {
            for arm in [
                RECT {
                    left,
                    top: center_y - radius,
                    right: x - 2,
                    bottom: center_y + radius + 1,
                },
                RECT {
                    left: x + ZOOM + 2,
                    top: center_y - radius,
                    right: self.bounds.right - BORDER,
                    bottom: center_y + radius + 1,
                },
                RECT {
                    left: center_x - radius,
                    top,
                    right: center_x + radius + 1,
                    bottom: y - 2,
                },
                RECT {
                    left: center_x - radius,
                    top: y + ZOOM + 2,
                    right: center_x + radius + 1,
                    bottom: self.bounds.bottom - BORDER,
                },
            ] {
                FillRect(dc, &arm, brush);
            }
        }
        FrameRect(
            dc,
            &RECT {
                left: x - 2,
                top: y - 2,
                right: x + ZOOM + 2,
                bottom: y + ZOOM + 2,
            },
            black,
        );
        FrameRect(
            dc,
            &RECT {
                left: x - 1,
                top: y - 1,
                right: x + ZOOM + 1,
                bottom: y + ZOOM + 1,
            },
            white,
        );
        self.paint_color(dc, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::null_mut;

    #[test]
    fn hidden_hints_fold_to_one_line_and_open_again() {
        unsafe {
            let area = RECT { right: 800, bottom: 600, ..Default::default() };
            let source = Surface::new(null_mut(), area.right, area.bottom).unwrap();
            let r = |rect: RECT| (rect.left, rect.top, rect.right, rect.bottom);
            let mut hint = HintPanel::new(source.dc, area).unwrap();
            let [_, open] = hint.set_mode(source.dc, HintMode::Selected).unwrap();
            assert!(hint.set_hidden(source.dc, false).is_none(), "already open");
            let [before, folded] = hint.set_hidden(source.dc, true).unwrap();
            assert_eq!(r(before), r(open));
            assert_eq!(folded.bottom, open.bottom, "stays anchored at the bottom");
            assert_eq!(folded.bottom - folded.top, ROW_HEIGHT + 2 * PANEL_INSET, "one line, no title");
            assert!(folded.right - folded.left < 300);
            // Mode changes while folded keep the one line.
            let [_, still] = hint.set_mode(source.dc, HintMode::Automatic).unwrap();
            assert_eq!(r(still), r(folded));
            let [_, reopened] = hint.set_hidden(source.dc, false).unwrap();
            assert_eq!(r(reopened), r(HintPanel::new(source.dc, area).unwrap().bounds), "back to the full Automatic panel");
        }
    }

    #[test]
    fn hint_rows_refresh_by_mode_and_keep_the_bottom_anchor() {
        unsafe {
            let area = RECT {
                right: 800,
                bottom: 600,
                ..Default::default()
            };
            let source = Surface::new(null_mut(), area.right, area.bottom).unwrap();
            let mut hint = HintPanel::new(source.dc, area).unwrap();
            let automatic = hint.bounds;
            let [before, selected] = hint.set_mode(source.dc, HintMode::Selected).unwrap();
            assert_eq!(before.top, automatic.top);
            assert_eq!(selected.bottom, automatic.bottom);
            // Automatic lists 6 rows; Selected's longest column (标注) has 10.
            assert_eq!(automatic.top - selected.top, 4 * ROW_HEIGHT);
            assert!(selected.right - selected.left > automatic.right - automatic.left + 200, "three columns");
            if std::env::var_os("SNAPSHOT_PREVIEW").is_some() {
                let folder = std::env::temp_dir().join("snapshot-preview");
                std::fs::create_dir_all(&folder).unwrap();
                for (name, mode, hidden) in [
                    ("selected", HintMode::Selected, false),
                    ("rectangle", HintMode::Annotate(Tool::Rectangle), false),
                    ("automatic", HintMode::Automatic, false),
                    ("text", HintMode::SelectedText, false),
                    ("hidden", HintMode::Selected, true),
                ] {
                    let mut panel = HintPanel::new(source.dc, area).unwrap();
                    panel.set_mode(source.dc, mode);
                    panel.set_hidden(source.dc, hidden);
                    let canvas = Surface::new(null_mut(), area.right, area.bottom).unwrap();
                    FillRect(canvas.dc, &area, GetStockObject(GRAY_BRUSH));
                    panel.paint(canvas.dc, area);
                    GdiFlush();
                    let mut image = crate::capture::Desktop { bounds: area, surface: canvas }.crop(panel.bounds).unwrap();
                    for pixel in image.pixels.chunks_exact_mut(4) {
                        pixel[3] = 255;
                    }
                    image.save_png(&folder.join(format!("hint-{name}.png"))).unwrap();
                }
            }
            assert!(hint.set_mode(source.dc, HintMode::Selected).is_none());
            let [_, text] = hint.set_mode(source.dc, HintMode::EditText).unwrap();
            // EditText has 3 rows; the panel stays anchored at the bottom.
            assert_eq!(text.top - automatic.top, 3 * ROW_HEIGHT);
            assert_eq!(text.bottom, automatic.bottom);
            let mut custom = hotkey::DEFAULT;
            custom.set(
                Binding::Copy,
                hotkey::Shortcut::parse("Ctrl+Alt+Shift+F10").unwrap(),
            );
            hotkey::activate(custom);
            let [_, wider] = hint.set_mode(source.dc, HintMode::Selected).unwrap();
            hotkey::activate(hotkey::DEFAULT);
            assert_eq!(wider.left, selected.left);
            assert_eq!(wider.bottom, selected.bottom);
            assert!(wider.right > selected.right);
            assert!(wider.right <= area.right);
        }
    }

    #[test]
    fn loupe_draws_both_guides_without_covering_the_sampled_pixel() {
        unsafe {
            let canvas = RECT {
                right: 400,
                bottom: 300,
                ..Default::default()
            };
            let source = Surface::new(null_mut(), canvas.right, canvas.bottom).unwrap();
            let rendered = Surface::new(null_mut(), canvas.right, canvas.bottom).unwrap();
            let source_color = 0x005a_c8f5;
            SetDCBrushColor(source.dc, source_color);
            FillRect(source.dc, &canvas, GetStockObject(DC_BRUSH));

            let loupe = Loupe::new(POINT { x: 100, y: 100 }, canvas).unwrap();
            loupe.paint(rendered.dc, source.dc, canvas);
            let x = loupe.bounds.left + LOUPE_WIDTH / 2;
            let y = loupe.bounds.top + (LOUPE_HEIGHT - COLOR_STRIP) / 2;
            assert_eq!(GetPixel(rendered.dc, x - 20, y), 0x00ff_ffff);
            assert_eq!(GetPixel(rendered.dc, x, y - 20), 0x00ff_ffff);
            assert_eq!(GetPixel(rendered.dc, x - 20, y - 1), 0);
            assert_eq!(GetPixel(rendered.dc, x, y), source_color);
            // The strip below shows the picked color as a swatch and in hex.
            let strip = loupe.bounds.bottom - BORDER - COLOR_STRIP / 2;
            assert_eq!(GetPixel(rendered.dc, loupe.bounds.left + BORDER + 12, strip), source_color);
            assert_eq!(hex(source_color), "#F5C85A");
            if std::env::var_os("SNAPSHOT_PREVIEW").is_some() {
                let bounds = loupe.bounds;
                let mut image = crate::capture::Desktop { bounds: canvas, surface: rendered }.crop(bounds).unwrap();
                for pixel in image.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                let folder = std::env::temp_dir().join("snapshot-preview");
                std::fs::create_dir_all(&folder).unwrap();
                image.save_png(&folder.join("loupe.png")).unwrap();
            }
        }
    }

    #[test]
    fn loupe_flips_at_each_monitor_edge_and_handles_negative_origins() {
        let monitor = RECT {
            left: -1200,
            top: -800,
            right: 0,
            bottom: 0,
        };
        for (point, left, top) in [
            (POINT { x: -700, y: -500 }, false, false),
            (POINT { x: -10, y: -500 }, true, false),
            (POINT { x: -700, y: -10 }, false, true),
            (POINT { x: -10, y: -10 }, true, true),
        ] {
            let rect = Loupe::new(point, monitor).unwrap().bounds;
            assert_eq!(rect.right < point.x, left);
            assert_eq!(rect.bottom < point.y, top);
            assert!(rect.left >= monitor.left && rect.right <= monitor.right);
            assert!(rect.top >= monitor.top && rect.bottom <= monitor.bottom);
        }
    }
}
