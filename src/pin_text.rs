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
/// A caret between characters: line, word, and how many of the word's
/// characters come before it.
pub type Pos = (usize, usize, usize);

pub enum Status {
    Recognizing,
    Ready(Vec<Line>),
    Failed,
}

/// Width kept free at the top right for the pin's pushpin, in 96-DPI pixels.
pub const PUSHPIN_ROOM: i32 = 30;

pub struct TextLayer {
    pub status: Status,
    anchor: Option<Pos>,
    focus: Option<Pos>,
    /// Where the button went down, in client pixels; a drag selects once
    /// it moves this far, a click only clears.
    pressed_at: Option<POINT>,
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
            pressed_at: None,
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
        // Left of the pin's pushpin in the top-right corner.
        let right = image.right - margin - view.s(PUSHPIN_ROOM);
        let rect = RECT {
            left: right - width,
            top: image.top + margin,
            right,
            bottom: image.top + margin + height,
        };
        (rect.left >= image.left && rect.bottom <= image.bottom).then_some(rect)
    }

    /// Whether `point` is on recognized text, where a drag selects instead
    /// of moving the pin: anywhere along a line, gaps between words
    /// included, and a little above and below it.
    pub fn over_text(&self, view: View, point: POINT) -> bool {
        if self.code_at(view, point).is_some() {
            return true;
        }
        let (x, y) = view.to_image(point);
        self.lines().iter().any(|line| {
            let [left, top, right, bottom] = line.bounds();
            let slack = (3.0 / view.zoom).max((bottom - top) * 0.25);
            x >= left - slack && x <= right + slack && y >= top - slack && y <= bottom + slack
        })
    }

    /// The button went down on the text: nothing shows until it drags.
    pub fn begin(&mut self, view: View, point: POINT) {
        let pos = nearest(self.lines(), view.to_image(point));
        self.anchor = pos;
        self.focus = None;
        self.pressed_at = Some(point);
        self.selecting = pos.is_some();
    }

    /// Extends the selection to `point` once the press has become a drag.
    /// True if it changed.
    pub fn extend(&mut self, view: View, point: POINT) -> bool {
        let threshold = (3.0 * view.scale).round() as i32;
        let moved = self.pressed_at.is_some_and(|at| (point.x - at.x).abs() > threshold || (point.y - at.y).abs() > threshold);
        if self.focus.is_none() && !moved {
            return false;
        }
        let pos = nearest(self.lines(), view.to_image(point));
        pos.is_some() && std::mem::replace(&mut self.focus, pos) != pos
    }

    /// Double-click: the word under the point the way a code editor sees
    /// it: letters, digits and underscores together (`E_WindowPlacementAnchors`),
    /// a run of CJK characters, or a single mark (`=`, `.`). It crosses OCR
    /// word boundaries where nothing separates them in the picture.
    pub fn select_word(&mut self, view: View, point: POINT) {
        let lines = self.lines();
        let Some((line, word, offset)) = character_at(lines, view.to_image(point)) else { return };
        // The line's characters in order; None where a space or gap separates words.
        let mut characters: Vec<Option<(usize, usize, char)>> = Vec::new();
        let mut at = 0;
        for (index, candidate) in lines[line].words.iter().enumerate() {
            if index > 0 && !lines[line].attached(index) {
                characters.push(None);
            }
            for (position, character) in candidate.text.chars().enumerate() {
                if index == word && position == offset {
                    at = characters.len();
                }
                characters.push(Some((index, position, character)));
            }
        }
        let Some(Some((_, _, here))) = characters.get(at).copied() else { return };
        let kind = class(here);
        let same = |slot: Option<(usize, usize, char)>| kind != CharClass::Mark && slot.is_some_and(|(_, _, c)| class(c) == kind);
        let mut first = at;
        while first > 0 && same(characters[first - 1]) {
            first -= 1;
        }
        let mut last = at;
        while last + 1 < characters.len() && same(characters[last + 1]) {
            last += 1;
        }
        let (Some((from_word, from_offset, _)), Some((to_word, to_offset, _))) = (characters[first], characters[last]) else { return };
        self.anchor = Some((line, from_word, from_offset));
        self.focus = Some((line, to_word, to_offset + 1));
    }

    /// Triple-click: the whole line.
    pub fn select_line(&mut self, view: View, point: POINT) {
        if let Some((line, _, _)) = nearest(self.lines(), view.to_image(point)) {
            let words = &self.lines()[line].words;
            let end = (line, words.len() - 1, length(&words[words.len() - 1]));
            self.anchor = Some((line, 0, 0));
            self.focus = Some(end);
        }
    }

    pub fn select_all(&mut self) {
        let lines = self.lines();
        if let Some(last) = lines.len().checked_sub(1) {
            let words = &lines[last].words;
            let end = (last, words.len() - 1, length(&words[words.len() - 1]));
            self.anchor = Some((0, 0, 0));
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

    /// The selection in reading order; none while empty.
    fn range(&self) -> Option<(Pos, Pos)> {
        let (a, b) = (self.anchor?, self.focus?);
        let (from, to) = if a <= b { (a, b) } else { (b, a) };
        (self.slices(from, to).iter().any(|line| !line.text().is_empty())).then_some((from, to))
    }

    /// The selected part of each line from `from` to `to`, words cut at the carets.
    fn slices(&self, from: Pos, to: Pos) -> Vec<Line> {
        let lines = self.lines();
        (from.0..=to.0)
            .map(|index| {
                let line = &lines[index];
                let first = if index == from.0 { (from.1, from.2) } else { (0, 0) };
                let last = if index == to.0 { (to.1, to.2) } else { (line.words.len() - 1, usize::MAX) };
                let words = (first.0..=last.0)
                    .filter_map(|word_index| {
                        let word = &line.words[word_index];
                        let start = if word_index == first.0 { first.1 } else { 0 };
                        let end = if word_index == last.0 { last.1 } else { usize::MAX };
                        let text: String = word.text.chars().skip(start).take(end.saturating_sub(start)).collect();
                        let carets = word.carets.iter().skip(start).take(text.chars().count() + 1).copied().collect();
                        (!text.is_empty()).then(|| ocr::Word { text, carets, ..word.clone() })
                    })
                    .collect();
                Line { words }
            })
            .collect()
    }

    /// The selected text, or everything when nothing is selected.
    pub fn copy_text(&self) -> String {
        let lines = self.lines();
        let Some((from, to)) = self.range() else {
            return ocr::text(lines);
        };
        self.slices(from, to).iter().map(Line::text).collect::<Vec<_>>().join("\r\n")
    }

    pub unsafe fn paint(&self, dc: HDC, view: View, image: RECT, fonts: &Fonts) {
        let palette = theme::palette();
        let Some(graphics) = Graphics::new(dc) else { return };
        let lines = self.lines();
        let pad = 2.0 * view.scale;
        let range = self.range();
        if let Some((from, to)) = range {
            let fixed = monospace(lines);
            for index in from.0..=to.0 {
                let line = &lines[index];
                let last = line.words.len() - 1;
                let start = if index == from.0 { from } else { (index, 0, 0) };
                let end = if index == to.0 { to } else { (index, last, length(&line.words[last])) };
                let [_, top, _, bottom] = line.bounds();
                let left = caret_x(&line.words[start.1], start.2, fixed);
                let right = caret_x(&line.words[end.1], end.2, fixed);
                if right - left < 0.5 {
                    continue;
                }
                // Flush with the carets left and right, like an editor's
                // selection, so it never reaches into the next character.
                let [left, top, right, bottom] = view.to_client([left, top, right, bottom]);
                graphics.fill_round([left, top - pad, right, bottom + pad], pad, argb(palette.accent, 96));
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

fn length(word: &ocr::Word) -> usize {
    word.text.chars().count()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CharClass {
    /// Letters, digits and underscores: an identifier.
    Word,
    Cjk,
    /// Anything else stands alone.
    Mark,
}

fn class(character: char) -> CharClass {
    if is_wide(character) {
        CharClass::Cjk
    } else if character.is_alphanumeric() || character == '_' {
        CharClass::Word
    } else {
        CharClass::Mark
    }
}

fn is_wide(character: char) -> bool {
    ('\u{2e80}'..='\u{9fff}').contains(&character) || ('\u{f900}'..='\u{ffef}').contains(&character)
}

/// Code is shot in fixed-width fonts: when the Latin words all come out
/// about as wide per character, every character gets the same width.
fn monospace(lines: &[Line]) -> bool {
    let widths: Vec<f32> = lines
        .iter()
        .flat_map(|line| &line.words)
        .filter(|word| word.text.is_ascii() && word.text.len() >= 3)
        .map(|word| (word.rect[2] - word.rect[0]) / word.text.len() as f32)
        .collect();
    if widths.len() < 2 {
        return false;
    }
    let mean = widths.iter().sum::<f32>() / widths.len() as f32;
    let spread = (widths.iter().map(|width| (width - mean).powi(2)).sum::<f32>() / widths.len() as f32).sqrt();
    spread < mean * 0.12
}

/// A character's share of its word's width: OCR only boxes whole words, so
/// characters are placed by how wide they usually are, or all alike in a
/// fixed-width font.
fn weight(character: char, fixed: bool) -> f32 {
    if is_wide(character) {
        2.0
    } else if fixed {
        1.0
    } else if "il.,:;|'!`Iıj ".contains(character) {
        0.6
    } else if "mwMW".contains(character) {
        1.5
    } else {
        1.0
    }
}

/// The x of the caret before character `offset` of `word`, in image pixels:
/// where the recognizer placed it, else estimated from the word's box.
fn caret_x(word: &ocr::Word, offset: usize, fixed: bool) -> f32 {
    if word.carets.len() == length(word) + 1 {
        return word.carets[offset.min(word.carets.len() - 1)];
    }
    let weights: Vec<f32> = word.text.chars().map(|character| weight(character, fixed)).collect();
    let total: f32 = weights.iter().sum::<f32>().max(f32::EPSILON);
    let before: f32 = weights.iter().take(offset).sum();
    word.rect[0] + (word.rect[2] - word.rect[0]) * before / total
}

/// The caret nearest to an image point: the closest line by height (of
/// lines side by side at that height, like line numbers beside code, the
/// closest along x), then the closest gap between characters along x.
fn nearest(lines: &[Line], (x, y): (f32, f32)) -> Option<Pos> {
    let distance = |low: f32, high: f32, value: f32| (low - value).max(value - high).max(0.0);
    let vertical = |line: &Line| {
        let [_, top, _, bottom] = line.bounds();
        distance(top, bottom, y)
    };
    let closest = lines.iter().map(vertical).min_by(f32::total_cmp)?;
    let (line, _) = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            let [_, top, _, bottom] = line.bounds();
            vertical(line) <= closest + (bottom - top) * 0.25
        })
        .min_by(|(_, a), (_, b)| {
            let horizontal = |line: &Line| {
                let [left, _, right, _] = line.bounds();
                distance(left, right, x)
            };
            horizontal(a).total_cmp(&horizontal(b)).then(vertical(a).total_cmp(&vertical(b)))
        })?;
    let words = &lines[line].words;
    let (word, _) = words.iter().enumerate().min_by(|(_, a), (_, b)| {
        distance(a.rect[0], a.rect[2], x).total_cmp(&distance(b.rect[0], b.rect[2], x))
    })?;
    let fixed = monospace(lines);
    let offset = (0..=length(&words[word]))
        .min_by(|&a, &b| (caret_x(&words[word], a, fixed) - x).abs().total_cmp(&(caret_x(&words[word], b, fixed) - x).abs()))
        .unwrap_or(0);
    Some((line, word, offset))
}

/// The character under an image point (not the gap between characters).
fn character_at(lines: &[Line], (x, y): (f32, f32)) -> Option<Pos> {
    let (line, word, _) = nearest(lines, (x, y))?;
    let target = &lines[line].words[word];
    let fixed = monospace(lines);
    let count = length(target);
    let offset = (0..count)
        .find(|&offset| x < caret_x(target, offset + 1, fixed))
        .unwrap_or(count.saturating_sub(1));
    Some((line, word, offset))
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
                        spaced: false,
                        carets: Vec::new(),
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
    fn dragging_selects_character_by_character_across_lines() {
        let mut layer = layer();
        assert_eq!(layer.copy_text(), "第一行\r\nSecond line here\r\n末行");
        // "Second" spans x 0..24, four pixels a letter: drag from after S to after o.
        layer.begin(VIEW, at(4, 22));
        assert!(!layer.extend(VIEW, at(4, 22)), "a click is not a selection");
        assert!(!layer.has_selection());
        assert!(layer.extend(VIEW, at(16, 22)));
        assert_eq!(layer.copy_text(), "eco");
        assert!(!layer.extend(VIEW, at(17, 23)), "same caret, no change");
        // Backwards from the end of the last line to the start of 一.
        layer.clear();
        layer.begin(VIEW, at(54, 45));
        assert!(layer.extend(VIEW, at(30, 5)));
        assert_eq!(layer.copy_text(), "一行\r\nSecond line here\r\n末行");
        layer.extend(VIEW, at(66, 25));
        assert_eq!(layer.copy_text(), "ere\r\n末行");
        assert!(layer.clear());
        assert_eq!(layer.copy_text(), "第一行\r\nSecond line here\r\n末行", "nothing selected copies all");
    }

    fn code_line(text: &str) -> Line {
        // A fixed-width font: every character 8 px wide.
        let mut left = 0.0;
        let words = text
            .split(' ')
            .map(|piece| {
                let width = piece.chars().count() as f32 * 8.0;
                let word = Word { text: piece.into(), rect: [left, 0.0, left + width, 14.0], spaced: false, carets: Vec::new() };
                left += width + 8.0;
                word
            })
            .collect();
        Line { words }
    }

    #[test]
    fn double_click_picks_identifiers_like_a_code_editor() {
        let mut layer = TextLayer::new();
        layer.status = Status::Ready(vec![code_line("layout_mode=E_WindowPlacementAnchors.Centered, tool_tip=None")]);
        let view = View { origin: POINT { x: 0, y: 0 }, zoom: 1.0, scale: 1.0 };
        // Inside Placement, character 25.
        layer.select_word(view, POINT { x: 25 * 8 + 3, y: 7 });
        assert_eq!(layer.copy_text(), "E_WindowPlacementAnchors");
        // On the underscore of layout_mode.
        layer.select_word(view, POINT { x: 6 * 8 + 3, y: 7 });
        assert_eq!(layer.copy_text(), "layout_mode");
        // A mark stands alone.
        layer.select_word(view, POINT { x: 11 * 8 + 3, y: 7 });
        assert_eq!(layer.copy_text(), "=");
        // Words split by OCR with nothing between them still join.
        let mut split = code_line("layout _mode=E");
        split.words[1].rect[0] = 48.0;
        split.words[1].rect[2] = 48.0 + 8.0 * 7.0;
        layer.status = Status::Ready(vec![split]);
        layer.select_word(view, POINT { x: 2 * 8 + 3, y: 7 });
        assert_eq!(layer.copy_text(), "layout_mode");
    }

    #[test]
    fn line_numbers_beside_code_do_not_take_the_click() {
        // OCR reads the gutter and the code as two lines at the same height.
        let mut code = code_line("self.cached_user_name = None");
        for word in &mut code.words {
            word.rect[0] += 100.0;
            word.rect[2] += 100.0;
        }
        let number = Line { words: vec![Word { text: "564".into(), rect: [2.0, 1.0, 30.0, 15.0], spaced: false, carets: Vec::new() }] };
        let mut layer = TextLayer::new();
        layer.status = Status::Ready(vec![number, code]);
        let view = View { origin: POINT { x: 0, y: 0 }, zoom: 1.0, scale: 1.0 };
        layer.select_word(view, POINT { x: 100 + 14 * 8 + 3, y: 7 });
        assert_eq!(layer.copy_text(), "cached_user_name");
        layer.select_word(view, POINT { x: 12, y: 7 });
        assert_eq!(layer.copy_text(), "564");
    }

    #[test]
    fn a_fixed_width_font_places_every_character_alike() {
        let lines = vec![code_line("self.window_manager.instance tool_tip=None, render_target")];
        assert!(monospace(&lines));
        let word = &lines[0].words[0];
        assert_eq!(caret_x(word, 4, true), 32.0, "after self, 4 × 8 px");
        // Proportional UI text is not taken for fixed-width.
        let ui = vec![line(0.0, &["Save", "illicit", "MMMM", "window"])];
        assert!(!monospace(&ui));
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
