use std::{
    cell::{Cell, LazyCell},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{HWND, RECT},
    Graphics::{Gdi::*, GdiPlus::*},
    UI::WindowsAndMessaging::*,
};

use crate::{
    capture::{Desktop, Image, Surface},
    icons::Seg,
};

const PARGB_32: i32 = 0x000e_200b;

struct GdiPlus(usize);

impl Drop for GdiPlus {
    fn drop(&mut self) {
        unsafe { GdiplusShutdown(self.0) };
    }
}

thread_local! {
    static STARTUP: LazyCell<Option<GdiPlus>> = LazyCell::new(|| unsafe {
        let mut token = 0;
        let input = GdiplusStartupInput { GdiplusVersion: 1, ..Default::default() };
        (GdiplusStartup(&mut token, &input, null_mut()) == Ok).then(|| GdiPlus(token))
    });
    pub static PRINT_BACKGROUND: Cell<Option<u32>> = const { Cell::new(None) };
}

pub unsafe fn snapshot_input(window: HWND) -> Option<Image> {
    let mut bounds = RECT::default();
    if GetClientRect(window, &mut bounds) == 0 || bounds.right <= 0 || bounds.bottom <= 0 {
        return None;
    }
    let render = |color, brush| {
        let surface = Surface::new(null_mut(), bounds.right, bounds.bottom).ok()?;
        let saved = SaveDC(surface.dc);
        if saved == 0 {
            return None;
        }
        FillRect(surface.dc, &bounds, GetStockObject(brush));
        PRINT_BACKGROUND.set(Some(color));
        SendMessageW(
            window,
            WM_PRINTCLIENT,
            surface.dc as usize,
            (PRF_CLIENT | PRF_ERASEBKGND) as isize,
        );
        PRINT_BACKGROUND.set(None);
        RestoreDC(surface.dc, saved);
        GdiFlush();
        Desktop { bounds, surface }.crop(bounds).ok()
    };
    let mut dark = render(0, BLACK_BRUSH)?;
    let light = render(0x00ff_ffff, WHITE_BRUSH)?;
    for (dark, light) in dark
        .pixels
        .chunks_exact_mut(4)
        .zip(light.pixels.chunks_exact(4))
    {
        // The difference between the two backgrounds gives the uncovered fraction.
        let uncovered = (0..3)
            .map(|channel| light[channel].saturating_sub(dark[channel]))
            .max()
            .unwrap();
        let alpha = 255 - uncovered;
        for channel in &mut dark[..3] {
            *channel = (*channel).min(alpha);
        }
        dark[3] = alpha;
    }
    Some(dark)
}

pub struct Graphics(*mut GpGraphics);

impl Graphics {
    pub unsafe fn new(dc: HDC) -> Option<Self> {
        if !STARTUP.with(|startup| startup.is_some()) {
            return None;
        }
        let mut graphics = null_mut();
        if GdipCreateFromHDC(dc, &mut graphics) != Ok {
            return None;
        }
        GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias8x8);
        Some(Self(graphics))
    }

    pub unsafe fn pen(&self, color: u32, width: f32) -> Option<Pen> {
        let mut pen = null_mut();
        let argb = 0xff00_0000 | ((color & 0xff) << 16) | (color & 0xff00) | ((color >> 16) & 0xff);
        (GdipCreatePen1(argb, width, UnitPixel, &mut pen) == Ok).then(|| Pen(pen))
    }

    pub unsafe fn lines(&self, pen: &Pen, points: &[PointF]) {
        GdipDrawLines(self.0, pen.0, points.as_ptr(), points.len() as i32);
    }

    /// Snipaste-style dashed outline pen.
    pub unsafe fn dashed_pen(&self, color: u32, width: f32) -> Option<Pen> {
        let pen = self.pen(color, width)?;
        let pattern = [4.0f32, 3.0];
        GdipSetPenDashArray(pen.0, pattern.as_ptr(), pattern.len() as i32);
        Some(pen)
    }

    /// A small white square handle with a thin outline.
    pub unsafe fn square_handle(&self, pen: &Pen, center: PointF, half: f32) {
        let mut brush = null_mut();
        let (x, y, size) = (center.X - half, center.Y - half, half * 2.0);
        if GdipCreateSolidFill(0xffff_ffff, &mut brush) == Ok {
            GdipFillRectangle(self.0, brush.cast(), x, y, size, size);
            GdipDeleteBrush(brush.cast());
        }
        GdipDrawRectangle(self.0, pen.0, x, y, size, size);
    }

    pub unsafe fn handle(&self, pen: &Pen, center: PointF, radius: f32) {
        let mut brush = null_mut();
        let x = center.X - radius;
        let y = center.Y - radius;
        let diameter = radius * 2.0;
        if GdipCreateSolidFill(0xffff_ffff, &mut brush) == Ok {
            GdipFillEllipse(self.0, brush.cast(), x, y, diameter, diameter);
            GdipDeleteBrush(brush.cast());
        }
        GdipDrawEllipse(self.0, pen.0, x, y, diameter, diameter);
    }

    pub unsafe fn rotation_arrow(&self, pen: &Pen, PointF { X: x, Y: y }: PointF) {
        GdipDrawArc(self.0, pen.0, x - 5.0, y - 5.0, 10.0, 10.0, 40.0, 280.0);
        self.lines(
            pen,
            &[
                PointF {
                    X: x + 5.0,
                    Y: y - 6.0,
                },
                PointF {
                    X: x + 5.0,
                    Y: y - 2.0,
                },
                PointF {
                    X: x + 1.0,
                    Y: y - 2.0,
                },
            ],
        );
    }

    pub unsafe fn image(&self, image: &Image, corners: [PointF; 3], rotated: bool) {
        let mut bitmap = null_mut();
        if GdipCreateBitmapFromScan0(
            image.width,
            image.height,
            image.width * 4,
            PARGB_32,
            image.pixels.as_ptr(),
            &mut bitmap,
        ) != Ok
        {
            return;
        }
        GdipSetInterpolationMode(
            self.0,
            if rotated {
                InterpolationModeHighQualityBicubic
            } else {
                InterpolationModeNearestNeighbor
            },
        );
        GdipSetPixelOffsetMode(self.0, PixelOffsetModeHalf);
        GdipDrawImagePointsRect(
            self.0,
            bitmap.cast(),
            corners.as_ptr(),
            3,
            0.0,
            0.0,
            image.width as f32,
            image.height as f32,
            UnitPixel,
            null(),
            0,
            null_mut(),
        );
        GdipDisposeImage(bitmap.cast());
    }
}

impl Graphics {
    /// Fills an anti-aliased rounded rectangle. `argb` is 0xAARRGGBB.
    pub unsafe fn fill_round(&self, bounds: [f32; 4], radius: f32, argb: u32) {
        let Some(path) = round_path(bounds, radius) else {
            return;
        };
        let mut brush = null_mut();
        if GdipCreateSolidFill(argb, &mut brush) == Ok {
            GdipFillPath(self.0, brush.cast(), path);
            GdipDeleteBrush(brush.cast());
        }
        GdipDeletePath(path);
    }

    pub unsafe fn stroke_round(&self, bounds: [f32; 4], radius: f32, argb: u32, width: f32) {
        let Some(path) = round_path(bounds, radius) else {
            return;
        };
        let mut pen = null_mut();
        if GdipCreatePen1(argb, width, UnitPixel, &mut pen) == Ok {
            GdipDrawPath(self.0, pen, path);
            GdipDeletePen(pen);
        }
        GdipDeletePath(path);
    }

    pub unsafe fn fill_ellipse(&self, [left, top, right, bottom]: [f32; 4], argb: u32) {
        let mut brush = null_mut();
        if GdipCreateSolidFill(argb, &mut brush) == Ok {
            GdipFillEllipse(self.0, brush.cast(), left, top, right - left, bottom - top);
            GdipDeleteBrush(brush.cast());
        }
    }
}

/// Renders into a transparent premultiplied-BGRA image of `size`×`size`.
pub unsafe fn rasterize(size: i32, draw: impl FnOnce(&Graphics)) -> Option<Image> {
    if size <= 0 || !STARTUP.with(|startup| startup.is_some()) {
        return None;
    }
    let mut pixels = vec![0u8; (size * size * 4) as usize];
    let mut bitmap = null_mut();
    if GdipCreateBitmapFromScan0(size, size, size * 4, PARGB_32, pixels.as_mut_ptr(), &mut bitmap)
        != Ok
    {
        return None;
    }
    let mut context = null_mut();
    if GdipGetImageGraphicsContext(bitmap.cast(), &mut context) == Ok {
        GdipSetSmoothingMode(context, SmoothingModeAntiAlias8x8);
        GdipSetPixelOffsetMode(context, PixelOffsetModeHalf);
        draw(&Graphics(context));
    }
    GdipDisposeImage(bitmap.cast());
    Some(Image {
        width: size,
        height: size,
        pixels,
    })
}

impl Graphics {
    /// Strokes icon segments in a 24-unit grid scaled by `scale`, with round caps and joins.
    pub unsafe fn stroke_segments(&self, segments: &[Seg], scale: f32, argb: u32, width: f32) {
        let Some(path) = segment_path(segments, scale) else {
            return;
        };
        let mut pen = null_mut();
        if GdipCreatePen1(argb, width, UnitPixel, &mut pen) == Ok {
            GdipSetPenStartCap(pen, LineCapRound);
            GdipSetPenEndCap(pen, LineCapRound);
            GdipSetPenLineJoin(pen, LineJoinRound);
            GdipDrawPath(self.0, pen, path);
            GdipDeletePen(pen);
        }
        GdipDeletePath(path);
    }

    pub unsafe fn fill_segments(&self, segments: &[Seg], scale: f32, argb: u32) {
        let Some(path) = segment_path(segments, scale) else {
            return;
        };
        let mut brush = null_mut();
        if GdipCreateSolidFill(argb, &mut brush) == Ok {
            GdipFillPath(self.0, brush.cast(), path);
            GdipDeleteBrush(brush.cast());
        }
        GdipDeletePath(path);
    }
}

unsafe fn segment_path(segments: &[Seg], scale: f32) -> Option<*mut GpPath> {
    if segments.is_empty() {
        return None;
    }
    let mut path = null_mut();
    if GdipCreatePath(FillModeWinding, &mut path) != Ok {
        return None;
    }
    let mut current = (0.0, 0.0);
    for segment in segments {
        match *segment {
            Seg::M(x, y) => {
                GdipStartPathFigure(path);
                current = (x * scale, y * scale);
            }
            Seg::L(x, y) => {
                let next = (x * scale, y * scale);
                GdipAddPathLine(path, current.0, current.1, next.0, next.1);
                current = next;
            }
            Seg::C(x1, y1, x2, y2, x, y) => {
                let next = (x * scale, y * scale);
                GdipAddPathBezier(
                    path,
                    current.0,
                    current.1,
                    x1 * scale,
                    y1 * scale,
                    x2 * scale,
                    y2 * scale,
                    next.0,
                    next.1,
                );
                current = next;
            }
            Seg::Z => {
                GdipClosePathFigure(path);
            }
        }
    }
    Some(path)
}

unsafe fn round_path([left, top, right, bottom]: [f32; 4], radius: f32) -> Option<*mut GpPath> {
    let mut path = null_mut();
    if GdipCreatePath(FillModeAlternate, &mut path) != Ok {
        return None;
    }
    let diameter = (radius * 2.0).min(right - left).min(bottom - top).max(0.0);
    if diameter <= 0.0 {
        GdipAddPathRectangle(path, left, top, right - left, bottom - top);
    } else {
        GdipAddPathArc(path, left, top, diameter, diameter, 180.0, 90.0);
        GdipAddPathArc(path, right - diameter, top, diameter, diameter, 270.0, 90.0);
        GdipAddPathArc(path, right - diameter, bottom - diameter, diameter, diameter, 0.0, 90.0);
        GdipAddPathArc(path, left, bottom - diameter, diameter, diameter, 90.0, 90.0);
        GdipClosePathFigure(path);
    }
    Some(path)
}

impl Drop for Graphics {
    fn drop(&mut self) {
        unsafe { GdipDeleteGraphics(self.0) };
    }
}

pub struct Pen(*mut GpPen);

impl Drop for Pen {
    fn drop(&mut self) {
        unsafe { GdipDeletePen(self.0) };
    }
}
