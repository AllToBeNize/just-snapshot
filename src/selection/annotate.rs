//! Freehand, rectangle and mosaic strokes, and leaving the current tool.

use super::*;

pub(super) unsafe fn begin_annotation(window: HWND, point: POINT) -> bool {
    let started = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.automatic || !contains(selection.rect, point) {
            return None;
        }
        let tool = selection.annotations.tool?;
        let previous = selection
            .selected_text()
            .map(TextMark::bounds)
            .or_else(|| selection.take_selected_mark());
        selection.selected_text = None;
        let draft = Draft::new(
            tool,
            point,
            selection.style,
            selection.rect,
            &selection.desktop,
        );
        let dirty = draft.start_bounds();
        selection.annotations.draft = Some(draft);
        Some((dirty, previous))
    });
    if let Some((dirty, previous)) = started {
        InvalidateRect(window, &dirty, 0);
        if let Some(previous) = previous {
            InvalidateRect(window, &previous, 0);
            refresh_options(window);
            refresh_hint(window);
        }
    }
    started.is_some()
}

pub(super) unsafe fn update_annotation(window: HWND, point: POINT, end: bool) -> bool {
    let change = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let draft = selection.annotations.draft.as_mut()?;
        let dirty = draft.update(point, selection.rect, &selection.desktop);
        let finished = end.then(|| selection.annotations.draft.take().unwrap());
        Some((dirty, selection.rect, selection.style, finished))
    });
    let Some((dirty, clip, style, finished)) = change else {
        return false;
    };
    InvalidateRect(window, &dirty, 0);
    if let Some(draft) = finished {
        ReleaseCapture();
        if draft.tool == Tool::Text {
            let mut text = TextMark::new(draft.text_rect(clip), style.text_size, style.text_color);
            text.look = style.text_look;
            open_text(window, None, text);
        } else {
            let added = SESSION.with_borrow_mut(|session| {
                let selection = session.as_mut().unwrap();
                let next = selection.annotations.next_number();
                let mark = draft.finish(next, clip, &selection.desktop)?;
                let bounds = mark.bounds();
                selection.annotations.push(mark);
                Some(bounds)
            });
            // A lens appears away from where it was dragged.
            if let Some(bounds) = added {
                InvalidateRect(window, &bounds, 0);
            }
        }
        invalidate_toolbar(window);
    }
    true
}

pub(super) unsafe fn leave_tool(window: HWND, point: POINT) -> bool {
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        selection.annotations.tool.take()?;
        let frame = selection
            .selected_text()
            .map(TextMark::bounds)
            .or_else(|| selection.take_selected_mark());
        selection.selected_text = None;
        let draft = selection.annotations.draft.take().is_some();
        Some((
            selection.rect,
            frame,
            draft || selection.text_drag.is_some(),
        ))
    });
    if let Some((rect, frame, captured)) = changed {
        if captured {
            cancel_text_drag(window);
            ReleaseCapture();
        }
        for rect in [Some(rect), frame].into_iter().flatten() {
            InvalidateRect(window, &rect, 0);
        }
        refresh_options(window);
        refresh_hint(window);
        invalidate_toolbar(window);
        update_cursor(point);
    }
    changed.is_some()
}
