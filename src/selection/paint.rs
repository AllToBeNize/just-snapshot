//! Invalidation helpers and painting the overlay.

use super::*;

pub(super) unsafe fn invalidate_change(window: HWND, old: RECT, new: RECT, canvas: RECT) {
    if EqualRect(&old, &new) == 0 {
        // The shared interior keeps the same pixels as the selection moves.
        let region = CreateRectRgnIndirect(&old);
        let next = CreateRectRgnIndirect(&new);
        if !region.is_null() && !next.is_null() && CombineRgn(region, region, next, RGN_XOR) != 0 {
            InvalidateRgn(window, region, 0);
        } else {
            InvalidateRect(window, &old, 0);
            InvalidateRect(window, &new, 0);
        }
        if !next.is_null() {
            DeleteObject(next);
        }
        if !region.is_null() {
            DeleteObject(region);
        }
        for rect in [old, new] {
            for edge in edges(rect) {
                InvalidateRect(window, &edge, 0);
            }
            if !outline_empty(rect) {
                for handle in HANDLES {
                    InvalidateRect(window, &handle.bounds(rect, canvas), 0);
                }
            }
        }
        if let Some(desktop) = SESSION.with_borrow(|session| Some(session.as_ref()?.desktop.bounds))
        {
            for badge in [old, new]
                .into_iter()
                .filter_map(|rect| size_badge(rect, desktop, canvas))
            {
                InvalidateRect(window, &badge, 0);
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
    let width = 146;
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

        let dc = selection.back_buffer.dc;
        let source = selection.desktop.surface.dc;
        if clipped {
            SelectClipRgn(dc, update_region);
        } else {
            IntersectClipRect(dc, area.left, area.top, area.right, area.bottom);
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
            let size = crate::wide(&format!(
                "{} × {} px",
                rect.right - rect.left,
                rect.bottom - rect.top
            ));
            paint_size_badge(dc, badge, size.as_ptr());
        }
        if let Some(toolbar) = selection.toolbar {
            if intersect(toolbar.bounds, area).is_some() {
                toolbar.paint(dc, &selection.annotations, selection.toolbar_hover);
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
                paint_tooltip(dc, bounds, label);
            }
        }
        if let Some(options) = selection
            .options
            .filter(|options| intersect(options.bounds, area).is_some())
        {
            options.paint(dc, selection.option_style(), selection.options_hover);
        }
        SelectClipRgn(dc, null_mut());
        BitBlt(
            window_dc, area.left, area.top, width, height, dc, area.left, area.top, SRCCOPY,
        );
    });
    EndPaint(window, &paint);
    if !update_region.is_null() {
        DeleteObject(update_region);
    }
}
