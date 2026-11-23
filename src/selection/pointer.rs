//! Selection dragging, automatic target snapping, the loupe and cursors.

use super::*;

pub(super) unsafe fn update_drag(window: HWND, point: POINT, end: bool) -> bool {
    let dirty = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let previous = selection.rect;
        let drag = selection.drag.as_mut()?;
        drag.started |=
            (drag.anchor.x - point.x).abs() >= 2 || (drag.anchor.y - point.y).abs() >= 2;
        let inside = contains(selection.rect, point);
        let copy = end && drag.copy_on_click && !drag.started && inside;
        if drag.started {
            selection.rect = drag_rect(
                drag.kind,
                drag.previous,
                selection.canvas,
                drag.anchor,
                point,
            );
            selection.automatic = false;
        } else if end && inside {
            selection.automatic = false;
        }
        if end {
            let drag = selection.drag.take()?;
            if drag.started && drag.locked && EqualRect(&drag.previous, &selection.rect) == 0 {
                selection.annotations.record_region(drag.previous);
            }
        }
        Some((previous, selection.rect, selection.canvas, copy))
    });
    if let Some((old, new, canvas, copy)) = dirty {
        invalidate_change(window, old, new, canvas);
        refresh_toolbar(window);
        refresh_hint(window);
        if end {
            ReleaseCapture();
        }
        if copy {
            command(window, Action::Copy);
        }
    }
    dirty.is_some()
}

pub(super) unsafe fn hover(window: HWND, point: POINT) {
    let change = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if !selection.automatic || selection.drag.is_some() || hold_layer(selection, point) {
            return None;
        }
        if selection.pointer.is_some_and(|last| last.x == point.x && last.y == point.y) {
            return None;
        }
        selection.pointer = Some(point);
        let desktop = selection.desktop.bounds;
        let screen_point = POINT {
            x: point.x + desktop.left,
            y: point.y + desktop.top,
        };
        let target = selection
            .targets
            .iter()
            .copied()
            .find(|target| contains(target.rect, screen_point));
        let previous = selection.rect;
        let id = target.map(|target| target.id);
        if id != selection.target {
            selection.visual_probe = None;
            selection.element_layers.clear();
        }
        if id != selection.target || !contains(selection.rect, point) {
            selection.rect = target
                .and_then(|target| overlay_rect(target.rect, desktop))
                .map(|bounds| visual_region(selection, bounds, point).unwrap_or(bounds))
                // Nothing under the pointer: fall back to its whole monitor.
                .or_else(|| {
                    monitor_info(screen_point)
                        .and_then(|info| overlay_rect(info.rcMonitor, desktop))
                })
                .unwrap_or_default();
        }
        selection.target = id;
        if let (Some(probe), Some(target)) = (&selection.probe, target) {
            probe.request(Request {
                window: target,
                point: screen_point,
            });
        }
        Some((previous, selection.rect, selection.canvas))
    });
    if let Some((old, new, canvas)) = change {
        invalidate_change(window, old, new, canvas);
    }
}

pub(super) fn visual_region(selection: &mut Selection, bounds: RECT, point: POINT) -> Option<RECT> {
    if let Some((last, region)) = selection.visual_probe {
        if region.is_some_and(|region| contains(region, point)) {
            return region;
        }
        if (last.x - point.x).abs() < 24 && (last.y - point.y).abs() < 24 {
            return None;
        }
    }
    let region = selection.desktop.visual_panel(bounds, point);
    selection.visual_probe = Some((point, region));
    region
}

pub(super) unsafe fn refine_target(window: HWND) {
    let change = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let found = selection.probe.as_ref()?.latest()?;
        let desktop = selection.desktop.bounds;
        let point = selection.pointer?;
        let screen_point = POINT {
            x: point.x + desktop.left,
            y: point.y + desktop.top,
        };
        if !selection.automatic
            || selection.drag.is_some()
            || selection.target != Some(found.request.window.id)
        {
            return None;
        }
        selection.element_layers = found.layers;
        // A layer picked with the side buttons stays until the pointer moves.
        if selection.layer.is_some() {
            return None;
        }
        let same_point =
            found.request.point.x == screen_point.x && found.request.point.y == screen_point.y;
        let (rect, weak) = match found.region {
            Some(region) if contains(region.rect, screen_point) => (region.rect, region.weak),
            None if same_point => (found.request.window.rect, true),
            _ => return None,
        };
        let previous = selection.rect;
        let bounds = overlay_rect(found.request.window.rect, desktop)?;
        let semantic = overlay_rect(rect, desktop)?;
        let visual = visual_region(selection, bounds, point);
        selection.rect = visual
            .filter(|visual| {
                weak || (visual.left >= semantic.left
                    && visual.top >= semantic.top
                    && visual.right <= semantic.right
                    && visual.bottom <= semantic.bottom
                    && rect_area(*visual) * 5 < rect_area(semantic) * 4)
            })
            .unwrap_or(semantic);
        Some((previous, selection.rect, selection.canvas))
    });
    if let Some((old, new, canvas)) = change {
        invalidate_change(window, old, new, canvas);
    }
}

pub(super) fn rect_area(rect: RECT) -> i64 {
    i64::from(rect.right - rect.left) * i64::from(rect.bottom - rect.top)
}

/// Ctrl+A: selects the whole monitor under the cursor and locks it, so the
/// toolbar appears right away. Ignored while a drag or text edit is active.
pub(super) unsafe fn select_screen(window: HWND) {
    let mut cursor = POINT::default();
    GetCursorPos(&mut cursor);
    let change = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.drag.is_some()
            || selection.text_drag.is_some()
            || selection.mark_drag.is_some()
            || selection.editor.is_some()
            || selection.annotations.draft.is_some()
        {
            return None;
        }
        let screen = monitor_info(cursor)
            .and_then(|info| overlay_rect(info.rcMonitor, selection.desktop.bounds))?;
        let previous = selection.rect;
        if !selection.automatic && EqualRect(&previous, &screen) == 0 {
            selection.annotations.record_region(previous);
        }
        selection.rect = screen;
        selection.automatic = false;
        selection.target = None;
        selection.visual_probe = None;
        Some((previous, screen, selection.canvas))
    });
    if let Some((old, new, canvas)) = change {
        invalidate_change(window, old, new, canvas);
        invalidate(window, &old, 0);
        hide_loupe(window);
        refresh_toolbar(window);
        refresh_hint(window);
        let mut point = cursor;
        ScreenToClient(window, &mut point);
        update_cursor(point);
    }
}

pub(super) unsafe fn reset_automatic(window: HWND, point: POINT) {
    let change = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let previous = selection.rect;
        let was_dragging = selection.drag.take().is_some() || selection.annotations.draft.is_some();
        selection.automatic = true;
        selection.target = None;
        selection.pointer = None;
        selection.layer = None;
        selection.visual_probe = None;
        selection.rect = RECT::default();
        selection.annotations = History::default();
        selection.selected_text = None;
        selection.selected_mark = None;
        selection.mark_drag = None;
        selection.picked.clear();
        selection.pick_drag = None;
        Some((previous, selection.rect, selection.canvas, was_dragging))
    });
    if let Some((old, new, canvas, was_dragging)) = change {
        if was_dragging {
            ReleaseCapture();
        }
        invalidate_change(window, old, new, canvas);
        invalidate(window, &old, 0);
        refresh_toolbar(window);
        refresh_hint(window);
        hide_loupe(window);
        hover(window, point);
        update_cursor(point);
    }
}

pub(super) unsafe fn update_loupe(window: HWND, point: POINT) {
    let change = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let previous = selection.loupe;
        let dragging = selection
            .drag
            .as_ref()
            .is_some_and(|drag| drag.started || !selection.automatic);
        // The recording picker shows the live screen; there is no frozen picture to magnify.
        let visible = selection.record.is_none()
            && selection.annotations.tool.is_none()
            && contains(selection.canvas, point)
            && (dragging || !selection.automatic && contains(selection.rect, point));
        if visible
            && previous.is_some_and(|loupe| loupe.point.x == point.x && loupe.point.y == point.y)
        {
            return None;
        }
        selection.loupe = if visible {
            if !contains(selection.pointer_monitor, point) {
                let bounds = selection.desktop.bounds;
                let cursor = POINT {
                    x: point.x + bounds.left,
                    y: point.y + bounds.top,
                };
                selection.pointer_monitor = monitor_info(cursor)
                    .and_then(|info| overlay_rect(info.rcMonitor, bounds))
                    .unwrap_or(selection.canvas);
            }
            Loupe::new(point, selection.pointer_monitor)
        } else {
            None
        };
        Some((previous, selection.loupe))
    });
    if let Some((previous, current)) = change {
        if let Some(previous) = previous {
            invalidate(window, &previous.bounds, 0);
        } else if current.is_some() {
            let mut tracking = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: window,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut tracking);
        }
        if let Some(current) = current {
            invalidate(window, &current.bounds, 0);
        }
    }
}

/// Copies the color under the magnifier as `#RRGGBB` and says so on it.
pub(super) unsafe fn copy_color(window: HWND) {
    let picked = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let loupe = selection.loupe.as_mut()?;
        loupe.copied = true;
        let color = GetPixel(selection.desktop.surface.dc, loupe.point.x, loupe.point.y);
        Some((loupe.bounds, color))
    });
    let Some((bounds, color)) = picked else { return };
    match crate::output::copy_text(window, &crate::overlay_ui::hex(color)) {
        Ok(()) => {
            invalidate(window, &bounds, 0);
        }
        Err(error) => show_error(window, &format!("无法复制颜色。\n{error}")),
    }
}

pub(super) unsafe fn hide_loupe(window: HWND) {
    let loupe = SESSION.with_borrow_mut(|session| {
        session
            .as_mut()
            .and_then(|selection| selection.loupe.take())
    });
    if let Some(loupe) = loupe {
        invalidate(window, &loupe.bounds, 0);
    }
}

pub(super) unsafe fn update_cursor(point: POINT) {
    let cursor = SESSION.with_borrow_mut(|session| {
        let Some(selection) = session else {
            return LoadCursorW(null_mut(), IDC_CROSS);
        };
        let system = |cursor| LoadCursorW(null_mut(), cursor);
        if let Some(options) = selection
            .options
            .filter(|options| contains(options.bounds, point))
        {
            return system(if options.hit(point).is_some() {
                IDC_HAND
            } else {
                IDC_ARROW
            });
        }
        if let Some(toolbar) = selection
            .toolbar
            .filter(|toolbar| contains(toolbar.bounds, point))
        {
            return system(
                if toolbar
                    .hit(point)
                    .is_some_and(|action| action.enabled(&selection.annotations))
                {
                    IDC_HAND
                } else {
                    IDC_ARROW
                },
            );
        }
        let text_handle = selection
            .text_drag
            .as_ref()
            .map(|drag| (drag.handle, drag.original.angle()))
            .or_else(|| {
                if let Some(editor) = &selection.editor {
                    let frame = editor.frame();
                    frame.hit_chrome(point).map(|handle| (handle, frame.angle()))
                } else {
                    let text = selection.selected_text()?;
                    text.hit(point).map(|handle| (handle, text.angle()))
                }
            });
        if let Some((handle, angle)) = text_handle {
            return system(handle_cursor(handle, angle));
        }
        if selection.editor.is_none() && selection.drag.is_none() {
            if let Some(cursor) = mark_cursor(selection, point) {
                return system(cursor);
            }
        }
        if selection
            .editor
            .as_ref()
            .is_some_and(|editor| editor.frame().contains(point))
        {
            return system(IDC_IBEAM);
        }
        if contains(selection.rect, point) && !selection.automatic && selection.drag.is_none() {
            match selection.annotations.tool {
                Some(Tool::Brush) => {
                    if selection.brush_cursor.is_none() {
                        selection.brush_cursor = Cursor::brush();
                    }
                    if let Some(cursor) = &selection.brush_cursor {
                        return cursor.0;
                    }
                }
                Some(Tool::Mosaic) => {
                    let width = selection.style.mosaic_width;
                    if selection
                        .mosaic_cursor
                        .as_ref()
                        .is_none_or(|(cached, _)| *cached != width)
                    {
                        selection.mosaic_cursor =
                            Cursor::mosaic(width).map(|cursor| (width, cursor));
                    }
                    if let Some((_, cursor)) = &selection.mosaic_cursor {
                        return cursor.0;
                    }
                }
                Some(Tool::Text) => {
                    return system(if selection.annotations.hit_text(point).is_some() {
                        IDC_SIZEALL
                    } else {
                        IDC_IBEAM
                    });
                }
                Some(Tool::Select) => {
                    let over = matches!(selection.pick_drag, Some(PickDrag::Move { .. }))
                        || selection.annotations.hit_any(point).is_some();
                    return system(if over { IDC_SIZEALL } else { IDC_ARROW });
                }
                None if selection.annotations.hit_text(point).is_some() => {
                    return system(IDC_SIZEALL)
                }
                _ => {}
            }
        }
        let kind = selection
            .drag
            .as_ref()
            .map(|drag| drag.kind)
            .unwrap_or_else(|| {
                if selection.automatic
                    || (selection.annotations.tool.is_some() && contains(selection.rect, point))
                {
                    DragKind::Draw
                } else {
                    hit_drag(selection.rect, selection.canvas, point)
                }
            });
        system(match kind {
            DragKind::Draw => IDC_CROSS,
            DragKind::Move => IDC_SIZEALL,
            DragKind::Resize(Handle(0, _)) => IDC_SIZENS,
            DragKind::Resize(Handle(_, 0)) => IDC_SIZEWE,
            DragKind::Resize(Handle(x, y)) if x == y => IDC_SIZENWSE,
            DragKind::Resize(_) => IDC_SIZENESW,
        })
    });
    SetCursor(cursor);
}

pub(super) unsafe fn update_cursor_at_pointer(window: HWND) {
    let mut point = POINT::default();
    if GetCursorPos(&mut point) != 0 && ScreenToClient(window, &mut point) != 0 {
        update_cursor(point);
    }
}
