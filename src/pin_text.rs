//! The text layer of a pin made with "extract text": recognition status,
//! word selection by dragging (like Live Text), and the "copy all" button.
//! Geometry is in source image pixels; `View` maps it onto the zoomed pin.

use windows_sys::Win32::{
    Foundation::{POINT, RECT},
    Graphics::Gdi::HDC,
};

use crate::{
    codes::Code,
    gdiplus::Graphics,
    ocr::{self, Line},
    theme::{self, argb},
    ui::{self, Fonts},
};

/// Word position: (line, word), ordered in reading order.
pub type Pos = (usize, usize);

pub enum Status {
    Recognizing,
    Ready(Vec<Line>),
    Failed,
}

pub struct TextLayer {
    pub status: Status,
    anchor: Option<Pos>,
    focus: Option<Pos>,
    pub selecting: bool,
    pub hovering: bool,
    pub button_hovered: bool,
    /// Short feedback shown at the bottom, like "已复制".
    pub toast: Option<String>,
    /// QR codes and barcodes found next to the text.
    pub codes: Vec<Code>,
    pub hovered_code: Option<usize>,
}

/// Where the image sits in the pin's client area.
#[derive(Clone, Copy)]
pub struct View {
    pub origin: POINT,
    pub zoom: f32,
    pub scale: f32,
}

impl View {
    fn to_client(self, rect: [f32; 4]) -> [f32; 4] {
        [
            self.origin.x as f32 + rect[0] * self.zoom,
            self.origin.y as f32 + rect[1] * self.zoom,
            self.origin.x as f32 + rect[2] * self.zoom,
            self.origin.y as f32 + rect[3] * self.zoom,
        ]
    }

    fn to_image(self, point: POINT) -> (f32, f32) {
        (
            (point.x - self.origin.x) as f32 / self.zoom,
            (point.y - self.origin.y) as f32 / self.zoom,
        )
    }

    fn s(self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }
}

impl TextLayer {
    pub fn new() -> Self {
        Self {
            status: Status::Recognizing,
            anchor: None,
            focus: None,
            selecting: false,
            hovering: false,
            button_hovered: false,
            toast: None,
            codes: Vec::new(),
            hovered_code: None,
        }
    }

    /// The code under `point`, if any; a click there copies it.
    pub fn code_at(&self, view: View, point: POINT) -> Option<usize> {
        let (x, y) = view.to_image(point);
        self.codes
            .iter()
            .position(|code| x >= code.rect[0] && x <= code.rect[2] && y >= code.rect[1] && y <= code.rect[3])
    }

    pub fn lines(&self) -> &[Line] {
        match &self.status {
            Status::Ready(lines) => lines,
            _ => &[],
        }
    }

    /// The "copy all" button in the top-right corner of the image.
    pub fn button(&self, view: View, image: RECT, fonts: &Fonts, dc: HDC) -> Option<RECT> {
        if self.lines().is_empty() {
            return None;
        }
        let width = unsafe { ui::measure(dc, BUTTON_LABEL, fonts.small) } + view.s(20);
        let height = view.s(26);
        let margin = view.s(6);
        let rect = RECT {
            left: image.right - margin - width,
            top: image.top + margin,
            right: image.right - margin,
            bottom: image.top + margin + height,
        };
        (rect.left >= image.left && rect.bottom <= image.bottom).then_some(rect)
    }

    /// Whether `point` is on recognized text, where a drag selects instead of moving the pin.
    pub fn over_text(&self, view: View, point: POINT) -> bool {
        if self.code_at(view, point).is_some() {
            return true;
        }
        let slack = 3.0 / view.zoom;
        let (x, y) = view.to_image(point);
        self.lines().iter().flat_map(|line| &line.words).any(|word| {
            x >= word.rect[0] - slack
                && x <= word.rect[2] + slack
                && y >= word.rect[1] - slack
                && y <= word.rect[3] + slack
        })
    }

    pub fn begin(&mut self, view: View, point: POINT) {
        let pos = nearest(self.lines(), view.to_image(point));
        self.anchor = pos;
        self.focus = pos;
        self.selecting = pos.is_some();
    }

    /// Extends the selection to `point`. True if it changed.
    pub fn extend(&mut self, view: View, point: POINT) -> bool {
        let pos = nearest(self.lines(), view.to_image(point));
        pos.is_some() && std::mem::replace(&mut self.focus, pos) != pos
    }

    pub fn select_line(&mut self, view: View, point: POINT) {
        if let Some((line, _)) = nearest(self.lines(), view.to_image(point)) {
            self.anchor = Some((line, 0));
            self.focus = Some((line, self.lines()[line].words.len() - 1));
        }
    }

    pub fn select_all(&mut self) {
        let lines = self.lines();
        if let Some(last) = lines.len().checked_sub(1) {
            let end = (last, lines[last].words.len() - 1);
            self.anchor = Some((0, 0));
            self.focus = Some(end);
        }
    }

    /// Clears the selection. True if there was one.
    pub fn clear(&mut self) -> bool {
        self.selecting = false;
        self.focus = None;
        self.anchor.take().is_some()
    }

    pub fn has_selection(&self) -> bool {
        self.range().is_some()
    }

    fn range(&self) -> Option<(Pos, Pos)> {
        let (a, b) = (self.anchor?, self.focus?);
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// The selected text, or everything when nothing is selected.
    pub fn copy_text(&self) -> String {
        let lines = self.lines();
        let Some(((first_line, first_word), (last_line, last_word))) = self.range() else {
            return ocr::text(lines);
        };
        (first_line..=last_line)
            .map(|index| {
                let line = &lines[index];
                let from = if index == first_line { first_word } else { 0 };
                let to = if index == last_line { last_word + 1 } else { line.words.len() };
                line.text_of(from, to)
            })
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    pub unsafe fn paint(&self, dc: HDC, view: View, image: RECT, fonts: &Fonts) {
        let palette = theme::palette();
        let Some(graphics) = Graphics::new(dc) else { return };
        let lines = self.lines();
        let pad = 2.0 * view.scale;
        let range = self.range();
        if let Some(((first_line, first_word), (last_line, last_word))) = range {
            for index in first_line..=last_line {
                let line = &lines[index];
                let from = if index == first_line { first_word } else { 0 };
                let to = if index == last_line { last_word } else { line.words.len() - 1 };
                let [_, top, _, bottom] = line.bounds();
                let rect = [line.words[from].rect[0], top, line.words[to].rect[2], bottom];
                graphics.fill_round(
                    ui::inset(view.to_client(rect), -pad),
                    pad,
                    argb(palette.accent, 96),
                );
            }
        } else if self.hovering {
            // A faint mark on every line shows the text can be selected.
            for line in lines {
                graphics.fill_round(
                    ui::inset(view.to_client(line.bounds()), -pad),
                    pad,
                    argb(palette.accent, 34),
                );
            }
        }
        drop(graphics);
        if self.hovering {
            if let Some(button) = self.button(view, image, fonts, dc) {
                let face = if self.button_hovered { palette.hover } else { palette.card };
                ui::button(dc, button, BUTTON_LABEL, fonts.small, palette, false, face, 7.0 * view.scale);
            }
        }
        self.paint_codes(dc, view, image, fonts);
        let note = match (&self.status, &self.toast) {
            (Status::Recognizing, _) => Some("正在识别文字…"),
            (_, Some(toast)) => Some(toast.as_str()),
            _ => None,
        };
        if let Some(note) = note {
            paint_note(dc, view, image, fonts, note);
        }
    }
}

impl TextLayer {
    /// An outline on every code; the hovered one is filled and gets a hint
    /// with its content under (or above) it.
    unsafe fn paint_codes(&self, dc: HDC, view: View, image: RECT, fonts: &Fonts) {
        if self.codes.is_empty() {
            return;
        }
        let palette = theme::palette();
        if let Some(graphics) = Graphics::new(dc) {
            for (index, code) in self.codes.iter().enumerate() {
                let rect = ui::inset(view.to_client(code.rect), -2.0 * view.scale);
                let radius = 6.0 * view.scale;
                let hovered = self.hovered_code == Some(index);
                if hovered {
                    graphics.fill_round(rect, radius, argb(palette.accent, 40));
                }
                let alpha = if hovered || self.hovering { 255 } else { 150 };
                graphics.stroke_round(rect, radius, argb(palette.accent, alpha), 2.0 * view.scale);
            }
        }
        let Some(code) = self.hovered_code.and_then(|index| self.codes.get(index)) else { return };
        let action = if code.link().is_some() { "单击复制 · Ctrl+单击打开链接" } else { "单击复制" };
        let label = format!("{}：{}", code.kind(), code.text.replace(['\r', '\n'], " "));
        let max_width = image.right - image.left - view.s(12);
        let width = (ui::measure(dc, &label, fonts.small).max(ui::measure(dc, action, fonts.small)) + view.s(20))
            .min(max_width);
        let height = view.s(42);
        let bounds = view.to_client(code.rect);
        let left = (bounds[0] as i32).clamp(image.left + view.s(6), (image.right - view.s(6) - width).max(image.left));
        let below = bounds[3] as i32 + view.s(6);
        let top = if below + height <= image.bottom {
            below
        } else {
            (bounds[1] as i32 - view.s(6) - height).max(image.top)
        };
        let pill = RECT { left, top, right: left + width, bottom: top + height };
        if let Some(graphics) = Graphics::new(dc) {
            graphics.fill_round(ui::to_f(pill), 8.0 * view.scale, argb(palette.tooltip, 240));
        }
        let line = |top: i32, text: &str, color: u32| {
            ui::text(
                dc,
                RECT { left: pill.left + view.s(10), top, right: pill.right - view.s(10), bottom: top + view.s(18) },
                text,
                fonts.small,
                color,
                windows_sys::Win32::Graphics::Gdi::DT_LEFT
                    | windows_sys::Win32::Graphics::Gdi::DT_VCENTER
                    | windows_sys::Win32::Graphics::Gdi::DT_SINGLELINE
                    | windows_sys::Win32::Graphics::Gdi::DT_END_ELLIPSIS,
            )
        };
        line(pill.top + view.s(4), &label, palette.tooltip_text);
        line(pill.top + view.s(21), action, theme::mix(palette.tooltip, palette.tooltip_text, 0.65));
    }
}

const BUTTON_LABEL: &str = "复制全部文字";

/// A dark pill at the bottom-left of the image.
unsafe fn paint_note(dc: HDC, view: View, image: RECT, fonts: &Fonts, note: &str) {
    let palette = theme::palette();
    let width = (ui::measure(dc, note, fonts.small) + view.s(20)).min(image.right - image.left);
    let height = view.s(24);
    let margin = view.s(6);
    let rect = RECT {
        left: image.left + margin,
        top: image.bottom - margin - height,
        right: image.left + margin + width,
        bottom: image.bottom - margin,
    };
    if rect.top < image.top {
        return;
    }
    if let Some(graphics) = Graphics::new(dc) {
        graphics.fill_round(ui::to_f(rect), height as f32 / 2.0, argb(palette.tooltip, 235));
    }
    ui::text(
        dc,
        rect,
        note,
        fonts.small,
        palette.tooltip_text,
        windows_sys::Win32::Graphics::Gdi::DT_CENTER
            | windows_sys::Win32::Graphics::Gdi::DT_VCENTER
            | windows_sys::Win32::Graphics::Gdi::DT_SINGLELINE,
    );
}

/// The word nearest to an image point: the closest line by height, then the
/// closest word in it along x.
fn nearest(lines: &[Line], (x, y): (f32, f32)) -> Option<Pos> {
    let distance = |low: f32, high: f32, value: f32| (low - value).max(value - high).max(0.0);
    let (line, _) = lines.iter().enumerate().min_by(|(_, a), (_, b)| {
        let [_, a_top, _, a_bottom] = a.bounds();
        let [_, b_top, _, b_bottom] = b.bounds();
        distance(a_top, a_bottom, y).total_cmp(&distance(b_top, b_bottom, y))
    })?;
    let (word, _) = lines[line].words.iter().enumerate().min_by(|(_, a), (_, b)| {
        distance(a.rect[0], a.rect[2], x).total_cmp(&distance(b.rect[0], b.rect[2], x))
    })?;
    Some((line, word))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::Word;

    fn line(top: f32, words: &[&str]) -> Line {
        Line {
            words: words
                .iter()
                .enumerate()
                .map(|(index, text)| {
                    let left = index as f32 * 30.0;
                    Word {
                        text: (*text).into(),
                        rect: [left, top, left + 24.0, top + 14.0],
                    }
                })
                .collect(),
        }
    }

    fn layer() -> TextLayer {
        let mut layer = TextLayer::new();
        layer.status = Status::Ready(vec![
            line(0.0, &["第", "一", "行"]),
            line(20.0, &["Second", "line", "here"]),
            line(40.0, &["末", "行"]),
        ]);
        layer
    }

    const VIEW: View = View {
        origin: POINT { x: 5, y: 5 },
        zoom: 2.0,
        scale: 1.0,
    };

    /// Client point at image coordinates (x, y).
    fn at(x: i32, y: i32) -> POINT {
        POINT {
            x: 5 + x * 2,
            y: 5 + y * 2,
        }
    }

    #[test]
    fn dragging_selects_across_lines_in_reading_order() {
        let mut layer = layer();
        assert_eq!(layer.copy_text(), "第一行\r\nSecond line here\r\n末行");
        // Drag backwards from the 2nd word of line 3 to the 2nd word of line 1.
        layer.begin(VIEW, at(40, 45));
        assert!(layer.extend(VIEW, at(35, 5)));
        assert!(!layer.extend(VIEW, at(36, 6)), "same word, no change");
        assert_eq!(layer.copy_text(), "一行\r\nSecond line here\r\n末行");
        layer.extend(VIEW, at(70, 25));
        assert_eq!(layer.copy_text(), "here\r\n末行");
        assert!(layer.clear());
        assert_eq!(layer.copy_text(), "第一行\r\nSecond line here\r\n末行", "nothing selected copies all");
    }

    #[test]
    fn double_click_selects_the_line_and_ctrl_a_everything() {
        let mut layer = layer();
        layer.select_line(VIEW, at(35, 22));
        assert_eq!(layer.copy_text(), "Second line here");
        layer.select_all();
        assert!(layer.has_selection());
        assert_eq!(layer.copy_text(), "第一行\r\nSecond line here\r\n末行");
    }

    #[test]
    fn only_text_is_a_selection_target() {
        let layer = layer();
        assert!(layer.over_text(VIEW, at(10, 5)));
        assert!(!layer.over_text(VIEW, at(10, 70)), "below the last line moves the pin");
        assert!(!layer.over_text(VIEW, at(200, 5)), "right of the text moves the pin");
        assert!(!TextLayer::new().over_text(VIEW, at(10, 5)), "nothing while recognizing");
    }

    /// Renders a pin's text layer over real OCR output to
    /// `%TEMP%\snapshot-preview\pin-text-*.png`. Run with `<test binary> pin_text --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        use crate::capture::{Desktop, Surface};
        use windows_sys::Win32::Graphics::Gdi::{BitBlt, GdiFlush, SRCCOPY};
        unsafe {
            let folder = std::env::temp_dir().join("snapshot-preview");
            std::fs::create_dir_all(&folder).unwrap();
            let source = crate::ocr::tests::sample("识别文字 Snapshot 保存到 C:\\Users\\eric\\Pictures", 18, true);
            let lines = crate::ocr::recognize(&source).unwrap();
            let (width, height) = (source.width, source.height);
            let (image_surface, _, _) = source.into_surface().unwrap();
            let fonts = Fonts::new(1.0);
            let view = View { origin: POINT { x: 0, y: 0 }, zoom: 1.0, scale: 1.0 };
            let bounds = RECT { left: 0, top: 0, right: width, bottom: height };
            let mut states = Vec::new();
            let mut hover = TextLayer::new();
            hover.status = Status::Ready(lines.clone());
            hover.hovering = true;
            states.push(("hover", hover));
            let mut selected = TextLayer::new();
            selected.status = Status::Ready(lines.clone());
            selected.hovering = true;
            selected.begin(view, POINT { x: 40, y: height / 2 });
            selected.extend(view, POINT { x: 250, y: height / 2 });
            selected.toast = Some("已复制 12 个字".into());
            states.push(("selected", selected));
            states.push(("recognizing", TextLayer::new()));
            let mut code = TextLayer::new();
            code.status = Status::Ready(lines.clone());
            code.codes = vec![crate::codes::Code {
                text: "https://github.com/AllToBeNize/snapshot".into(),
                matrix: true,
                rect: [width as f32 - 60.0, 4.0, width as f32 - 8.0, height as f32 - 4.0],
            }];
            code.hovered_code = Some(0);
            states.push(("code", code));
            for (name, layer) in states {
                let surface = Surface::new(std::ptr::null_mut(), width, height).unwrap();
                BitBlt(surface.dc, 0, 0, width, height, image_surface.dc, 0, 0, SRCCOPY);
                layer.paint(surface.dc, view, bounds, &fonts);
                GdiFlush();
                let mut image = Desktop { bounds, surface }.crop(bounds).unwrap();
                for pixel in image.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                image.save_png(&folder.join(format!("pin-text-{name}.png"))).unwrap();
            }
        }
    }
}
