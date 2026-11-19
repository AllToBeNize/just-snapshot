use std::{
    cell::{Cell, LazyCell},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{HWND, RECT},
    Graphics::{Gdi::*, GdiPlus::*},
    UI::WindowsAndMessaging::*,
};

use crate::capture::{Desktop, Image, Surface};

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
