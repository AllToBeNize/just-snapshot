use std::ptr::null_mut;
use windows_sys::Win32::{
    Foundation::{POINT, RECT},
    Graphics::Gdi::*,
};

use windows_sys::Win32::Graphics::GdiPlus::PointF;

use crate::{
    capture::{Desktop, Image},
    gdiplus::Graphics,
    icons::Seg,
    mosaic::Mosaic,
    selection_geometry::BORDER_BLUE,
    text_mark::{contrast, Align, Look, TextMark},
    theme::argb,
    wide,
};

pub const INK: u32 = 0x0038_46ff;
/// How close the pointer must be to grab a handle or a thin stroke.
const GRAB: i32 = 6;
const HANDLE_HALF: f32 = 3.5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Rectangle,
    Text,
    Brush,
    Mosaic,
    Arrow,
    /// Numbered circles that count up: ①②③.
    Number,
    /// A circle of the capture shown enlarged in a lens beside it.
    Magnify,
    /// Not a drawing tool: picks marks, one by one or in a box, to move together.
    Select,
}

#[derive(Clone, Copy)]
pub struct Style {
    pub rect_width: i32,
    pub rect_color: u32,
    pub rect_round: bool,
    pub rect_dashed: bool,
    pub brush_width: i32,
    pub brush_color: u32,
    pub mosaic_width: i32,
    pub mosaic_block: i32,
    pub text_size: i32,
    pub text_color: u32,
    pub text_look: Look,
    pub text_align: Align,
    pub arrow_width: i32,
    pub arrow_color: u32,
    /// Diameter of a step number.
    pub number_size: i32,
    pub number_color: u32,
    pub magnify_zoom: i32,
    pub magnify_color: u32,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            rect_width: 3,
            rect_color: INK,
            rect_round: false,
            rect_dashed: false,
            brush_width: 4,
            brush_color: INK,
            mosaic_width: 32,
            mosaic_block: 12,
            text_size: 20,
            text_color: INK,
            text_look: Look::Plain,
            text_align: Align::Left,
            arrow_width: 4,
            arrow_color: INK,
            number_size: 28,
            number_color: INK,
            magnify_zoom: 2,
            magnify_color: INK,
        }
    }
}

pub struct Draft {
    pub tool: Tool,
    style: Style,
    start: POINT,
    end: POINT,
    points: Vec<POINT>,
    mosaic: Mosaic,
}

/// A part of a selected mark that can be dragged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Grip {
    Move,
    /// A rectangle corner or edge: -1, 0 or 1 on each axis.
    Resize(i32, i32),
    /// An arrow's tail and tip.
    Start,
    End,
    /// The size of a step number or of the circle a lens enlarges.
    Radius,
    /// The enlarged view of a lens, moved on its own.
    Lens,
}

#[derive(Clone)]
pub enum Mark {
    Rectangle {
        rect: RECT,
        width: i32,
        color: u32,
        round: bool,
        dashed: bool,
    },
    Text(TextMark),
    Brush(Vec<POINT>, i32, u32),
    Mosaic(Mosaic),
    Arrow {
        from: POINT,
        to: POINT,
        width: i32,
        color: u32,
    },
    Number {
        center: POINT,
        value: u32,
        size: i32,
        color: u32,
    },
    Magnify(Lens),
}

/// A local magnifier: the circle at `center` shown `zoom` times larger in a
/// lens at `target`, joined by tangent lines.
#[derive(Clone)]
pub struct Lens {
    pub center: POINT,
    pub radius: i32,
    pub target: POINT,
    pub zoom: i32,
    pub color: u32,
    /// The capture under the circle, clipped to the selection.
    pixels: Image,
    area: RECT,
}

impl Lens {
    const MIN_RADIUS: i32 = 8;
    const DEFAULT_RADIUS: i32 = 36;

    /// A lens for the circle at `center`, placed where it fits inside `clip`.
    pub fn new(center: POINT, radius: i32, zoom: i32, color: u32, clip: RECT, source: &Desktop) -> Self {
        let mut lens = Self {
            center,
            radius: radius.max(Self::MIN_RADIUS),
            target: center,
            zoom,
            color,
            pixels: Image { width: 0, height: 0, pixels: Vec::new() },
            area: RECT::default(),
        };
        lens.target = lens.place(clip);
        lens.capture(clip, source);
        lens
    }

    fn lens_radius(&self) -> i32 {
        self.radius * self.zoom
    }

    /// Up-right first, like a callout; then the other diagonals; then whatever fits best.
    fn place(&self, clip: RECT) -> POINT {
        let big = self.lens_radius();
        let reach = f64::from(self.radius + big + 24) / std::f64::consts::SQRT_2;
        let reach = reach.round() as i32;
        let candidates = [(1, -1), (-1, -1), (1, 1), (-1, 1)].map(|(x, y)| POINT {
            x: self.center.x + x * reach,
            y: self.center.y + y * reach,
        });
        let fits = |point: &POINT| {
            point.x - big >= clip.left
                && point.x + big <= clip.right
                && point.y - big >= clip.top
                && point.y + big <= clip.bottom
        };
        candidates.iter().copied().find(fits).unwrap_or_else(|| {
            let first = candidates[0];
            POINT {
                x: first.x.clamp(clip.left + big, (clip.right - big).max(clip.left + big)),
                y: first.y.clamp(clip.top + big, (clip.bottom - big).max(clip.top + big)),
            }
        })
    }

    fn capture(&mut self, clip: RECT, source: &Desktop) {
        let r = self.radius;
        let wanted = RECT {
            left: self.center.x - r,
            top: self.center.y - r,
            right: self.center.x + r,
            bottom: self.center.y + r,
        };
        let canvas = RECT {
            right: source.bounds.right - source.bounds.left,
            bottom: source.bounds.bottom - source.bounds.top,
            ..Default::default()
        };
        let area = crate::selection_geometry::intersect(wanted, clip)
            .and_then(|area| crate::selection_geometry::intersect(area, canvas));
        match area.and_then(|area| source.crop(area).ok().map(|pixels| (area, pixels))) {
            Some((area, mut pixels)) => {
                // GDI leaves alpha at 0; the lens draws premultiplied pixels.
                for pixel in pixels.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                self.area = area;
                self.pixels = pixels;
            }
            None => {
                self.area = RECT::default();
                self.pixels = Image { width: 0, height: 0, pixels: Vec::new() };
            }
        }
    }

    fn circle(center: POINT, radius: i32) -> [f32; 4] {
        let (x, y, r) = (center.x as f32, center.y as f32, radius as f32);
        [x - r, y - r, x + r, y + r]
    }

    fn radius_handle(&self) -> POINT {
        let offset = (f64::from(self.radius) / std::f64::consts::SQRT_2).round() as i32;
        POINT { x: self.center.x + offset, y: self.center.y + offset }
    }

    fn bounds(&self) -> RECT {
        let pad = 4;
        let big = self.lens_radius();
        join(
            padded(between(self.center, self.center), self.radius + pad),
            padded(between(self.target, self.target), big + pad),
        )
    }

    unsafe fn paint(&self, dc: HDC) {
        let Some(graphics) = Graphics::new(dc) else { return };
        let ink = argb(self.color, 255);
        let big = self.lens_radius();
        // Outer tangents from the small circle to the lens.
        let (dx, dy) = (f64::from(self.target.x - self.center.x), f64::from(self.target.y - self.center.y));
        let distance = (dx * dx + dy * dy).sqrt();
        let (r1, r2) = (f64::from(self.radius), f64::from(big));
        if distance > r2 - r1 + 1.0 {
            let base = dy.atan2(dx);
            let spread = ((r1 - r2) / distance).clamp(-1.0, 1.0).acos();
            for angle in [base + spread, base - spread] {
                let (sin, cos) = angle.sin_cos();
                let from = (self.center.x as f64 + r1 * cos, self.center.y as f64 + r1 * sin);
                let to = (self.target.x as f64 + r2 * cos, self.target.y as f64 + r2 * sin);
                graphics.line((from.0 as f32, from.1 as f32), (to.0 as f32, to.1 as f32), argb(self.color, 170), 1.5);
            }
        }
        graphics.stroke_ellipse(Self::circle(self.center, self.radius), ink, 2.0, false);
        let lens = Self::circle(self.target, big);
        // A soft shadow lifts the lens off the capture.
        for step in 1..=3 {
            let spread = step as f32 * 1.5;
            graphics.stroke_ellipse(
                [lens[0] - spread + 1.0, lens[1] - spread + 2.0, lens[2] + spread + 1.0, lens[3] + spread + 2.0],
                argb(0, 40 - step as u8 * 10),
                1.5,
                false,
            );
        }
        graphics.fill_ellipse(lens, argb(0x00ff_ffff, 255));
        if self.pixels.width > 0 {
            let zoom = self.zoom as f32;
            let map = |value: i32, origin: i32, center: i32| center as f32 + (value - origin) as f32 * zoom;
            let target = [
                map(self.area.left, self.center.x, self.target.x),
                map(self.area.top, self.center.y, self.target.y),
                map(self.area.right, self.center.x, self.target.x),
                map(self.area.bottom, self.center.y, self.target.y),
            ];
            graphics.image_in_ellipse(&self.pixels, target, lens);
        }
        graphics.stroke_ellipse(lens, ink, 3.0, false);
    }
}

#[derive(Clone)]
enum Edit {
    /// Several edits undone and redone as one step.
    Batch(Vec<Edit>),
    Remove(usize),
    Insert(usize, Mark),
    /// The mark that was at the index before a change.
    Replace(usize, Mark),
    /// The selection rectangle before a move or resize.
    Region(RECT),
}

impl Edit {
    fn selected_text(&self) -> Option<usize> {
        match self {
            Self::Insert(index, Mark::Text(_)) | Self::Replace(index, Mark::Text(_)) => Some(*index),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct History {
    pub tool: Option<Tool>,
    pub draft: Option<Draft>,
    marks: Vec<Mark>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
}

impl History {
    pub fn has_marks(&self) -> bool {
        !self.marks.is_empty()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Records a selection move or resize so it can be undone like a mark.
    pub fn record_region(&mut self, previous: RECT) {
        self.redo.clear();
        self.undo.push(Edit::Region(previous));
    }

    /// The value the next step number gets: one more than the largest so far.
    pub fn next_number(&self) -> u32 {
        self.marks
            .iter()
            .filter_map(|mark| match mark {
                Mark::Number { value, .. } => Some(*value),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 1
    }

    pub fn push(&mut self, mark: Mark) -> usize {
        self.redo.clear();
        self.undo.push(Edit::Remove(self.marks.len()));
        self.marks.push(mark);
        self.marks.len() - 1
    }

    pub fn mark(&self, index: usize) -> Option<&Mark> {
        self.marks.get(index)
    }

    /// The topmost shape under `point` that can be selected with `tool`
    /// (any shape when no tool is active), and the part that was hit.
    pub fn hit_mark(&self, point: POINT, tool: Option<Tool>) -> Option<(usize, Grip)> {
        self.marks.iter().enumerate().rev().find_map(|(index, mark)| {
            (mark.selectable() && tool.is_none_or(|tool| tool == mark.tool()))
                .then(|| mark.hit(point, false))
                .flatten()
                .map(|grip| (index, grip))
        })
    }

    /// Shows `mark` in place of the one at `index` without recording it,
    /// for live drags; [`History::replace`] records the final state.
    pub fn set(&mut self, index: usize, mark: Mark) {
        if let Some(slot) = self.marks.get_mut(index) {
            *slot = mark;
        }
    }

    /// Replaces the mark at `index`; undo brings back the previous one.
    pub fn replace(&mut self, index: usize, mark: Mark) {
        if let Some(slot) = self.marks.get_mut(index) {
            let previous = std::mem::replace(slot, mark);
            self.redo.clear();
            self.undo.push(Edit::Replace(index, previous));
        }
    }

    /// The topmost mark under `point` that the select tool can pick.
    pub fn hit_any(&self, point: POINT) -> Option<usize> {
        self.marks.iter().enumerate().rev().find_map(|(index, mark)| mark.touches(point).then_some(index))
    }

    /// Indices of the marks whose drawing overlaps `area`.
    pub fn in_area(&self, area: RECT) -> Vec<usize> {
        self.marks
            .iter()
            .enumerate()
            .filter(|(_, mark)| mark.movable())
            .filter(|(_, mark)| crate::selection_geometry::intersect(mark.extent(), area).is_some())
            .map(|(index, _)| index)
            .collect()
    }

    /// Replaces several marks as one undo step.
    pub fn replace_many(&mut self, changes: Vec<(usize, Mark)>) {
        let mut edits = Vec::new();
        for (index, mark) in changes {
            if let Some(slot) = self.marks.get_mut(index) {
                edits.push(Edit::Replace(index, std::mem::replace(slot, mark)));
            }
        }
        if !edits.is_empty() {
            self.redo.clear();
            self.undo.push(Edit::Batch(edits));
        }
    }

    /// Removes several marks as one undo step.
    pub fn remove_many(&mut self, indices: &[usize]) {
        let mut indices = indices.to_vec();
        indices.sort_unstable_by(|a, b| b.cmp(a));
        indices.dedup();
        let count = self.marks.len();
        let mut edits = Vec::new();
        for index in indices.into_iter().filter(|&index| index < count) {
            edits.push(Edit::Insert(index, self.marks.remove(index)));
        }
        if !edits.is_empty() {
            self.redo.clear();
            self.undo.push(Edit::Batch(edits));
        }
    }

    pub fn remove(&mut self, index: usize) {
        if index < self.marks.len() {
            let mark = self.marks.remove(index);
            self.redo.clear();
            self.undo.push(Edit::Insert(index, mark));
        }
    }

    pub fn text(&self, index: usize) -> Option<&TextMark> {
        match self.marks.get(index)? {
            Mark::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn text_mut(&mut self, index: usize) -> Option<&mut TextMark> {
        match self.marks.get_mut(index)? {
            Mark::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn hit_text(&self, point: POINT) -> Option<usize> {
        self.marks
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, mark)| match mark {
                Mark::Text(text) if text.contains(point) => Some(index),
                _ => None,
            })
    }

    pub fn replace_text(&mut self, index: usize, text: TextMark) {
        if self.text(index).is_some_and(|previous| *previous != text) {
            self.replace(index, Mark::Text(text));
        }
    }

    pub fn remove_text(&mut self, index: usize) {
        if self.text(index).is_some() {
            self.remove(index);
        }
    }

    /// Undoes one step. `region` is the live selection rectangle, which a
    /// region step swaps. Returns the text box to reselect, if any.
    pub fn undo(&mut self, region: &mut RECT) -> Option<usize> {
        let edit = self.undo.pop()?;
        let selected = edit.selected_text();
        let inverse = self.apply(edit, region);
        self.redo.push(inverse);
        selected
    }

    pub fn redo(&mut self, region: &mut RECT) -> Option<usize> {
        let edit = self.redo.pop()?;
        let selected = edit.selected_text();
        let inverse = self.apply(edit, region);
        self.undo.push(inverse);
        selected
    }

    fn apply(&mut self, edit: Edit, region: &mut RECT) -> Edit {
        match edit {
            Edit::Remove(index) => Edit::Insert(index, self.marks.remove(index)),
            Edit::Insert(index, mark) => {
                self.marks.insert(index, mark);
                Edit::Remove(index)
            }
            Edit::Replace(index, previous) => Edit::Replace(index, std::mem::replace(&mut self.marks[index], previous)),
            Edit::Region(previous) => Edit::Region(std::mem::replace(region, previous)),
            Edit::Batch(edits) => {
                let mut inverse = Vec::with_capacity(edits.len());
                for edit in edits.into_iter().rev() {
                    inverse.push(self.apply(edit, region));
                }
                Edit::Batch(inverse)
            }
        }
    }

    pub unsafe fn paint(&self, dc: HDC, clip: RECT, preview: bool, hidden_text: Option<usize>) {
        let saved = SaveDC(dc);
        if saved == 0 {
            return;
        }
        IntersectClipRect(dc, clip.left, clip.top, clip.right, clip.bottom);
        for (index, mark) in self.marks.iter().enumerate() {
            if hidden_text != Some(index) {
                mark.paint(dc);
            }
        }
        if let Some(draft) = self.draft.as_ref().filter(|_| preview) {
            let style = draft.style;
            match draft.tool {
                Tool::Brush => stroke(dc, &draft.points, style.brush_width, style.brush_color),
                Tool::Mosaic => draft.mosaic.paint(dc),
                Tool::Arrow => arrow(dc, draft.start, draft.end, style.arrow_width, style.arrow_color),
                Tool::Number => number(dc, draft.end, self.next_number(), style.number_size, style.number_color),
                Tool::Rectangle => rectangle(
                    dc,
                    draft.rect(),
                    style.rect_width,
                    style.rect_color,
                    style.rect_round,
                    style.rect_dashed,
                ),
                Tool::Magnify => {
                    if let Some(graphics) = Graphics::new(dc) {
                        let radius = draft.magnify_radius();
                        graphics.stroke_ellipse(Lens::circle(draft.start, radius), argb(style.magnify_color, 255), 2.0, true);
                    }
                }
                Tool::Text => outline(dc, draft.rect()),
                Tool::Select => {}
            }
        }
        RestoreDC(dc, saved);
    }
}

impl Draft {
    pub fn new(tool: Tool, point: POINT, style: Style, clip: RECT, source: &Desktop) -> Self {
        let mut draft = Self {
            tool,
            style,
            start: point,
            end: point,
            points: vec![point],
            mosaic: Mosaic::default(),
        };
        if tool == Tool::Mosaic {
            draft.mosaic.extend(
                source,
                clip,
                point,
                point,
                style.mosaic_width,
                style.mosaic_block,
            );
        }
        draft
    }

    pub fn update(&mut self, point: POINT, clip: RECT, source: &Desktop) -> RECT {
        let point = POINT {
            x: point.x.clamp(clip.left, clip.right - 1),
            y: point.y.clamp(clip.top, clip.bottom - 1),
        };
        let previous = self.end;
        let old_rect = self.rect();
        let old_radius = self.magnify_radius();
        if (point.x, point.y) != (previous.x, previous.y) {
            self.end = point;
            match self.tool {
                Tool::Brush => self.points.push(point),
                Tool::Mosaic => self.mosaic.extend(
                    source,
                    clip,
                    previous,
                    point,
                    self.style.mosaic_width,
                    self.style.mosaic_block,
                ),
                _ => {}
            }
        }
        match self.tool {
            Tool::Brush | Tool::Mosaic => self.stroke_bounds(previous, point),
            // The number follows the pointer until release.
            Tool::Number => join(self.stroke_bounds(previous, previous), self.stroke_bounds(point, point)),
            Tool::Arrow => padded(join(old_rect, self.rect()), arrow_head(self.style.arrow_width) + 2),
            Tool::Rectangle => padded(join(old_rect, self.rect()), self.style.rect_width / 2 + 3),
            Tool::Magnify => padded(between(self.start, self.start), old_radius.max(self.magnify_radius()) + 3),
            Tool::Text | Tool::Select => padded(join(old_rect, self.rect()), 3),
        }
    }

    /// The circle a lens draft covers: dragged out from its center.
    fn magnify_radius(&self) -> i32 {
        let (dx, dy) = (f64::from(self.end.x - self.start.x), f64::from(self.end.y - self.start.y));
        let radius = (dx * dx + dy * dy).sqrt().round() as i32;
        if radius < Lens::MIN_RADIUS {
            Lens::DEFAULT_RADIUS
        } else {
            radius
        }
    }

    pub fn start_bounds(&self) -> RECT {
        self.stroke_bounds(self.start, self.start)
    }

    fn stroke_bounds(&self, a: POINT, b: POINT) -> RECT {
        let pad = match self.tool {
            Tool::Brush => self.style.brush_width / 2 + 2,
            Tool::Mosaic => self.style.mosaic_width / 2 + 2,
            Tool::Number => self.style.number_size / 2 + 3,
            Tool::Arrow => arrow_head(self.style.arrow_width) + 2,
            Tool::Magnify => self.magnify_radius() + 3,
            Tool::Rectangle => self.style.rect_width / 2 + 3,
            Tool::Text | Tool::Select => 3,
        };
        padded(between(a, b), pad)
    }

    fn rect(&self) -> RECT {
        between(self.start, self.end)
    }

    pub fn text_rect(&self, clip: RECT) -> RECT {
        let rect = self.rect();
        if rect.right - rect.left >= 24 && rect.bottom - rect.top >= 20 {
            return rect;
        }
        let left = self.start.x.min((clip.right - 180).max(clip.left));
        let top = self.start.y.min((clip.bottom - 68).max(clip.top));
        RECT {
            left,
            top,
            right: (left + 180).min(clip.right),
            bottom: (top + 68).min(clip.bottom),
        }
    }

    /// `next_number` is the value a step number gets; a lens copies the
    /// capture under it from `source`, inside `clip`.
    pub fn finish(self, next_number: u32, clip: RECT, source: &Desktop) -> Option<Mark> {
        let style = self.style;
        match self.tool {
            Tool::Rectangle => ((self.start.x, self.start.y) != (self.end.x, self.end.y)).then(|| Mark::Rectangle {
                rect: self.rect(),
                width: style.rect_width,
                color: style.rect_color,
                round: style.rect_round,
                dashed: style.rect_dashed,
            }),
            Tool::Brush => Some(Mark::Brush(self.points, style.brush_width, style.brush_color)),
            Tool::Mosaic => self.mosaic.finish().map(Mark::Mosaic),
            // A click or a tiny drag is not an arrow.
            Tool::Arrow => ((self.end.x - self.start.x).pow(2) + (self.end.y - self.start.y).pow(2) >= 64).then_some(
                Mark::Arrow {
                    from: self.start,
                    to: self.end,
                    width: style.arrow_width,
                    color: style.arrow_color,
                },
            ),
            Tool::Number => Some(Mark::Number {
                center: self.end,
                value: next_number,
                size: style.number_size,
                color: style.number_color,
            }),
            Tool::Magnify => Some(Mark::Magnify(Lens::new(
                self.start,
                self.magnify_radius(),
                style.magnify_zoom,
                style.magnify_color,
                clip,
                source,
            ))),
            Tool::Text | Tool::Select => None,
        }
    }
}

impl Mark {
    unsafe fn paint(&self, dc: HDC) {
        match self {
            Self::Rectangle { rect, width, color, round, dashed } => rectangle(dc, *rect, *width, *color, *round, *dashed),
            Self::Brush(points, width, color) => stroke(dc, points, *width, *color),
            Self::Mosaic(mosaic) => mosaic.paint(dc),
            Self::Text(text) => text.paint(dc),
            Self::Arrow { from, to, width, color } => arrow(dc, *from, *to, *width, *color),
            Self::Number { center, value, size, color } => number(dc, *center, *value, *size, *color),
            Self::Magnify(lens) => lens.paint(dc),
        }
    }

    pub fn tool(&self) -> Tool {
        match self {
            Self::Rectangle { .. } => Tool::Rectangle,
            Self::Text(_) => Tool::Text,
            Self::Brush(..) => Tool::Brush,
            Self::Mosaic(_) => Tool::Mosaic,
            Self::Arrow { .. } => Tool::Arrow,
            Self::Number { .. } => Tool::Number,
            Self::Magnify(_) => Tool::Magnify,
        }
    }

    /// Everything but a mosaic, which belongs to the pixels under it.
    pub fn movable(&self) -> bool {
        !matches!(self, Self::Mosaic(_))
    }

    /// Whether a click at `point` lands on this mark, text boxes included.
    pub fn touches(&self, point: POINT) -> bool {
        match self {
            Self::Text(text) => text.contains(point),
            Self::Mosaic(_) => false,
            _ => self.hit(point, false).is_some(),
        }
    }

    /// The area the mark itself paints, without selection chrome.
    pub fn extent(&self) -> RECT {
        let bounds = self.bounds();
        let pad = HANDLE_HALF as i32 + 4;
        RECT { left: bounds.left + pad, top: bounds.top + pad, right: bounds.right - pad, bottom: bounds.bottom - pad }
    }

    /// The mark shifted by (`dx`, `dy`).
    pub fn moved(&self, dx: i32, dy: i32, clip: RECT, source: &Desktop) -> Self {
        match self {
            Self::Text(text) => Self::Text(text.transform_drag(
                crate::text_mark::TextHandle::Move,
                POINT::default(),
                POINT { x: dx, y: dy },
            )),
            Self::Mosaic(_) => self.clone(),
            _ => self.dragged(Grip::Move, POINT::default(), POINT { x: dx, y: dy }, clip, source),
        }
    }

    /// Text boxes have their own frame; a mosaic is part of the pixels.
    fn selectable(&self) -> bool {
        !matches!(self, Self::Text(_) | Self::Mosaic(_))
    }

    /// Everything the mark and its selection frame paint, for repainting.
    pub fn bounds(&self) -> RECT {
        let painted = match self {
            Self::Rectangle { rect, width, .. } => padded(*rect, width / 2 + 2),
            Self::Brush(points, width, _) => padded(points_bounds(points), width / 2 + 2),
            Self::Arrow { from, to, width, .. } => padded(between(*from, *to), arrow_head(*width) + 2),
            Self::Number { center, size, .. } => padded(between(*center, *center), size / 2 + 3),
            Self::Magnify(lens) => lens.bounds(),
            Self::Text(text) => text.bounds(),
            Self::Mosaic(_) => RECT::default(),
        };
        padded(painted, HANDLE_HALF as i32 + 4)
    }

    /// The draggable handles of a selected mark.
    fn grips(&self) -> Vec<(Grip, POINT)> {
        match self {
            Self::Rectangle { rect, .. } => {
                let (x0, y0, x1, y1) = (rect.left, rect.top, rect.right - 1, rect.bottom - 1);
                let (mx, my) = ((x0 + x1) / 2, (y0 + y1) / 2);
                [(-1, -1, x0, y0), (0, -1, mx, y0), (1, -1, x1, y0), (1, 0, x1, my), (1, 1, x1, y1), (0, 1, mx, y1), (-1, 1, x0, y1), (-1, 0, x0, my)]
                    .into_iter()
                    .map(|(gx, gy, x, y)| (Grip::Resize(gx, gy), POINT { x, y }))
                    .collect()
            }
            Self::Arrow { from, to, .. } => vec![(Grip::Start, *from), (Grip::End, *to)],
            Self::Number { center, size, .. } => {
                let offset = (f64::from(*size) / 2.0 / std::f64::consts::SQRT_2).round() as i32;
                vec![(Grip::Radius, POINT { x: center.x + offset, y: center.y + offset })]
            }
            Self::Magnify(lens) => vec![(Grip::Radius, lens.radius_handle())],
            _ => Vec::new(),
        }
    }

    /// The part of the mark under `point`, if any. Handles only count on a
    /// `selected` mark, where they are drawn.
    pub fn hit(&self, point: POINT, selected: bool) -> Option<Grip> {
        let near = |handle: POINT| (handle.x - point.x).abs() <= GRAB && (handle.y - point.y).abs() <= GRAB;
        if let Some((grip, _)) = self.grips().into_iter().filter(|_| selected).find(|(_, handle)| near(*handle)) {
            return Some(grip);
        }
        let inside_circle = |center: POINT, radius: i32| {
            let (dx, dy) = (i64::from(point.x - center.x), i64::from(point.y - center.y));
            dx * dx + dy * dy <= i64::from(radius).pow(2)
        };
        let body = match self {
            Self::Rectangle { rect, width, .. } => {
                // Only the outline: drawing a new shape inside an old one must still work.
                let reach = f64::from((width / 2 + 3).max(GRAB));
                let edges = [
                    (POINT { x: rect.left, y: rect.top }, POINT { x: rect.right - 1, y: rect.top }),
                    (POINT { x: rect.right - 1, y: rect.top }, POINT { x: rect.right - 1, y: rect.bottom - 1 }),
                    (POINT { x: rect.right - 1, y: rect.bottom - 1 }, POINT { x: rect.left, y: rect.bottom - 1 }),
                    (POINT { x: rect.left, y: rect.bottom - 1 }, POINT { x: rect.left, y: rect.top }),
                ];
                edges.iter().any(|&(a, b)| segment_distance(point, a, b) <= reach)
            }
            Self::Brush(points, width, _) => {
                let reach = f64::from(width / 2 + 3).max(f64::from(GRAB) - 1.0);
                match points.as_slice() {
                    [single] => segment_distance(point, *single, *single) <= reach,
                    _ => points.windows(2).any(|pair| segment_distance(point, pair[0], pair[1]) <= reach),
                }
            }
            Self::Arrow { from, to, width, .. } => {
                segment_distance(point, *from, *to) <= f64::from((width / 2 + 3).max(GRAB))
                    || inside_circle(*to, arrow_head(*width) / 2 + 2)
            }
            Self::Number { center, size, .. } => inside_circle(*center, size / 2 + 1),
            Self::Magnify(lens) => {
                if inside_circle(lens.target, lens.lens_radius()) {
                    return Some(Grip::Lens);
                }
                inside_circle(lens.center, lens.radius + 2)
            }
            Self::Text(_) | Self::Mosaic(_) => false,
        };
        body.then_some(Grip::Move)
    }

    /// The mark after dragging `grip` from `anchor` to `point`. Starting
    /// from the mark as it was keeps rounding from accumulating.
    pub fn dragged(&self, grip: Grip, anchor: POINT, point: POINT, clip: RECT, source: &Desktop) -> Self {
        let (dx, dy) = (point.x - anchor.x, point.y - anchor.y);
        let shift = |p: POINT| POINT { x: p.x + dx, y: p.y + dy };
        let mut next = self.clone();
        match (&mut next, grip) {
            (Self::Rectangle { rect, .. }, Grip::Move) => {
                *rect = RECT { left: rect.left + dx, top: rect.top + dy, right: rect.right + dx, bottom: rect.bottom + dy };
            }
            (Self::Rectangle { rect, .. }, Grip::Resize(gx, gy)) => {
                let original = rect_of(self);
                let mut moved = original;
                if gx < 0 {
                    moved.left += dx;
                } else if gx > 0 {
                    moved.right += dx;
                }
                if gy < 0 {
                    moved.top += dy;
                } else if gy > 0 {
                    moved.bottom += dy;
                }
                // Dragging past the opposite edge flips the box instead of collapsing it.
                let (left, right) = (moved.left.min(moved.right), moved.left.max(moved.right));
                let (top, bottom) = (moved.top.min(moved.bottom), moved.top.max(moved.bottom));
                *rect = RECT { left, top, right: right.max(left + 1), bottom: bottom.max(top + 1) };
            }
            (Self::Brush(points, ..), Grip::Move) => {
                for p in points.iter_mut() {
                    *p = shift(*p);
                }
            }
            (Self::Arrow { from, to, .. }, Grip::Move) => {
                *from = shift(*from);
                *to = shift(*to);
            }
            (Self::Arrow { from, .. }, Grip::Start) => *from = shift(*from),
            (Self::Arrow { to, .. }, Grip::End) => *to = shift(*to),
            (Self::Number { center, .. }, Grip::Move) => *center = shift(*center),
            (Self::Number { center, size, .. }, Grip::Radius) => {
                let (x, y) = (f64::from(point.x - center.x), f64::from(point.y - center.y));
                *size = (((x * x + y * y).sqrt() * 2.0).round() as i32).clamp(16, 96);
            }
            (Self::Magnify(lens), Grip::Move) => {
                lens.center = shift(lens.center);
                lens.target = shift(lens.target);
                lens.capture(clip, source);
            }
            (Self::Magnify(lens), Grip::Lens) => lens.target = shift(lens.target),
            (Self::Magnify(lens), Grip::Radius) => {
                let (x, y) = (f64::from(point.x - lens.center.x), f64::from(point.y - lens.center.y));
                lens.radius = ((x * x + y * y).sqrt().round() as i32).clamp(Lens::MIN_RADIUS, 200);
                lens.capture(clip, source);
            }
            _ => {}
        }
        next
    }

    /// Fills the style fields of this mark's tool from the mark.
    pub fn style_into(&self, style: &mut Style) {
        match *self {
            Self::Rectangle { width, color, round, dashed, .. } => {
                style.rect_width = width;
                style.rect_color = color;
                style.rect_round = round;
                style.rect_dashed = dashed;
            }
            Self::Brush(_, width, color) => {
                style.brush_width = width;
                style.brush_color = color;
            }
            Self::Arrow { width, color, .. } => {
                style.arrow_width = width;
                style.arrow_color = color;
            }
            Self::Number { size, color, .. } => {
                style.number_size = size;
                style.number_color = color;
            }
            Self::Magnify(ref lens) => {
                style.magnify_zoom = lens.zoom;
                style.magnify_color = lens.color;
            }
            Self::Text(_) | Self::Mosaic(_) => {}
        }
    }

    /// This mark restyled with the fields of its tool in `style`.
    pub fn with_style(&self, style: &Style) -> Self {
        let mut next = self.clone();
        match &mut next {
            Self::Rectangle { width, color, round, dashed, .. } => {
                (*width, *color, *round, *dashed) = (style.rect_width, style.rect_color, style.rect_round, style.rect_dashed);
            }
            Self::Brush(_, width, color) => (*width, *color) = (style.brush_width, style.brush_color),
            Self::Arrow { width, color, .. } => (*width, *color) = (style.arrow_width, style.arrow_color),
            Self::Number { size, color, .. } => (*size, *color) = (style.number_size, style.number_color),
            Self::Magnify(lens) => (lens.zoom, lens.color) = (style.magnify_zoom, style.magnify_color),
            Self::Text(_) | Self::Mosaic(_) => {}
        }
        next
    }

    /// The selection chrome: a dashed outline and square handles, like text boxes.
    pub unsafe fn paint_frame(&self, dc: HDC) {
        let Some(graphics) = Graphics::new(dc) else { return };
        let (Some(dashed), Some(solid)) = (graphics.dashed_pen(BORDER_BLUE, 1.0), graphics.pen(BORDER_BLUE, 1.0)) else {
            return;
        };
        let point = |p: POINT| PointF { X: p.x as f32, Y: p.y as f32 };
        let outline = |rect: RECT| {
            let corners = [
                PointF { X: rect.left as f32, Y: rect.top as f32 },
                PointF { X: rect.right as f32, Y: rect.top as f32 },
                PointF { X: rect.right as f32, Y: rect.bottom as f32 },
                PointF { X: rect.left as f32, Y: rect.bottom as f32 },
            ];
            graphics.lines(&dashed, &[corners[0], corners[1], corners[2], corners[3], corners[0]]);
        };
        match self {
            // The handles sit on the outline itself.
            Self::Rectangle { .. } | Self::Arrow { .. } => {}
            Self::Brush(points, width, _) => outline(padded(points_bounds(points), width / 2 + 3)),
            Self::Number { center, size, .. } => {
                graphics.stroke_ellipse(Lens::circle(*center, size / 2 + 3), argb(BORDER_BLUE, 255), 1.0, true)
            }
            Self::Magnify(lens) => {
                graphics.stroke_ellipse(Lens::circle(lens.center, lens.radius + 3), argb(BORDER_BLUE, 255), 1.0, true);
                graphics.stroke_ellipse(
                    Lens::circle(lens.target, lens.lens_radius() + 4),
                    argb(BORDER_BLUE, 255),
                    1.0,
                    true,
                );
            }
            Self::Text(_) | Self::Mosaic(_) => return,
        }
        for (_, handle) in self.grips() {
            graphics.square_handle(&solid, point(handle), HANDLE_HALF);
        }
    }
}

fn rect_of(mark: &Mark) -> RECT {
    match mark {
        Mark::Rectangle { rect, .. } => *rect,
        _ => RECT::default(),
    }
}

fn points_bounds(points: &[POINT]) -> RECT {
    let first = points.first().copied().unwrap_or_default();
    points.iter().fold(between(first, first), |bounds, &p| join(bounds, between(p, p)))
}

/// Distance from `point` to the segment `a`–`b`.
fn segment_distance(point: POINT, a: POINT, b: POINT) -> f64 {
    let (px, py) = (f64::from(point.x), f64::from(point.y));
    let (ax, ay, bx, by) = (f64::from(a.x), f64::from(a.y), f64::from(b.x), f64::from(b.y));
    let (dx, dy) = (bx - ax, by - ay);
    let length = dx * dx + dy * dy;
    let t = if length == 0.0 { 0.0 } else { (((px - ax) * dx + (py - ay) * dy) / length).clamp(0.0, 1.0) };
    let (x, y) = (ax + t * dx, ay + t * dy);
    ((px - x).powi(2) + (py - y).powi(2)).sqrt()
}

/// A box outline, square or rounded, solid or dashed.
unsafe fn rectangle(dc: HDC, rect: RECT, width: i32, color: u32, round: bool, dashed: bool) {
    let Some(graphics) = Graphics::new(dc) else {
        outline(dc, rect);
        return;
    };
    let bounds = [rect.left as f32 + 0.5, rect.top as f32 + 0.5, rect.right as f32 - 0.5, rect.bottom as f32 - 0.5];
    let radius = if round { (8 + width * 2) as f32 } else { 0.0 };
    graphics.stroke_shape(bounds, radius, argb(color, 255), width as f32, dashed);
}

/// Length of an arrowhead for a shaft of `width`.
fn arrow_head(width: i32) -> i32 {
    (width * 4 + 6).max(16)
}

/// A straight shaft ending in a solid, slightly swept-back head.
unsafe fn arrow(dc: HDC, from: POINT, to: POINT, width: i32, color: u32) {
    let Some(graphics) = Graphics::new(dc) else { return };
    let (dx, dy) = ((to.x - from.x) as f32, (to.y - from.y) as f32);
    let length = (dx * dx + dy * dy).sqrt();
    if length < 1.0 {
        return;
    }
    let (ux, uy) = (dx / length, dy / length);
    let head = (arrow_head(width) as f32).min(length);
    let half = head * 0.5;
    let (tx, ty) = (to.x as f32, to.y as f32);
    let (bx, by) = (tx - ux * head, ty - uy * head);
    // The notch keeps the head from looking like a plain triangle.
    let (nx, ny) = (tx - ux * head * 0.78, ty - uy * head * 0.78);
    let fill = argb(color, 255);
    graphics.fill_segments(
        &[
            Seg::M(tx, ty),
            Seg::L(bx - uy * half, by + ux * half),
            Seg::L(nx, ny),
            Seg::L(bx + uy * half, by - ux * half),
            Seg::Z,
        ],
        1.0,
        fill,
    );
    // Round caps on the tail; the end tucks under the head.
    graphics.stroke_segments(
        &[Seg::M(from.x as f32, from.y as f32), Seg::L(nx + ux, ny + uy)],
        1.0,
        fill,
        width as f32,
    );
}

/// A filled circle with its number in white (dark on light colors).
unsafe fn number(dc: HDC, center: POINT, value: u32, size: i32, color: u32) {
    let radius = size as f32 / 2.0;
    let (cx, cy) = (center.x as f32, center.y as f32);
    let light = contrast(color) != 0x00ff_ffff;
    if let Some(graphics) = Graphics::new(dc) {
        graphics.fill_ellipse([cx - radius, cy - radius, cx + radius, cy + radius], argb(color, 255));
        // A thin contrasting ring keeps it readable on any background.
        let ring = if light { argb(0x0022_2222, 150) } else { argb(0x00ff_ffff, 200) };
        graphics.stroke_round(
            [cx - radius, cy - radius, cx + radius, cy + radius],
            radius,
            ring,
            (size as f32 / 14.0).max(1.5),
        );
    }
    let digits = value.to_string();
    let scale = if digits.len() > 1 { 0.5 } else { 0.62 };
    let font = CreateFontW(
        -((size as f32 * scale).round() as i32),
        0,
        0,
        0,
        FW_BOLD as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        ANTIALIASED_QUALITY as u32,
        DEFAULT_PITCH as u32,
        windows_sys::core::w!("Segoe UI"),
    );
    let previous = SelectObject(dc, font);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, if light { 0x0022_2222 } else { 0x00ff_ffff });
    let mut rect = RECT {
        left: center.x - size,
        top: center.y - size,
        right: center.x + size + 1,
        bottom: center.y + size + 1,
    };
    let text = wide(&digits);
    DrawTextW(dc, text.as_ptr(), -1, &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(dc, previous);
    DeleteObject(font);
}

fn between(a: POINT, b: POINT) -> RECT {
    RECT {
        left: a.x.min(b.x),
        top: a.y.min(b.y),
        right: a.x.max(b.x) + 1,
        bottom: a.y.max(b.y) + 1,
    }
}

fn join(a: RECT, b: RECT) -> RECT {
    RECT {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

fn padded(rect: RECT, n: i32) -> RECT {
    RECT {
        left: rect.left - n,
        top: rect.top - n,
        right: rect.right + n,
        bottom: rect.bottom + n,
    }
}

unsafe fn outline(dc: HDC, rect: RECT) {
    let points = [
        POINT {
            x: rect.left,
            y: rect.top,
        },
        POINT {
            x: rect.right - 1,
            y: rect.top,
        },
        POINT {
            x: rect.right - 1,
            y: rect.bottom - 1,
        },
        POINT {
            x: rect.left,
            y: rect.bottom - 1,
        },
        POINT {
            x: rect.left,
            y: rect.top,
        },
    ];
    stroke(dc, &points, 3, INK);
}

unsafe fn stroke(dc: HDC, points: &[POINT], width: i32, color: u32) {
    if points.len() == 1 {
        let point = points[0];
        let half = width / 2;
        let brush = SelectObject(dc, GetStockObject(DC_BRUSH));
        let pen = SelectObject(dc, GetStockObject(NULL_PEN));
        SetDCBrushColor(dc, color);
        Ellipse(
            dc,
            point.x - half,
            point.y - half,
            point.x - half + width,
            point.y - half + width,
        );
        SelectObject(dc, pen);
        SelectObject(dc, brush);
        return;
    }
    let brush = LOGBRUSH {
        lbStyle: BS_SOLID,
        lbColor: color,
        lbHatch: 0,
    };
    let pen = ExtCreatePen(
        (PS_GEOMETRIC | PS_SOLID | PS_ENDCAP_ROUND | PS_JOIN_ROUND) as u32,
        width as u32,
        &brush,
        0,
        null_mut(),
    );
    if !pen.is_null() {
        let previous = SelectObject(dc, pen);
        Polyline(dc, points.as_ptr(), points.len() as i32);
        SelectObject(dc, previous);
        DeleteObject(pen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{capture::Surface, text_mark::TextHandle};

    #[test]
    fn undo_and_redo_keep_text_creation_edits_and_transforms_separate() {
        let mut history = History::default();
        let rect = RECT {
            left: 10,
            top: 10,
            right: 110,
            bottom: 70,
        };
        let mut original = TextMark::new(rect, 20, INK);
        original.text = vec![65];
        let text_index = history.push(Mark::Text(original.clone()));
        history.push(Mark::Rectangle { rect, width: 3, color: INK, round: false, dashed: false });

        let mut edited = original.clone();
        edited.text.push(66);
        history.replace_text(text_index, edited.clone());
        let moved = edited.transform_drag(
            TextHandle::Move,
            POINT { x: 30, y: 30 },
            POINT { x: 50, y: 35 },
        );
        history.replace_text(text_index, moved.clone());

        assert_eq!(history.undo(&mut RECT::default()), Some(text_index));
        assert!(history.text(text_index) == Some(&edited));
        assert_eq!(history.undo(&mut RECT::default()), Some(text_index));
        assert!(history.text(text_index) == Some(&original));
        assert_eq!(history.marks.len(), 2);
        history.redo(&mut RECT::default());
        history.redo(&mut RECT::default());
        assert!(history.text(text_index) == Some(&moved));
        history.remove_text(text_index);
        assert_eq!(history.marks.len(), 1);
        assert_eq!(history.undo(&mut RECT::default()), Some(text_index));
        assert!(history.text(text_index) == Some(&moved));
    }

    /// Arrows and step numbers in every color. Run with `<test binary> annotation --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        unsafe {
            let (width, height) = (520, 200);
            let clip = RECT { right: width, bottom: height, ..Default::default() };
            let surface = Surface::new(null_mut(), width, height).unwrap();
            FillRect(surface.dc, &clip, GetStockObject(WHITE_BRUSH));
            let colors = [INK, 0x0031_91ff, 0x0000_c8f3, 0x0061_a825, 0x00ed_7a25, 0x00c4_4e84, 0x0022_2222, 0x00ff_ffff];
            let mut history = History::default();
            for (index, color) in colors.into_iter().enumerate() {
                let x = 30 + index as i32 * 62;
                history.push(Mark::Arrow {
                    from: POINT { x, y: 150 },
                    to: POINT { x: x + 40, y: 90 },
                    width: 2 + index as i32,
                    color,
                });
                history.push(Mark::Number { center: POINT { x: x + 10, y: 40 }, value: history.next_number(), size: 22 + index as i32 * 2, color });
            }
            history.push(Mark::Number { center: POINT { x: 490, y: 160 }, value: 12, size: 30, color: INK });
            history.paint(surface.dc, clip, false, None);
            GdiFlush();
            let mut image = Desktop { bounds: clip, surface }.crop(clip).unwrap();
            for pixel in image.pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            let folder = std::env::temp_dir().join("snapshot-preview");
            std::fs::create_dir_all(&folder).unwrap();
            image.save_png(&folder.join("annotation-arrow-number.png")).unwrap();
        }
    }

    /// A striped capture so a lens shows something to enlarge.
    unsafe fn striped(width: i32, height: i32) -> Desktop {
        let clip = RECT { right: width, bottom: height, ..Default::default() };
        let surface = Surface::new(null_mut(), width, height).unwrap();
        FillRect(surface.dc, &clip, GetStockObject(WHITE_BRUSH));
        for x in (0..width).step_by(8) {
            let stripe = RECT { left: x, top: 0, right: x + 4, bottom: height };
            SetDCBrushColor(surface.dc, 0x0020_8040);
            FillRect(surface.dc, &stripe, GetStockObject(DC_BRUSH));
        }
        GdiFlush();
        Desktop { bounds: clip, surface }
    }

    #[test]
    fn drawn_shapes_can_be_picked_moved_resized_and_restyled() {
        unsafe {
            let clip = RECT { right: 400, bottom: 300, ..Default::default() };
            let source = striped(400, 300);
            let mut history = History::default();
            let rect = RECT { left: 50, top: 40, right: 150, bottom: 120 };
            history.push(Mark::Rectangle { rect, width: 3, color: INK, round: false, dashed: false });
            history.push(Mark::Arrow { from: POINT { x: 200, y: 200 }, to: POINT { x: 300, y: 150 }, width: 4, color: INK });
            history.push(Mark::Number { center: POINT { x: 350, y: 60 }, value: 1, size: 28, color: INK });

            // The outline picks the box; its empty middle does not, so new boxes can start there.
            assert_eq!(history.hit_mark(POINT { x: 50, y: 80 }, None), Some((0, Grip::Move)));
            assert_eq!(history.hit_mark(POINT { x: 100, y: 80 }, None), None);
            assert_eq!(history.hit_mark(POINT { x: 149, y: 119 }, None), Some((0, Grip::Move)), "no handles until picked");
            assert_eq!(history.mark(0).unwrap().hit(POINT { x: 149, y: 119 }, true), Some(Grip::Resize(1, 1)));
            assert_eq!(history.mark(0).unwrap().hit(POINT { x: 50, y: 80 }, true), Some(Grip::Resize(-1, 0)));
            // Only shapes of the active tool are picked while drawing.
            assert_eq!(history.hit_mark(POINT { x: 50, y: 80 }, Some(Tool::Arrow)), None);
            assert_eq!(history.hit_mark(POINT { x: 250, y: 175 }, Some(Tool::Arrow)), Some((1, Grip::Move)));
            assert_eq!(history.mark(1).unwrap().hit(POINT { x: 300, y: 150 }, true), Some(Grip::End));
            assert_eq!(history.hit_mark(POINT { x: 352, y: 58 }, None), Some((2, Grip::Move)));

            let moved = history.mark(0).unwrap().dragged(Grip::Move, POINT { x: 50, y: 80 }, POINT { x: 70, y: 90 }, clip, &source);
            assert!(matches!(moved, Mark::Rectangle { rect: RECT { left: 70, top: 50, right: 170, bottom: 130 }, .. }));
            // Dragging a corner past the opposite one flips the box.
            let flipped = history.mark(0).unwrap().dragged(Grip::Resize(1, 1), POINT { x: 149, y: 119 }, POINT { x: 20, y: 20 }, clip, &source);
            let Mark::Rectangle { rect: flipped, .. } = flipped else { panic!() };
            assert!(flipped.left < flipped.right && flipped.top < flipped.bottom);
            assert_eq!((flipped.left, flipped.right), (21, 50));
            let tip = history.mark(1).unwrap().dragged(Grip::End, POINT { x: 300, y: 150 }, POINT { x: 320, y: 100 }, clip, &source);
            assert!(matches!(tip, Mark::Arrow { from: POINT { x: 200, y: 200 }, to: POINT { x: 320, y: 100 }, .. }));

            history.replace(0, moved);
            let mut style = Style::default();
            history.mark(0).unwrap().style_into(&mut style);
            style.rect_dashed = true;
            style.rect_color = 0x0061_a825;
            let restyled = history.mark(0).unwrap().with_style(&style);
            history.replace(0, restyled);
            assert!(matches!(history.mark(0), Some(Mark::Rectangle { dashed: true, color: 0x0061_a825, rect: RECT { left: 70, .. }, .. })));
            history.remove(2);
            assert!(history.mark(2).is_none());

            // Undo walks back through delete, restyle and move.
            history.undo(&mut RECT::default());
            assert!(matches!(history.mark(2), Some(Mark::Number { .. })));
            history.undo(&mut RECT::default());
            assert!(matches!(history.mark(0), Some(Mark::Rectangle { dashed: false, rect: RECT { left: 70, .. }, .. })));
            history.undo(&mut RECT::default());
            assert!(matches!(history.mark(0), Some(Mark::Rectangle { rect: RECT { left: 50, .. }, .. })));
        }
    }

    #[test]
    fn a_lens_enlarges_the_circle_beside_it_and_follows_its_source() {
        unsafe {
            let clip = RECT { right: 400, bottom: 300, ..Default::default() };
            let source = striped(400, 300);
            let mut draft = Draft::new(Tool::Magnify, POINT { x: 100, y: 200 }, Style::default(), clip, &source);
            draft.update(POINT { x: 130, y: 200 }, clip, &source);
            let Some(Mark::Magnify(lens)) = draft.finish(1, clip, &source) else { panic!("a lens") };
            assert_eq!((lens.radius, lens.zoom), (30, 2));
            // Up and to the right, clear of the circle and inside the selection.
            assert!(lens.target.x > 100 && lens.target.y < 200, "{:?}", (lens.target.x, lens.target.y));
            assert!(lens.target.y - lens.lens_radius() >= 0);
            assert_eq!((lens.pixels.width, lens.pixels.height), (60, 60));

            let mut history = History::default();
            history.push(Mark::Magnify(lens.clone()));
            let target = Surface::new(null_mut(), 400, 300).unwrap();
            FillRect(target.dc, &clip, GetStockObject(BLACK_BRUSH));
            history.paint(target.dc, clip, false, None);
            GdiFlush();
            // Inside the lens the stripes are twice as wide: 8 px green, 8 px white.
            let (cx, cy) = (lens.target.x, lens.target.y);
            let green: Vec<bool> = (-20..20).map(|dx| GetPixel(target.dc, cx + dx, cy) & 0xff < 0x90).collect();
            let edges = green.windows(2).filter(|pair| pair[0] != pair[1]).count();
            assert!((4..=6).contains(&edges), "about five stripe edges in 40 px: {edges}");

            // Grabbing the lens moves only the lens; the circle moves both.
            assert_eq!(history.hit_mark(lens.target, None), Some((0, Grip::Lens)));
            assert_eq!(history.mark(0).unwrap().hit(lens.radius_handle(), true), Some(Grip::Radius));
            let moved = history.mark(0).unwrap().dragged(Grip::Lens, lens.target, POINT { x: lens.target.x - 10, y: lens.target.y }, clip, &source);
            let Mark::Magnify(moved) = moved else { panic!() };
            assert_eq!((moved.center.x, moved.target.x), (100, lens.target.x - 10));
            let resized = history.mark(0).unwrap().dragged(Grip::Radius, lens.radius_handle(), POINT { x: 100 + 40, y: 200 }, clip, &source);
            let Mark::Magnify(resized) = resized else { panic!() };
            assert_eq!(resized.radius, 40);
            assert_eq!(resized.pixels.width, 80, "the capture follows the bigger circle");
        }
    }

    /// Rectangle styles, text looks and a lens. Run with `<test binary> annotation --ignored`.
    #[test]
    #[ignore]
    fn preview_styles() {
        unsafe {
            let (width, height) = (640, 260);
            let clip = RECT { right: width, bottom: height, ..Default::default() };
            let source = striped(width, height);
            let mut history = History::default();
            for (index, (round, dashed)) in [(false, false), (true, false), (false, true), (true, true)].into_iter().enumerate() {
                let left = 20 + index as i32 * 80;
                history.push(Mark::Rectangle { rect: RECT { left, top: 20, right: left + 64, bottom: 80 }, width: 3 + index as i32, color: [INK, 0x0061_a825, 0x00ed_7a25, 0x0022_2222][index], round, dashed });
            }
            for (index, look) in [Look::Plain, Look::Outline, Look::Fill].into_iter().enumerate() {
                let left = 20 + index as i32 * 110;
                let mut text = TextMark::new(RECT { left, top: 110, right: left + 100, bottom: 150 }, 22, INK);
                text.text = "标注 Text".encode_utf16().collect();
                text.look = look;
                history.push(Mark::Text(text));
            }
            history.push(Mark::Magnify(Lens::new(POINT { x: 420, y: 200 }, 26, 3, INK, clip, &source)));
            let selected = Mark::Arrow { from: POINT { x: 30, y: 230 }, to: POINT { x: 200, y: 200 }, width: 4, color: INK };
            history.push(selected.clone());
            let surface = Surface::new(null_mut(), width, height).unwrap();
            BitBlt(surface.dc, 0, 0, width, height, source.surface.dc, 0, 0, SRCCOPY);
            history.paint(surface.dc, clip, false, None);
            history.mark(history.marks.len() - 2).unwrap().paint_frame(surface.dc);
            selected.paint_frame(surface.dc);
            history.mark(0).unwrap().paint_frame(surface.dc);
            GdiFlush();
            let mut image = Desktop { bounds: clip, surface }.crop(clip).unwrap();
            for pixel in image.pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            let folder = std::env::temp_dir().join("snapshot-preview");
            std::fs::create_dir_all(&folder).unwrap();
            image.save_png(&folder.join("annotation-styles.png")).unwrap();
        }
    }

    #[test]
    fn step_numbers_count_up_and_follow_undo() {
        unsafe {
            let clip = RECT { right: 200, bottom: 120, ..Default::default() };
            let source = Desktop { bounds: clip, surface: Surface::new(null_mut(), 200, 120).unwrap() };
            let mut history = History::default();
            let place = |history: &mut History, x: i32| {
                let mut draft = Draft::new(Tool::Number, POINT { x, y: 40 }, Style::default(), clip, &source);
                // A drag moves the number before it lands.
                draft.update(POINT { x: x + 10, y: 50 }, clip, &source);
                history.push(draft.finish(history.next_number(), clip, &source).unwrap());
            };
            place(&mut history, 20);
            place(&mut history, 70);
            assert_eq!(history.next_number(), 3);
            assert!(matches!(history.marks[1], Mark::Number { value: 2, center: POINT { x: 80, y: 50 }, .. }));
            history.undo(&mut RECT::default());
            assert_eq!(history.next_number(), 2, "undo frees the number");
            place(&mut history, 120);
            assert!(matches!(history.marks[1], Mark::Number { value: 2, .. }));

            // Painted: a filled circle with a white digit in the middle.
            let target = Surface::new(null_mut(), 200, 120).unwrap();
            FillRect(target.dc, &clip, GetStockObject(WHITE_BRUSH));
            history.paint(target.dc, clip, false, None);
            GdiFlush();
            assert_eq!(GetPixel(target.dc, 30 - 10, 50), INK, "inside the first circle");
        }
    }

    #[test]
    fn arrows_need_a_real_drag_and_point_at_the_end() {
        unsafe {
            let clip = RECT { right: 200, bottom: 120, ..Default::default() };
            let source = Desktop { bounds: clip, surface: Surface::new(null_mut(), 200, 120).unwrap() };
            let click = Draft::new(Tool::Arrow, POINT { x: 50, y: 50 }, Style::default(), clip, &source);
            assert!(click.finish(1, clip, &source).is_none(), "a click is not an arrow");
            let mut draft = Draft::new(Tool::Arrow, POINT { x: 20, y: 60 }, Style::default(), clip, &source);
            let dirty = draft.update(POINT { x: 180, y: 60 }, clip, &source);
            assert!(dirty.right >= 180 + 11 && dirty.top <= 60 - 11, "the head is repainted");
            let mut history = History::default();
            history.push(draft.finish(1, clip, &source).unwrap());
            let target = Surface::new(null_mut(), 200, 120).unwrap();
            FillRect(target.dc, &clip, GetStockObject(WHITE_BRUSH));
            history.paint(target.dc, clip, false, None);
            GdiFlush();
            assert_eq!(GetPixel(target.dc, 60, 60), INK, "shaft");
            assert_eq!(GetPixel(target.dc, 172, 60), INK, "head");
            assert_eq!(GetPixel(target.dc, 168, 65), INK, "head is wider than the shaft");
            assert_eq!(GetPixel(target.dc, 60, 65), 0x00ff_ffff, "shaft stays thin");
        }
    }

    #[test]
    fn mosaic_stroke_covers_sparse_pointer_samples_and_only_the_brushed_area() {
        unsafe {
            let source = Surface::new(null_mut(), 120, 90).unwrap();
            let target = Surface::new(null_mut(), 120, 90).unwrap();
            let clip = RECT {
                right: 120,
                bottom: 90,
                ..Default::default()
            };
            FillRect(source.dc, &clip, GetStockObject(WHITE_BRUSH));
            let half_block = RECT {
                left: 12,
                top: 36,
                right: 18,
                bottom: 48,
            };
            FillRect(source.dc, &half_block, GetStockObject(BLACK_BRUSH));
            GdiFlush();
            let source = Desktop {
                bounds: clip,
                surface: source,
            };
            FillRect(target.dc, &clip, GetStockObject(BLACK_BRUSH));
            let mut draft = Draft::new(
                Tool::Mosaic,
                POINT { x: 15, y: 40 },
                Style::default(),
                clip,
                &source,
            );
            draft.update(POINT { x: 105, y: 40 }, clip, &source);
            let mut history = History::default();
            history.push(draft.finish(1, clip, &source).unwrap());
            history.paint(target.dc, clip, false, None);
            assert_eq!(GetPixel(target.dc, 15, 40), 0x0080_8080);
            assert_eq!(GetPixel(target.dc, 20, 40), 0x0080_8080);
            for x in 24..=105 {
                assert_eq!(GetPixel(target.dc, x, 40), 0x00ff_ffff);
                assert_eq!(GetPixel(target.dc, x, 72), 0);
            }
            assert_eq!(GetPixel(target.dc, 50, 24), 0x0080_8080);
            assert_eq!(GetPixel(target.dc, 50, 22), 0);
            let export = RECT {
                left: 20,
                top: 20,
                right: 110,
                bottom: 70,
            };
            let image = source
                .composite_crop(export, |dc| history.paint(dc, export, false, None))
                .unwrap();
            let offset = 20 * image.width as usize * 4;
            assert_eq!(image.pixels[offset..offset + 3], [128; 3]);
            history.undo(&mut RECT::default());
            assert!(!history.has_marks());
        }
    }
}
