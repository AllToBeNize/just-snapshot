//! Captures the hidden native text editor so the overlay can draw its text.

use std::{cell::Cell, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::*,
};

use crate::capture::{Desktop, Image, Surface};

thread_local! {
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
