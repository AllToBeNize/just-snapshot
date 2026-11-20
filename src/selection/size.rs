//! Exact sizes: typing a width and height into the size badge, and nudging
//! the selection or the selected shape with the arrow keys.

use super::*;

/// Posted by the size box: `wparam` is 1 to apply, 0 to cancel.
pub(super) const SIZE_DONE: u32 = WM_APP + 9;

/// "800 600", "800x600", "800×600", "800*600": the first two numbers.
pub(super) fn parse_size(text: &str) -> Option<(i32, i32)> {
    let mut numbers = text
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<i32>().ok());
    let width = numbers.next()??;
    let height = numbers.next()??;
    (width > 0 && height > 0).then_some((width, height))
}

/// `rect` resized to `width`×`height` from its top-left corner, shifted back
/// inside `canvas` when it would hang over the edge.
pub(super) fn sized(rect: RECT, width: i32, height: i32, canvas: RECT) -> RECT {
    let width = width.min(canvas.right - canvas.left);
    let height = height.min(canvas.bottom - canvas.top);
    let left = rect.left.min(canvas.right - width).max(canvas.left);
    let top = rect.top.min(canvas.bottom - height).max(canvas.top);
    RECT { left, top, right: left + width, bottom: top + height }
}

/// A click on the size badge turns it into a box for typing the size.
pub(super) unsafe fn begin_size_input(window: HWND, point: POINT) -> bool {
    let badge = SESSION.with_borrow(|session| {
        let selection = session.as_ref()?;
        if selection.automatic || selection.size_input.is_some() || selection.editor.is_some() {
            return None;
        }
        let badge = size_badge(selection.rect, selection.desktop.bounds, selection.canvas)?;
        let rect = selection.rect;
        contains(badge, point).then_some((badge, rect))
    });
    let Some((badge, rect)) = badge else {
        return false;
    };
    let text = crate::wide(&format!("{} × {}", rect.right - rect.left, rect.bottom - rect.top));
    let input = CreateWindowExW(
        0,
        w!("EDIT"),
        text.as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_BORDER | ES_CENTER as u32 | ES_AUTOHSCROLL as u32,
        badge.left,
        badge.top,
        badge.right - badge.left,
        badge.bottom - badge.top,
        window,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if input.is_null() || SetWindowSubclass(input, Some(size_proc), 2, window as usize) == 0 {
        if !input.is_null() {
            DestroyWindow(input);
        }
        return false;
    }
    SESSION.with_borrow_mut(|session| {
        if let Some(selection) = session.as_mut() {
            selection.size_input = Some(input);
        }
    });
    SendMessageW(input, WM_SETFONT, typography::font(typography::TextStyle::HintDescription) as usize, 1);
    SetFocus(input);
    SendMessageW(input, EM_SETSEL, 0, -1);
    hide_loupe(window);
    true
}

unsafe extern "system" fn size_proc(
    input: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    parent: usize,
) -> LRESULT {
    let parent = parent as HWND;
    match message {
        WM_KEYDOWN if wparam == VK_RETURN as usize => {
            PostMessageW(parent, SIZE_DONE, 1, input as LPARAM);
            return 0;
        }
        WM_KEYDOWN if wparam == VK_ESCAPE as usize => {
            PostMessageW(parent, SIZE_DONE, 0, input as LPARAM);
            return 0;
        }
        // Enter and Esc would beep as characters.
        WM_CHAR if wparam == 13 || wparam == 27 => return 0,
        WM_KILLFOCUS => {
            PostMessageW(parent, SIZE_DONE, 0, input as LPARAM);
        }
        _ => {}
    }
    DefSubclassProc(input, message, wparam, lparam)
}

/// Closes the size box, applying what was typed when `apply` is set.
pub(super) unsafe fn finish_size_input(window: HWND, input: HWND, apply: bool) {
    let current = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        (selection.size_input == Some(input)).then(|| selection.size_input.take())
    });
    if current.is_none() {
        return;
    }
    let typed = apply.then(|| String::from_utf16_lossy(&read_editor_text(input)));
    DestroyWindow(input);
    SetFocus(window);
    let Some(text) = typed else {
        invalidate_badge(window);
        return;
    };
    let Some((width, height)) = parse_size(&text) else {
        show_error(window, &format!("请按「宽 × 高」输入尺寸，例如 800 × 600。\n收到的是：{text}"));
        return;
    };
    set_region(window, |rect, canvas| sized(rect, width, height, canvas), true);
}

unsafe fn invalidate_badge(window: HWND) {
    let badge = SESSION.with_borrow(|session| {
        let selection = session.as_ref()?;
        size_badge(selection.rect, selection.desktop.bounds, selection.canvas)
    });
    if let Some(badge) = badge {
        InvalidateRect(window, &badge, 0);
    }
}

/// Changes the locked selection; `record` makes it an undo step.
unsafe fn set_region(window: HWND, change: impl FnOnce(RECT, RECT) -> RECT, record: bool) {
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.automatic || selection.drag.is_some() {
            return None;
        }
        let previous = selection.rect;
        let next = change(previous, selection.canvas);
        if EqualRect(&previous, &next) != 0 || outline_empty(next) {
            return None;
        }
        if record {
            selection.annotations.record_region(previous);
        }
        selection.rect = next;
        Some((previous, next, selection.canvas))
    });
    if let Some((old, new, canvas)) = changed {
        invalidate_change(window, old, new, canvas);
        InvalidateRect(window, &old, 0);
        InvalidateRect(window, &new, 0);
        refresh_toolbar(window);
        refresh_hint(window);
        invalidate_toolbar(window);
    }
}

/// Arrow keys: move the selected shape, or the selection; Shift moves 10
/// pixels; Ctrl resizes the selection from its bottom-right corner.
pub(super) unsafe fn nudge(window: HWND, key: u32, modifiers: u32, repeat: bool) -> bool {
    let (dx, dy) = match key {
        k if k == u32::from(VK_LEFT) => (-1, 0),
        k if k == u32::from(VK_RIGHT) => (1, 0),
        k if k == u32::from(VK_UP) => (0, -1),
        k if k == u32::from(VK_DOWN) => (0, 1),
        _ => return false,
    };
    let busy = SESSION.with_borrow(|session| {
        session.as_ref().is_none_or(|selection| {
            selection.automatic
                || selection.editor.is_some()
                || selection.drag.is_some()
                || selection.mark_drag.is_some()
                || selection.text_drag.is_some()
                || selection.annotations.draft.is_some()
        })
    });
    if busy || modifiers & !(MOD_SHIFT | MOD_CONTROL) != 0 {
        return false;
    }
    let step = if modifiers & MOD_SHIFT != 0 { 10 } else { 1 };
    let (dx, dy) = (dx * step, dy * step);
    let moved_mark = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let index = selection.selected_mark?;
        let mark = selection.annotations.mark(index)?;
        let anchor = POINT::default();
        let next = mark.dragged(Grip::Move, anchor, POINT { x: dx, y: dy }, selection.rect, &selection.desktop);
        let dirty = [mark.bounds(), next.bounds()];
        selection.annotations.replace(index, next);
        Some(dirty)
    });
    if let Some(dirty) = moved_mark {
        for rect in dirty {
            InvalidateRect(window, &rect, 0);
        }
        invalidate_toolbar(window);
        return true;
    }
    // Holding the key repeats; only the first press starts an undo step.
    if modifiers & MOD_CONTROL != 0 {
        set_region(
            window,
            |rect, canvas| RECT {
                right: (rect.right + dx).clamp(rect.left + 1, canvas.right),
                bottom: (rect.bottom + dy).clamp(rect.top + 1, canvas.bottom),
                ..rect
            },
            !repeat,
        );
    } else {
        set_region(
            window,
            |rect, canvas| {
                let dx = dx.clamp(canvas.left - rect.left, canvas.right - rect.right);
                let dy = dy.clamp(canvas.top - rect.top, canvas.bottom - rect.bottom);
                RECT { left: rect.left + dx, top: rect.top + dy, right: rect.right + dx, bottom: rect.bottom + dy }
            },
            !repeat,
        );
    }
    true
}
