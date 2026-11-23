//! Selecting drawn shapes again: moving them, dragging their handles,
//! restyling them from the option bar and deleting them.

use super::*;

impl Selection {
    fn selected_mark(&self) -> Option<&Mark> {
        self.annotations.mark(self.selected_mark?)
    }

    /// Clears the shape selection; returns the area its frame covered.
    pub(super) fn take_selected_mark(&mut self) -> Option<RECT> {
        let index = self.selected_mark.take()?;
        self.annotations.mark(index).map(Mark::bounds)
    }

    pub(super) fn selected_mark_frame(&self) -> Option<&Mark> {
        self.selected_mark()
    }
}

/// Starts dragging a shape, or the selected shape's handle, under `point`.
/// A click elsewhere drops the shape selection and returns false so the
/// click can draw or move the selection as usual.
pub(super) unsafe fn begin_mark_drag(window: HWND, point: POINT) -> bool {
    let result = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.automatic {
            return None;
        }
        let selected = selection
            .selected_mark
            .and_then(|index| Some((index, selection.annotations.mark(index)?.hit(point, true)?)));
        let hit = selected.or_else(|| {
            contains(selection.rect, point)
                .then(|| selection.annotations.hit_mark(point, selection.annotations.tool))
                .flatten()
        });
        let Some((index, grip)) = hit else {
            return selection.take_selected_mark().map(|bounds| (bounds, None));
        };
        let previous = if selection.selected_mark == Some(index) {
            None
        } else {
            selection.take_selected_mark()
        };
        let mark = selection.annotations.mark(index)?.clone();
        selection.selected_mark = Some(index);
        // The option bar shows the shape's style, as it does for a text box.
        selection.annotations.tool = Some(mark.tool());
        let bounds = mark.bounds();
        selection.mark_drag = Some(MarkDrag {
            index,
            grip,
            anchor: point,
            original: mark,
            moved: false,
        });
        Some((previous.unwrap_or(bounds), Some(bounds)))
    });
    let Some((previous, current)) = result else {
        return false;
    };
    invalidate(window, &previous, 0);
    refresh_options(window);
    refresh_hint(window);
    invalidate_toolbar(window);
    let Some(current) = current else {
        return false;
    };
    invalidate(window, &current, 0);
    SetCapture(window);
    update_cursor(point);
    true
}

pub(super) unsafe fn update_mark_drag(window: HWND, point: POINT, end: bool) -> bool {
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let drag = selection.mark_drag.as_mut()?;
        let clip = selection.rect;
        let point = POINT {
            x: point.x.clamp(clip.left, clip.right - 1),
            y: point.y.clamp(clip.top, clip.bottom - 1),
        };
        drag.moved |= (point.x - drag.anchor.x).abs() >= 1 || (point.y - drag.anchor.y).abs() >= 1;
        let previous = selection.annotations.mark(drag.index)?.bounds();
        let next = drag
            .original
            .dragged(drag.grip, drag.anchor, point, clip, &selection.desktop);
        let current = next.bounds();
        let index = drag.index;
        if end {
            let drag = selection.mark_drag.take().unwrap();
            if drag.moved {
                // Record the change from the shape as it was before the drag.
                selection.annotations.set(index, drag.original);
                selection.annotations.replace(index, next);
            } else {
                selection.annotations.set(index, drag.original);
            }
        } else {
            selection.annotations.set(index, next);
        }
        Some([previous, current])
    });
    let Some(dirty) = changed else {
        return false;
    };
    for rect in dirty {
        invalidate(window, &rect, 0);
    }
    if end {
        ReleaseCapture();
        invalidate_toolbar(window);
        update_cursor(point);
    }
    true
}

/// Puts a shape back where it was when its drag is interrupted.
pub(super) unsafe fn cancel_mark_drag(window: HWND) {
    let dirty = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let drag = selection.mark_drag.take()?;
        let previous = selection.annotations.mark(drag.index)?.bounds();
        let current = drag.original.bounds();
        selection.annotations.set(drag.index, drag.original);
        Some([previous, current])
    });
    for rect in dirty.into_iter().flatten() {
        invalidate(window, &rect, 0);
    }
}

/// Delete or Backspace: removes the selected shape or text box.
pub(super) unsafe fn delete_selected(window: HWND) -> bool {
    let removed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.mark_drag.is_some() || selection.text_drag.is_some() || selection.editor.is_some() {
            return None;
        }
        if let Some(index) = selection.selected_mark.take() {
            let bounds = selection.annotations.mark(index)?.bounds();
            selection.annotations.remove(index);
            return Some(bounds);
        }
        let index = selection.selected_text.take()?;
        let bounds = selection.annotations.text(index)?.bounds();
        selection.annotations.remove_text(index);
        Some(bounds)
    });
    let Some(bounds) = removed else {
        return false;
    };
    invalidate(window, &bounds, 0);
    refresh_options(window);
    refresh_hint(window);
    invalidate_toolbar(window);
    update_cursor_at_pointer(window);
    true
}

/// Drops the shape selection, keeping the tool. True if one was selected.
pub(super) unsafe fn deselect_mark(window: HWND) -> bool {
    let bounds = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.mark_drag.is_some() {
            return None;
        }
        selection.take_selected_mark()
    });
    let Some(bounds) = bounds else {
        return false;
    };
    invalidate(window, &bounds, 0);
    refresh_options(window);
    refresh_hint(window);
    true
}

/// The cursor over a shape that a click would select or drag.
pub(super) fn mark_cursor(selection: &Selection, point: POINT) -> Option<*const u16> {
    let grip = selection
        .mark_drag
        .as_ref()
        .map(|drag| drag.grip)
        .or_else(|| selection.selected_mark()?.hit(point, true))
        .or_else(|| {
            if selection.automatic || !contains(selection.rect, point) {
                return None;
            }
            selection
                .annotations
                .hit_mark(point, selection.annotations.tool)
                .map(|(_, grip)| grip)
        })?;
    Some(match grip {
        Grip::Move | Grip::Lens => IDC_SIZEALL,
        Grip::Start | Grip::End | Grip::Radius => IDC_SIZEALL,
        Grip::Resize(0, _) => IDC_SIZENS,
        Grip::Resize(_, 0) => IDC_SIZEWE,
        Grip::Resize(x, y) if x == y => IDC_SIZENWSE,
        Grip::Resize(_, _) => IDC_SIZENESW,
    })
}
