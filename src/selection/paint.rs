//! Invalidation helpers and painting the overlay.

use super::*;

/// Marker colors (COLORREF) the recording picker paints in place of the
/// frozen screen: inside the area, and the dim mask outside it. `present_live`
/// turns them into alpha; nothing else draws in these exact colors.
pub(super) const LIVE_INSIDE: u32 = 0x0003_0201;
pub(super) const LIVE_DIM: u32 = 0x0006_0504;
/// How dark the live mask is, like the capture's dimming.
const LIVE_DIM_ALPHA: u8 = 102;

pub(super) unsafe fn invalidate_change(window: HWND, old: RECT, new: RECT, canvas: RECT) {
    if EqualRect(&old, &new) == 0 {
        // The shared interior keeps the same pixels as the selection moves.
        let region = CreateRectRgnIndirect(&old);
        let next = CreateRectRgnIndirect(&new);
        if !region.is_null() && !next.is_null() && CombineRgn(region, region, next, RGN_XOR) != 0 {
            invalidate_region(window, region, 0);
        } else {
            invalidate(window, &old, 0);
            invalidate(window, &new, 0);
        }
        if !next.is_null() {
            DeleteObject(next);
        }
        if !region.is_null() {
            DeleteObject(region);
        }
        for rect in [old, new] {
            for edge in edges(rect) {
                invalidate(window, &edge, 0);
            }
            if !outline_empty(rect) {
                for handle in HANDLES {
                    invalidate(window, &handle.bounds(rect, canvas), 0);
                }
            }
        }
        if let Some(desktop) = SESSION.with_borrow(|session| Some(session.as_ref()?.desktop.bounds))
        {
            for badge in [old, new]
                .into_iter()
                .filter_map(|rect| size_badge(rect, desktop, canvas))
            {
                invalidate(window, &badge, 0);
            }
        }
    }
}

pub(super) unsafe fn size_badge(rect: RECT, desktop: RECT, canvas: RECT) -> Option<RECT> {
    if outline_empty(rect) {
        return None;
    }
    let monitor = monitor_info(POINT {
        x: rect.left + desktop.left,
        y: rect.top + desktop.top,
    })
    .and_then(|info| overlay_rect(info.rcMonitor, desktop))
    .unwrap_or(canvas);
    let width = 176;
    let height = 24;
    if monitor.right - monitor.left < width || monitor.bottom - monitor.top < height {
        return None;
    }
    let left = (rect.left + 5).clamp(monitor.left, monitor.right - width);
    let above = rect.top - height - 3;
    let top = if above >= monitor.top {
        above
    } else {
        (rect.top + 6).clamp(monitor.top, monitor.bottom - height)
    };
    Some(RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    })
}

pub(super) fn outline_empty(rect: RECT) -> bool {
    rect.left >= rect.right || rect.top >= rect.bottom
}

pub(super) fn edges(rect: RECT) -> [RECT; 4] {
    if outline_empty(rect) {
        return [RECT::default(); 4];
    }
    [
        RECT {
            right: (rect.left + 2).min(rect.right),
            ..rect
        },
        RECT {
            bottom: (rect.top + 2).min(rect.bottom),
            ..rect
        },
        RECT {
            left: (rect.right - 2).max(rect.left),
            ..rect
        },
        RECT {
            top: (rect.bottom - 2).max(rect.top),
            ..rect
        },
    ]
}

pub(super) unsafe fn paint(window: HWND) {
    refresh_text_preview();
    let update_region = CreateRectRgn(0, 0, 0, 0);
    let clipped = !update_region.is_null() && GetUpdateRgn(window, update_region, 0) != 0;
    let mut paint = PAINTSTRUCT::default();
    let window_dc = BeginPaint(window, &mut paint);
    SESSION.with_borrow(|session| {
        let Some(selection) = session else { return };
        let area = paint.rcPaint;
        let width = area.right - area.left;
        let height = area.bottom - area.top;
        if width <= 0 || height <= 0 {
            return;
        }
        compose(selection, area, clipped.then_some(update_region));
        let dc = selection.back_buffer.dc;
        match &selection.live {
            Some(live) => present_live(window, selection, live, area),
            None => {
                BitBlt(window_dc, area.left, area.top, width, height, dc, area.left, area.top, SRCCOPY);
            }
        }
    });
    EndPaint(window, &paint);
    if !update_region.is_null() {
        DeleteObject(update_region);
    }
    // Panels switched on in the recording toolbar follow the area.
    sync_record_panels();
}

/// The recording picker's repaint of what changed (it never gets WM_PAINT; see `invalidate`).
pub(super) unsafe fn paint_live(window: HWND, dirty: RECT) {
    refresh_text_preview();
    SESSION.with_borrow(|session| {
        let Some(selection) = session else { return };
        let Some(live) = &selection.live else { return };
        let Some(area) = intersect(dirty, selection.canvas) else { return };
        compose(selection, area, None);
        present_live(window, selection, live, area);
    });
    sync_record_panels();
}

/// Draws the overlay's `area` (only inside `region` when given) into the back buffer.
unsafe fn compose(selection: &Selection, area: RECT, region: Option<HRGN>) {
    let width = area.right - area.left;
    let height = area.bottom - area.top;
    let dc = selection.back_buffer.dc;
    let source = selection.desktop.surface.dc;
    match region {
        Some(region) => {
            SelectClipRgn(dc, region);
        }
        None => {
            IntersectClipRect(dc, area.left, area.top, area.right, area.bottom);
        }
    }
    BitBlt(
        dc,
        area.left,
        area.top,
        width,
        height,
        selection.dimmed.dc,
        area.left,
        area.top,
        SRCCOPY,
    );
    let rect = selection.rect;
    if let Some(visible) = intersect(rect, area) {
        BitBlt(
            dc,
            visible.left,
            visible.top,
            visible.right - visible.left,
            visible.bottom - visible.top,
            source,
            visible.left,
            visible.top,
            SRCCOPY,
        );
        selection.annotations.paint(
            dc,
            rect,
            true,
            selection.editor.as_ref().and_then(|editor| editor.index),
        );
        if let Some(editor) = selection.editor.as_ref() {
            let saved = SaveDC(dc);
            if saved != 0 {
                IntersectClipRect(dc, rect.left, rect.top, rect.right, rect.bottom);
                if editor.preview.is_some() {
                    // A filled box shows behind the letters while typing too.
                    let frame = if selection.text_drag.is_some() { Cow::Borrowed(&editor.text) } else { editor.frame() };
                    let mut filled = frame.into_owned();
                    filled.look = editor.text.look;
                    filled.paint_background(dc);
                    // The outline follows what is being typed.
                    filled.text = read_editor_text(editor.window);
                    // The edit box always lays out from the top.
                    filled.valign = crate::text_mark::VAlign::Top;
                    filled.paint_halo(dc);
                }
                if let Some(drag) = &selection.text_drag {
                    if let Some(preview) = &editor.preview {
                        editor.text.paint_input(dc, preview, drag.caret);
                    } else {
                        editor.text.paint(dc);
                    }
                } else if let Some(preview) = &editor.preview {
                    let caret = if editor.caret_visible {
                        input_caret(editor.window)
                    } else {
                        None
                    };
                    editor.frame().paint_input(dc, preview, caret);
                }
                RestoreDC(dc, saved);
            }
        }
    }
    if !outline_empty(rect) {
        let blue = GetStockObject(DC_BRUSH);
        SetDCBrushColor(dc, BORDER_BLUE);
        for edge in edges(rect) {
            FillRect(dc, &edge, blue);
        }
        let old_pen = SelectObject(dc, GetStockObject(DC_PEN));
        let old_brush = SelectObject(dc, GetStockObject(WHITE_BRUSH));
        SetDCPenColor(dc, BORDER_BLUE);
        for handle in HANDLES {
            let bounds = handle.bounds(rect, selection.canvas);
            Ellipse(dc, bounds.left, bounds.top, bounds.right, bounds.bottom);
        }
        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);
    }
    if let Some(mark) = selection
        .selected_mark_frame()
        .filter(|mark| intersect(mark.bounds(), area).is_some())
    {
        let saved = SaveDC(dc);
        IntersectClipRect(dc, rect.left - 8, rect.top - 8, rect.right + 8, rect.bottom + 8);
        mark.paint_frame(dc);
        RestoreDC(dc, saved);
    }
    paint_picks(selection, dc);
    if let Some(text) = selection.text_frame() {
        if intersect(text.bounds(), area).is_some() {
            text.paint_frame(dc);
        }
    }
    selection.hint.paint(dc, area);
    if let Some(loupe) = selection.loupe {
        if intersect(loupe.bounds, area).is_some() {
            loupe.paint(dc, source, selection.canvas);
        }
    }
    if let Some(badge) = size_badge(rect, selection.desktop.bounds, selection.canvas)
        .filter(|badge| intersect(*badge, area).is_some())
    {
        let layer = selection.layer.as_ref().map(|pick| format!(" · 层 {}", pick.label())).unwrap_or_default();
        let size = crate::wide(&format!(
            "{} × {} px{layer}",
            rect.right - rect.left,
            rect.bottom - rect.top
        ));
        paint_size_badge(dc, badge, size.as_ptr());
    }
    if let Some(toolbar) = selection.toolbar {
        if intersect(toolbar.bounds, area).is_some() {
            let switches = selection
                .record
                .map_or([false; 4], |options| [options.system_audio, options.microphone, options.show_keys, options.show_pad]);
            toolbar.paint(dc, &selection.annotations, selection.toolbar_hover, switches);
        }
        if let Some((bounds, label)) = selection
            .toolbar_hover
            .and_then(|action| toolbar.tooltip(action))
            .filter(|(bounds, _)| intersect(*bounds, area).is_some())
            .filter(|(bounds, _)| {
                selection
                    .options
                    .is_none_or(|options| intersect(*bounds, options.bounds).is_none())
            })
        {
            paint_tooltip(dc, bounds, label.as_ptr());
        }
    }
    if let Some(options) = selection
        .options
        .filter(|options| intersect(options.bounds, area).is_some())
    {
        options.paint(dc, selection.option_style(), selection.options_hover);
    }
    if let Some(bar) = selection.panel_bar.filter(|bar| intersect(bar.bounds, area).is_some()) {
        bar.paint(dc, selection.panel_bar_hover);
    }
    SelectClipRgn(dc, null_mut());
}

/// The recording picker's frame: marker colors become a see-through inside
/// (alpha 1, so it still takes the mouse) and a dim mask outside; everything
/// drawn on top stays opaque. Only `area` changed, so only it is converted
/// and handed to the layered window.
pub(super) unsafe fn present_live(window: HWND, selection: &Selection, live: &Surface, area: RECT) {
    let (width, height) = (selection.canvas.right, selection.canvas.bottom);
    GdiFlush();
    let source = selection.back_buffer.bytes(width, height);
    let target = live.bytes_mut(width, height);
    let [inside, dim] = [LIVE_INSIDE, LIVE_DIM].map(|color| [(color >> 16) as u8, (color >> 8) as u8, color as u8]);
    for y in area.top.max(0)..area.bottom.min(height) {
        let row = (y * width) as usize * 4;
        for x in area.left.max(0)..area.right.min(width) {
            let at = row + x as usize * 4;
            let pixel = [source[at], source[at + 1], source[at + 2]];
            let out = if pixel == inside {
                [0, 0, 0, 1]
            } else if pixel == dim {
                [0, 0, 0, LIVE_DIM_ALPHA]
            } else {
                [pixel[0], pixel[1], pixel[2], 255]
            };
            target[at..at + 4].copy_from_slice(&out);
        }
    }
    let bounds = selection.desktop.bounds;
    let position = POINT { x: bounds.left, y: bounds.top };
    let size = SIZE { cx: width, cy: height };
    let origin = POINT::default();
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    let info = UPDATELAYEREDWINDOWINFO {
        cbSize: size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
        hdcDst: null_mut(),
        pptDst: &position,
        psize: &size,
        hdcSrc: live.dc,
        pptSrc: &origin,
        crKey: 0,
        pblend: &blend,
        dwFlags: ULW_ALPHA,
        prcDirty: &area,
    };
    UpdateLayeredWindowIndirect(window, &info);
}
