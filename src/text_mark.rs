use windows_sys::Win32::{
    Foundation::{POINT, RECT},
    Graphics::{Gdi::*, GdiPlus::PointF},
    UI::WindowsAndMessaging::{
        IDC_HAND, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE,
    },
};

use crate::{capture::Image, selection_geometry::BORDER_BLUE, gdiplus::Graphics, typography};

#[derive(Clone, PartialEq)]
pub struct TextMark {
    pub text: Vec<u16>,
    pub size: i32,
    pub color: u32,
    center: (f64, f64),
    width: i32,
    height: i32,
    angle: f64,
}

#[derive(Clone, Copy)]
pub enum TextHandle {
    Move,
    Scale(i32, i32),
    Rotate,
}

const CORNERS: [(i32, i32); 4] = [(-1, -1), (1, -1), (1, 1), (-1, 1)];
const RESIZE_CURSORS: [*const u16; 4] = [IDC_SIZEWE, IDC_SIZENWSE, IDC_SIZENS, IDC_SIZENESW];

/// The system cursor for a frame handle on a box rotated by `angle` radians.
/// Corner handles point along their rotated diagonal, snapped to the nearest
/// of the four resize cursors.
pub fn handle_cursor(handle: TextHandle, angle: f64) -> *const u16 {
    match handle {
        TextHandle::Move => IDC_SIZEALL,
        TextHandle::Rotate => IDC_HAND,
        TextHandle::Scale(x, y) => {
            let (sin, cos) = angle.sin_cos();
            let (x, y) = (f64::from(x), f64::from(y));
            let (dx, dy) = (x * cos - y * sin, x * sin + y * cos);
            // Screen y points down, so 45° is the "\\" diagonal (NW-SE).
            let degrees = dy.atan2(dx).to_degrees().rem_euclid(180.0);
            RESIZE_CURSORS[((degrees + 22.5) / 45.0) as usize % 4]
        }
    }
}
const ROTATE_OFFSET: i32 = 14;
const ROTATE_RADIUS: i32 = 8;
const HANDLE_HALF: f32 = 3.5;
const MIN_SIZE: i32 = 8;
const MAX_SIZE: i32 = 128;

impl TextMark {
    pub fn new(rect: RECT, size: i32, color: u32) -> Self {
        Self {
            text: Vec::new(),
            size,
            color,
            center: (
                f64::from(rect.left + rect.right) / 2.0,
                f64::from(rect.top + rect.bottom) / 2.0,
            ),
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
            angle: 0.0,
        }
    }

    fn local_rect(&self) -> RECT {
        let left = -self.width / 2;
        let top = -self.height / 2;
        RECT {
            left,
            top,
            right: left + self.width,
            bottom: top + self.height,
        }
    }

    pub fn edit_rect(&self, clip: RECT) -> RECT {
        if self.is_rotated() {
            let rect = self.local_rect();
            let x = self.center.0.round() as i32;
            let y = self.center.1.round() as i32;
            return RECT {
                left: rect.left + x,
                top: rect.top + y,
                right: rect.left + x + self.width.min(clip.right - clip.left),
                bottom: rect.top + y + self.height.min(clip.bottom - clip.top),
            };
        }
        let width = self.width.max(32).min(clip.right - clip.left);
        let height = self.height.max(self.size + 10).min(clip.bottom - clip.top);
        let left = (self.center.0.round() as i32 - width / 2).clamp(clip.left, clip.right - width);
        let top = (self.center.1.round() as i32 - height / 2).clamp(clip.top, clip.bottom - height);
        RECT {
            left,
            top,
            right: left + width,
            bottom: top + height,
        }
    }

    pub fn angle(&self) -> f64 {
        self.angle
    }

    pub fn is_rotated(&self) -> bool {
        self.angle.cos() < 1.0 - f64::EPSILON
    }

    fn transform(&self, x: f64, y: f64) -> (f64, f64) {
        let (sin, cos) = self.angle.sin_cos();
        (
            self.center.0 + x * cos - y * sin,
            self.center.1 + x * sin + y * cos,
        )
    }

    fn local_point(&self, point: POINT) -> (f64, f64) {
        let (sin, cos) = self.angle.sin_cos();
        let x = f64::from(point.x) - self.center.0;
        let y = f64::from(point.y) - self.center.1;
        (x * cos + y * sin, -x * sin + y * cos)
    }

    fn input_origin(&self) -> (i32, i32) {
        let rect = self.local_rect();
        (
            rect.left + ((self.width - 1) / 2).min(2),
            rect.top + ((self.height - 1) / 2).min(2),
        )
    }

    pub fn input_point(&self, point: POINT) -> POINT {
        let (x, y) = self.local_point(point);
        let (left, top) = self.input_origin();
        POINT {
            x: x.round() as i32 - left,
            y: y.round() as i32 - top,
        }
    }

    pub fn input_screen_point(&self, point: POINT) -> POINT {
        let (left, top) = self.input_origin();
        self.screen_point(left + point.x, top + point.y)
    }

    fn precise_point(&self, x: i32, y: i32) -> PointF {
        let (x, y) = self.transform(f64::from(x), f64::from(y));
        PointF {
            X: x as f32,
            Y: y as f32,
        }
    }

    pub unsafe fn paint_input(&self, dc: HDC, image: &Image, caret: Option<RECT>) {
        let Some(graphics) = Graphics::new(dc) else {
            return;
        };
        let (left, top) = self.input_origin();
        graphics.image(
            image,
            [
                self.precise_point(left, top),
                self.precise_point(left + image.width, top),
                self.precise_point(left, top + image.height),
            ],
            self.is_rotated(),
        );
        if let Some(caret) = caret {
            let start = caret.top.clamp(0, image.height);
            let end = caret.bottom.clamp(0, image.height);
            if start < end && (0..image.width).contains(&caret.left) {
                let pen = graphics.pen(self.color, (caret.right - caret.left).max(1) as f32);
                if let Some(pen) = pen {
                    let x = left + caret.left;
                    graphics.lines(
                        &pen,
                        &[
                            self.precise_point(x, top + start),
                            self.precise_point(x, top + end),
                        ],
                    );
                }
            }
        }
    }

    pub fn contains(&self, point: POINT) -> bool {
        let (x, y) = self.local_point(point);
        let rect = self.local_rect();
        x >= f64::from(rect.left)
            && x <= f64::from(rect.right)
            && y >= f64::from(rect.top)
            && y <= f64::from(rect.bottom)
    }

    fn corner(&self, x: i32, y: i32) -> POINT {
        let rect = self.local_rect();
        self.screen_point(
            if x < 0 { rect.left } else { rect.right },
            if y < 0 { rect.top } else { rect.bottom },
        )
    }

    fn screen_point(&self, x: i32, y: i32) -> POINT {
        let (x, y) = self.transform(f64::from(x), f64::from(y));
        POINT {
            x: x.round() as i32,
            y: y.round() as i32,
        }
    }

    fn rotation_point(&self) -> POINT {
        self.screen_point(0, self.local_rect().top - ROTATE_OFFSET)
    }

    pub fn bounds(&self) -> RECT {
        let rotation = self.rotation_point();
        let mut bounds = RECT {
            left: rotation.x,
            top: rotation.y,
            right: rotation.x,
            bottom: rotation.y,
        };
        for (x, y) in CORNERS {
            let point = self.corner(x, y);
            bounds.left = bounds.left.min(point.x);
            bounds.top = bounds.top.min(point.y);
            bounds.right = bounds.right.max(point.x);
            bounds.bottom = bounds.bottom.max(point.y);
        }
        RECT {
            left: bounds.left - ROTATE_RADIUS - 2,
            top: bounds.top - ROTATE_RADIUS - 2,
            right: bounds.right + ROTATE_RADIUS + 3,
            bottom: bounds.bottom + ROTATE_RADIUS + 3,
        }
    }

    pub fn hit(&self, point: POINT) -> Option<TextHandle> {
        self.hit_chrome(point)
            .or_else(|| self.contains(point).then_some(TextHandle::Move))
    }

    pub fn hit_chrome(&self, point: POINT) -> Option<TextHandle> {
        let near =
            |handle: POINT| (handle.x - point.x).abs() <= 6 && (handle.y - point.y).abs() <= 6;
        let rotate = self.rotation_point();
        let dx = rotate.x - point.x;
        let dy = rotate.y - point.y;
        if i64::from(dx) * i64::from(dx) + i64::from(dy) * i64::from(dy)
            <= i64::from((ROTATE_RADIUS + 1).pow(2))
        {
            return Some(TextHandle::Rotate);
        }
        for (x, y) in CORNERS {
            if near(self.corner(x, y)) {
                return Some(TextHandle::Scale(x, y));
            }
        }
        let (x, y) = self.local_point(point);
        let rect = self.local_rect();
        let distance = [
            x - f64::from(rect.left),
            f64::from(rect.right) - x,
            y - f64::from(rect.top),
            f64::from(rect.bottom) - y,
        ]
        .into_iter()
        .fold(f64::INFINITY, f64::min);
        (-5.0..=3.0).contains(&distance).then_some(TextHandle::Move)
    }

    pub fn transform_drag(&self, handle: TextHandle, anchor: POINT, point: POINT) -> Self {
        self.transform_drag_from(self, handle, anchor, point)
    }

    pub fn transform_drag_from(
        &self,
        frame: &Self,
        handle: TextHandle,
        anchor: POINT,
        point: POINT,
    ) -> Self {
        let mut next = self.clone();
        if anchor.x == point.x && anchor.y == point.y {
            return next;
        }
        match handle {
            TextHandle::Move => {
                next.center.0 += f64::from(point.x - anchor.x);
                next.center.1 += f64::from(point.y - anchor.y);
            }
            TextHandle::Rotate => {
                let angle = |point: POINT| {
                    (f64::from(point.y) - frame.center.1).atan2(f64::from(point.x) - frame.center.0)
                };
                next.angle += angle(point) - angle(anchor);
            }
            TextHandle::Scale(x, y) => {
                let fixed = self.corner(-x, -y);
                let origin = frame.corner(-x, -y);
                let dragged = frame.corner(x, y);
                let dx = f64::from(dragged.x - origin.x);
                let dy = f64::from(dragged.y - origin.y);
                let delta_x = f64::from(point.x - anchor.x);
                let delta_y = f64::from(point.y - anchor.y);
                let factor = 1.0 + (delta_x * dx + delta_y * dy) / (dx * dx + dy * dy).max(1.0);
                next.size = (f64::from(self.size) * factor)
                    .round()
                    .clamp(f64::from(MIN_SIZE), f64::from(MAX_SIZE))
                    as i32;
                let scale = f64::from(next.size) / f64::from(self.size);
                next.width = (f64::from(self.width) * scale).round().max(1.0) as i32;
                next.height = (f64::from(self.height) * scale).round().max(1.0) as i32;
                next.center = (
                    f64::from(fixed.x) + (self.center.0 - f64::from(fixed.x)) * scale,
                    f64::from(fixed.y) + (self.center.1 - f64::from(fixed.y)) * scale,
                );
            }
        }
        next
    }

    pub unsafe fn fit_text(&mut self, dc: HDC) {
        if self.text.is_empty() {
            return;
        }
        self.width = self.width.max(self.size + 6);
        let previous = SelectObject(dc, typography::annotation_font(self.size));
        let mut bounds = RECT {
            right: self.width - 6,
            ..Default::default()
        };
        DrawTextW(
            dc,
            self.text.as_ptr(),
            self.text.len() as i32,
            &mut bounds,
            DT_WORDBREAK | DT_NOPREFIX | DT_CALCRECT | DT_EDITCONTROL,
        );
        SelectObject(dc, previous);
        self.height = self.height.max(bounds.bottom + 6);
    }

    pub unsafe fn paint(&self, dc: HDC) {
        if self.text.is_empty() {
            return;
        }
        let saved = SaveDC(dc);
        if saved == 0 {
            return;
        }
        let (sin, cos) = self.angle.sin_cos();
        let transform = XFORM {
            eM11: cos as f32,
            eM12: sin as f32,
            eM21: -sin as f32,
            eM22: cos as f32,
            eDx: self.center.0 as f32,
            eDy: self.center.1 as f32,
        };
        if SetGraphicsMode(dc, GM_ADVANCED) != 0 && SetWorldTransform(dc, &transform) != 0 {
            SelectObject(dc, typography::annotation_font(self.size));
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, self.color);
            let mut rect = self.local_rect();
            InflateRect(&mut rect, -3, -3);
            DrawTextW(
                dc,
                self.text.as_ptr(),
                self.text.len() as i32,
                &mut rect,
                DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL,
            );
        }
        RestoreDC(dc, saved);
    }

    /// Snipaste-style chrome: a thin dashed outline, small square corner
    /// handles, and a slim rotation knob above the top edge.
    pub unsafe fn paint_frame(&self, dc: HDC) {
        if self.paint_smooth_frame(dc) {
            return;
        }
        // Without GDI+, fall back to a plain dotted outline.
        let saved = SaveDC(dc);
        if saved == 0 {
            return;
        }
        let pen = CreatePen(PS_DOT, 1, BORDER_BLUE);
        if !pen.is_null() {
            SelectObject(dc, pen);
        }
        SetBkMode(dc, TRANSPARENT as i32);
        let corners = CORNERS.map(|(x, y)| self.corner(x, y));
        let outline = [corners[0], corners[1], corners[2], corners[3], corners[0]];
        Polyline(dc, outline.as_ptr(), outline.len() as i32);
        RestoreDC(dc, saved);
        if !pen.is_null() {
            DeleteObject(pen);
        }
    }

    unsafe fn paint_smooth_frame(&self, dc: HDC) -> bool {
        let Some(graphics) = Graphics::new(dc) else {
            return false;
        };
        let (Some(dashed), Some(solid)) = (
            graphics.dashed_pen(BORDER_BLUE, 1.0),
            graphics.pen(BORDER_BLUE, 1.0),
        ) else {
            return false;
        };
        let rect = self.local_rect();
        let corners = [
            self.precise_point(rect.left, rect.top),
            self.precise_point(rect.right, rect.top),
            self.precise_point(rect.right, rect.bottom),
            self.precise_point(rect.left, rect.bottom),
        ];
        graphics.lines(
            &dashed,
            &[corners[0], corners[1], corners[2], corners[3], corners[0]],
        );
        let rotation = self.precise_point(0, rect.top - ROTATE_OFFSET);
        graphics.lines(&solid, &[self.precise_point(0, rect.top), rotation]);
        for corner in corners {
            graphics.square_handle(&solid, corner, HANDLE_HALF);
        }
        graphics.handle(&solid, rotation, ROTATE_RADIUS as f32);
        graphics.rotation_arrow(&solid, rotation);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{Desktop, Surface};
    use std::ptr::null_mut;

    /// Renders a plain and a rotated text frame to `%TEMP%\\snapshot-preview`.
    #[test]
    #[ignore]
    fn preview() {
        let folder = std::env::temp_dir().join("snapshot-preview");
        std::fs::create_dir_all(&folder).unwrap();
        unsafe {
            let bounds = RECT {
                right: 420,
                bottom: 170,
                ..Default::default()
            };
            let surface = Surface::new(null_mut(), bounds.right, bounds.bottom).unwrap();
            FillRect(surface.dc, &bounds, GetStockObject(WHITE_BRUSH));
            for (frame, angle) in [
                (RECT { left: 30, top: 50, right: 190, bottom: 100 }, 0.0),
                (RECT { left: 240, top: 60, right: 380, bottom: 110 }, -0.3),
            ] {
                let mut mark = TextMark::new(frame, 24, 0x0045_45ff);
                mark.text = "Snapshot 文本".encode_utf16().collect();
                mark.angle = angle;
                mark.paint(surface.dc);
                mark.paint_frame(surface.dc);
            }
            GdiFlush();
            let mut image = Desktop { bounds, surface }.crop(bounds).unwrap();
            for pixel in image.pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            image.save_png(&folder.join("text-frame.png")).unwrap();
        }
    }

    #[test]
    fn corner_cursors_follow_the_rotation() {
        use std::f64::consts::FRAC_PI_4;
        let top_left = TextHandle::Scale(-1, -1);
        let top_right = TextHandle::Scale(1, -1);
        assert_eq!(handle_cursor(top_left, 0.0), IDC_SIZENWSE);
        assert_eq!(handle_cursor(top_right, 0.0), IDC_SIZENESW);
        // A quarter turn swaps the diagonals.
        assert_eq!(handle_cursor(top_left, 2.0 * FRAC_PI_4), IDC_SIZENESW);
        assert_eq!(handle_cursor(top_right, 2.0 * FRAC_PI_4), IDC_SIZENWSE);
        // An eighth turn lines the corners up with the axes.
        assert_eq!(handle_cursor(top_left, FRAC_PI_4), IDC_SIZENS);
        assert_eq!(handle_cursor(top_right, FRAC_PI_4), IDC_SIZEWE);
        assert_eq!(handle_cursor(TextHandle::Move, 1.0), IDC_SIZEALL);
    }

    #[test]
    fn rotated_frame_edges_blend_into_the_background() {
        unsafe {
            let surface = Surface::new(null_mut(), 220, 180).unwrap();
            let bounds = RECT {
                right: 220,
                bottom: 180,
                ..Default::default()
            };
            FillRect(surface.dc, &bounds, GetStockObject(WHITE_BRUSH));
            let frame = RECT {
                left: 55,
                top: 60,
                right: 175,
                bottom: 120,
            };
            let mut text = TextMark::new(frame, 20, 0);
            text.angle = 0.47;
            text.paint_frame(surface.dc);
            let edge = text.local_rect().top;
            let mut blended = 0;
            for x in -45..-15 {
                let point = text.screen_point(x, edge);
                for dy in -2..=2 {
                    let pixel = GetPixel(surface.dc, point.x, point.y + dy);
                    if pixel != BORDER_BLUE && pixel != 0x00ff_ffff && pixel != CLR_INVALID {
                        blended += 1;
                    }
                }
            }
            assert!(
                blended > 20,
                "diagonal edge must contain partial-coverage pixels"
            );
        }
    }

    #[test]
    fn rotated_text_renders_on_a_translated_crop_without_a_fill_or_handles() {
        unsafe {
            let desktop_bounds = RECT {
                right: 220,
                bottom: 180,
                ..Default::default()
            };
            let surface = Surface::new(null_mut(), 220, 180).unwrap();
            SetDCBrushColor(surface.dc, 0x0030_2010);
            FillRect(surface.dc, &desktop_bounds, GetStockObject(DC_BRUSH));
            GdiFlush();
            let desktop = Desktop {
                bounds: desktop_bounds,
                surface,
            };
            let frame = RECT {
                left: 80,
                top: 55,
                right: 170,
                bottom: 105,
            };
            let mut mark = TextMark::new(frame, 26, 0x00ff_ffff);
            mark.text = "TEST".encode_utf16().collect();
            let anchor = mark.rotation_point();
            let rotated = mark.transform_drag(TextHandle::Rotate, anchor, POINT { x: 169, y: 80 });
            assert!(rotated.angle > 1.5);
            let clip = RECT {
                left: 70,
                top: 20,
                right: 190,
                bottom: 140,
            };
            let image = desktop
                .composite_crop(clip, |dc| rotated.paint(dc))
                .unwrap();
            let background = [0x30, 0x20, 0x10];
            let changed_rows = image
                .pixels
                .chunks_exact(image.width as usize * 4)
                .filter(|row| row.chunks_exact(4).any(|pixel| pixel[..3] != background))
                .count();
            assert!(
                (35..100).contains(&changed_rows),
                "changed rows: {changed_rows}"
            );
            assert_eq!(image.pixels[..3], background);
        }
    }
}
