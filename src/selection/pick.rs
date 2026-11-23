//! The select tool: click a mark to pick it (Ctrl adds or removes), drag a
//! box to pick several, then drag any picked mark to move them together.
//! Delete removes them and the arrow keys nudge them; each is one undo step.

use super::*;
use crate::ui;

pub(super) enum PickDrag {
    /// A box from `anchor` to `current`; `base` was picked before it (Ctrl).
    Box { anchor: POINT, current: POINT, base: Vec<usize> },
    /// Moving every picked mark; `originals` are where they started.
    Move { anchor: POINT, originals: Vec<(usize, Mark)>, moved: bool },
}

fn boxed(anchor: POINT, current: POINT) -> RECT {
    RECT {
        left: anchor.x.min(current.x),
        top: anchor.y.min(current.y),
        right: anchor.x.max(current.x) + 1,
        bottom: anchor.y.max(current.y) + 1,
    }
}

/// The area the pick chrome covers: every picked mark and the box.
pub(super) fn pick_bounds(selection: &Selection) -> Vec<RECT> {
    let mut rects: Vec<RECT> = selection
        .picked
        .iter()
        .filter_map(|&index| selection.annotations.mark(index))
        .map(Mark::bounds)
        .collect();
    if let Some(PickDrag::Box { anchor, current, .. }) = &selection.pick_drag {
        rects.push(ui::inflate(boxed(*anchor, *current), 2));
    }
    rects
}

unsafe fn invalidate_all(window: HWND, rects: impl IntoIterator<Item = RECT>) {
    for rect in rects {
        invalidate(window, &rect, 0);
    }
}

pub(super) unsafe fn begin_pick(window: HWND, point: POINT, adding: bool) -> bool {
    let started = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.annotations.tool != Some(Tool::Select) || selection.automatic || !contains(selection.rect, point) {
            return None;
        }
        let before = pick_bounds(selection);
        selection.selected_text = None;
        selection.selected_mark = None;
        match selection.annotations.hit_any(point).filter(|&index| selection.annotations.mark(index).is_some_and(Mark::movable)) {
            Some(index) => {
                if adding {
                    if let Some(position) = selection.picked.iter().position(|&picked| picked == index) {
                        selection.picked.remove(position);
                        return Some(before);
                    }
                    selection.picked.push(index);
                } else if !selection.picked.contains(&index) {
                    selection.picked = vec![index];
                }
                let originals = selection
                    .picked
                    .iter()
                    .filter_map(|&index| Some((index, selection.annotations.mark(index)?.clone())))
                    .collect();
                selection.pick_drag = Some(PickDrag::Move { anchor: point, originals, moved: false });
            }
            None => {
                let base = if adding { selection.picked.clone() } else { Vec::new() };
                selection.picked = base.clone();
                selection.pick_drag = Some(PickDrag::Box { anchor: point, current: point, base });
            }
        }
        Some(before.into_iter().chain(pick_bounds(selection)).collect::<Vec<_>>())
    });
    let Some(dirty) = started else {
        return false;
    };
    invalidate_all(window, dirty);
    refresh_hint(window);
    if SESSION.with_borrow(|session| session.as_ref().is_some_and(|selection| selection.pick_drag.is_some())) {
        SetCapture(window);
    }
    true
}

pub(super) unsafe fn update_pick(window: HWND, point: POINT, end: bool) -> bool {
    let dirty = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let clip = selection.rect;
        let point = POINT {
            x: point.x.clamp(clip.left, clip.right - 1),
            y: point.y.clamp(clip.top, clip.bottom - 1),
        };
        let before = pick_bounds(selection);
        match selection.pick_drag.as_mut()? {
            PickDrag::Box { anchor, current, base } => {
                *current = point;
                let mut picked = base.clone();
                for index in selection.annotations.in_area(boxed(*anchor, point)) {
                    if !picked.contains(&index) {
                        picked.push(index);
                    }
                }
                selection.picked = picked;
            }
            PickDrag::Move { anchor, originals, moved } => {
                let (dx, dy) = (point.x - anchor.x, point.y - anchor.y);
                *moved |= dx != 0 || dy != 0;
                for (index, original) in originals.iter() {
                    let next = original.moved(dx, dy, clip, &selection.desktop);
                    selection.annotations.set(*index, next);
                }
            }
        }
        if end {
            if let Some(PickDrag::Move { originals, moved: true, .. }) = selection.pick_drag.take() {
                // One undo step for the whole move, from where the marks started.
                let finals: Vec<(usize, Mark)> = originals
                    .iter()
                    .filter_map(|(index, _)| Some((*index, selection.annotations.mark(*index)?.clone())))
                    .collect();
                for (index, original) in originals {
                    selection.annotations.set(index, original);
                }
                selection.annotations.replace_many(finals);
            }
        }
        Some(before.into_iter().chain(pick_bounds(selection)).collect::<Vec<_>>())
    });
    let Some(dirty) = dirty else {
        return false;
    };
    invalidate_all(window, dirty);
    if end {
        ReleaseCapture();
        invalidate_toolbar(window);
        refresh_hint(window);
    }
    true
}

/// Puts moved marks back when a drag is interrupted.
pub(super) unsafe fn cancel_pick(window: HWND) {
    let dirty = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let before = pick_bounds(selection);
        if let PickDrag::Move { originals, .. } = selection.pick_drag.take()? {
            for (index, original) in originals {
                selection.annotations.set(index, original);
            }
        }
        Some(before.into_iter().chain(pick_bounds(selection)).collect::<Vec<_>>())
    });
    if let Some(dirty) = dirty {
        invalidate_all(window, dirty);
    }
}

/// Drops the picks. True if anything was picked.
pub(super) unsafe fn clear_picks(window: HWND) -> bool {
    let dirty = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.picked.is_empty() || selection.pick_drag.is_some() {
            return None;
        }
        let dirty = pick_bounds(selection);
        selection.picked.clear();
        Some(dirty)
    });
    let Some(dirty) = dirty else {
        return false;
    };
    invalidate_all(window, dirty);
    refresh_hint(window);
    true
}

/// Delete: removes every picked mark as one undo step.
pub(super) unsafe fn delete_picks(window: HWND) -> bool {
    let dirty = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.picked.is_empty() || selection.pick_drag.is_some() {
            return None;
        }
        let dirty = pick_bounds(selection);
        let picked = std::mem::take(&mut selection.picked);
        selection.annotations.remove_many(&picked);
        Some(dirty)
    });
    let Some(dirty) = dirty else {
        return false;
    };
    invalidate_all(window, dirty);
    invalidate_toolbar(window);
    refresh_hint(window);
    true
}

/// Arrow keys move the picked marks together.
pub(super) unsafe fn nudge_picks(window: HWND, dx: i32, dy: i32) -> bool {
    let dirty = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.picked.is_empty() || selection.pick_drag.is_some() {
            return None;
        }
        let before = pick_bounds(selection);
        let changes: Vec<(usize, Mark)> = selection
            .picked
            .iter()
            .filter_map(|&index| Some((index, selection.annotations.mark(index)?.moved(dx, dy, selection.rect, &selection.desktop))))
            .collect();
        selection.annotations.replace_many(changes);
        Some(before.into_iter().chain(pick_bounds(selection)).collect::<Vec<_>>())
    });
    let Some(dirty) = dirty else {
        return false;
    };
    invalidate_all(window, dirty);
    invalidate_toolbar(window);
    true
}

/// Dashed outlines on the picked marks and a translucent pick box.
pub(super) unsafe fn paint_picks(selection: &Selection, dc: HDC) {
    if selection.picked.is_empty() && selection.pick_drag.is_none() {
        return;
    }
    let Some(graphics) = crate::gdiplus::Graphics::new(dc) else { return };
    let Some(dashed) = graphics.dashed_pen(BORDER_BLUE, 1.0) else { return };
    for mark in selection.picked.iter().filter_map(|&index| selection.annotations.mark(index)) {
        let rect = ui::inflate(mark.extent(), 3);
        let corners = [
            (rect.left, rect.top),
            (rect.right, rect.top),
            (rect.right, rect.bottom),
            (rect.left, rect.bottom),
            (rect.left, rect.top),
        ]
        .map(|(x, y)| windows_sys::Win32::Graphics::GdiPlus::PointF { X: x as f32, Y: y as f32 });
        graphics.lines(&dashed, &corners);
    }
    if let Some(PickDrag::Box { anchor, current, .. }) = &selection.pick_drag {
        let area = ui::to_f(boxed(*anchor, *current));
        graphics.fill_round(area, 0.0, crate::theme::argb(BORDER_BLUE, 40));
        graphics.stroke_round(area, 0.0, crate::theme::argb(BORDER_BLUE, 220), 1.0);
    }
}
