//! Text boxes: dragging, the hidden native editor, caret, IME and committing text.

use super::*;

pub(super) unsafe fn begin_text_drag(window: HWND, point: POINT, double: bool) -> bool {
    let target = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.automatic || !matches!(selection.annotations.tool, None | Some(Tool::Text)) {
            return None;
        }
        let previous = selection.selected_text().map(TextMark::bounds);
        let (index, handle) = selection
            .selected_text
            .zip(selection.selected_text().and_then(|text| text.hit(point)))
            .or_else(|| {
                contains(selection.rect, point)
                    .then(|| selection.annotations.hit_text(point))
                    .flatten()
                    .map(|index| (index, TextHandle::Move))
            })?;
        let text = selection.annotations.text(index)?.clone();
        let bounds = text.bounds();
        selection.selected_text = Some(index);
        selection.annotations.tool = Some(Tool::Text);
        let edit = double && matches!(handle, TextHandle::Move);
        if edit {
            Some((previous, bounds, Some((index, text))))
        } else {
            selection.text_drag = Some(TextDrag {
                index: Some(index),
                anchor: point,
                handle,
                original: text,
                frame: None,
                caret: None,
            });
            Some((previous, bounds, None))
        }
    });
    let Some((previous, bounds, edit)) = target else {
        return false;
    };
    for rect in previous.into_iter().chain([bounds]) {
        InvalidateRect(window, &rect, 0);
    }
    refresh_options(window);
    refresh_hint(window);
    invalidate_toolbar(window);
    if let Some((index, text)) = edit {
        open_text(window, Some(index), text);
    } else {
        SetCapture(window);
        update_cursor(point);
    }
    true
}

pub(super) unsafe fn begin_editor_drag(window: HWND, point: POINT) -> bool {
    let target = SESSION.with_borrow(|session| {
        let editor = session.as_ref()?.editor.as_ref()?;
        let frame = editor.frame();
        Some((editor.window, frame.hit_chrome(point)?, frame.into_owned()))
    });
    let Some((editor, handle, frame)) = target else {
        return false;
    };
    let bounds = frame.bounds();
    suppress_native_caret(editor);
    let value = read_editor_text(editor);
    let (preview, caret) = if matches!(handle, TextHandle::Rotate) {
        (text_render::snapshot_input(editor), input_caret(editor))
    } else {
        (None, None)
    };
    SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut().unwrap();
        let editor = selection.editor.as_mut().unwrap();
        editor.text.text = value;
        editor.preview = preview;
        if !editor.text.text.is_empty() {
            editor.text.fit_text(selection.desktop.surface.dc);
            let index = if let Some(index) = editor.index {
                selection
                    .annotations
                    .replace_text(index, editor.text.clone());
                index
            } else {
                selection.annotations.push(Mark::Text(editor.text.clone()))
            };
            editor.index = Some(index);
            selection.selected_text = Some(index);
        }
        selection.text_drag = Some(TextDrag {
            index: None,
            anchor: point,
            handle,
            original: editor.text.clone(),
            frame: Some(frame),
            caret,
        });
    });
    SetCapture(window);
    KillTimer(window, TEXT_CARET_TIMER);
    ShowWindow(editor, SW_HIDE);
    SetFocus(window);
    InvalidateRect(window, &bounds, 0);
    refresh_options(window);
    SetCursor(LoadCursorW(null_mut(), text_handle_cursor(handle)));
    true
}

pub(super) unsafe fn update_text_drag(window: HWND, point: POINT, end: bool) -> bool {
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let drag = selection.text_drag.as_ref()?;
        let previous = selection.text_frame()?.bounds();
        let limit = if matches!(drag.handle, TextHandle::Move) {
            selection.rect
        } else {
            selection.canvas
        };
        let point = POINT {
            x: point.x.clamp(limit.left, limit.right - 1),
            y: point.y.clamp(limit.top, limit.bottom - 1),
        };
        let mut next = match &drag.frame {
            Some(frame) => {
                drag.original
                    .transform_drag_from(frame, drag.handle, drag.anchor, point)
            }
            None => drag
                .original
                .transform_drag(drag.handle, drag.anchor, point),
        };
        if matches!(drag.handle, TextHandle::Scale(_, _)) && next.size != drag.original.size {
            next.fit_text(selection.desktop.surface.dc);
        }
        let current = next.bounds();
        let editing = drag.index.is_none();
        if end {
            let drag = selection.text_drag.take().unwrap();
            if let Some(index) = drag.index {
                *selection.annotations.text_mut(index)? = drag.original;
                selection.annotations.replace_text(index, next);
            } else {
                let editor = selection.editor.as_mut()?;
                if let Some(index) = editor.index.filter(|_| !next.text.is_empty()) {
                    selection.annotations.replace_text(index, next.clone());
                }
                editor.text = next;
            }
        } else if let Some(index) = drag.index {
            *selection.annotations.text_mut(index)? = next;
        } else {
            selection.editor.as_mut()?.text = next;
        }
        Some(([previous, current], editing))
    });
    if let Some((dirty, editing)) = changed {
        for rect in dirty {
            InvalidateRect(window, &rect, 0);
        }
        if end {
            ReleaseCapture();
            invalidate_toolbar(window);
            if editing {
                resume_editor(window);
                invalidate_text_selection(window);
            } else {
                update_cursor(point);
            }
        }
        refresh_options(window);
    }
    changed.is_some()
}

pub(super) unsafe fn cancel_text_drag(window: HWND) {
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let drag = selection.text_drag.take()?;
        let text = if let Some(index) = drag.index {
            selection.annotations.text_mut(index)?
        } else {
            &mut selection.editor.as_mut()?.text
        };
        let previous = text.bounds();
        *text = drag.original;
        Some(([previous, text.bounds()], drag.index.is_none()))
    });
    if let Some((dirty, editing)) = changed {
        for rect in dirty {
            InvalidateRect(window, &rect, 0);
        }
        refresh_options(window);
        invalidate_toolbar(window);
        if editing {
            resume_editor(window);
            invalidate_text_selection(window);
        }
    }
}

pub(super) unsafe fn open_text(window: HWND, index: Option<usize>, text: TextMark) {
    let rect = SESSION.with_borrow(|session| text_editor_bounds(session.as_ref().unwrap(), &text));
    let content: Vec<u16> = text.text.iter().copied().chain([0]).collect();
    let input = editor_input_rect(rect);
    let width = input.right - input.left;
    let height = input.bottom - input.top;
    let editor = CreateWindowExW(
        0,
        w!("EDIT"),
        content.as_ptr(),
        WS_CHILD | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32,
        0,
        -height - 1,
        width,
        height,
        window,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if editor.is_null()
        || SetWindowSubclass(editor, Some(text_proc), 1, window as usize) == 0
        || !clip_native_editor(editor)
    {
        let error = io::Error::last_os_error();
        if !editor.is_null() {
            DestroyWindow(editor);
        }
        show_error(window, &format!("无法创建文本框。\n{error}"));
        return;
    }
    let font = typography::annotation_font(text.size);
    let bounds = text.bounds();
    let frame = if text.is_rotated() {
        bounds
    } else {
        TextMark::new(rect, text.size, text.color).bounds()
    };
    SESSION.with_borrow_mut(|session| {
        session.as_mut().unwrap().editor = Some(Editor {
            window: editor,
            rect,
            index,
            text,
            preview: None,
            caret_visible: true,
        });
    });
    SendMessageW(editor, WM_SETFONT, font as usize, 0);
    set_editor_format(editor, input);
    start_text_caret(window, editor);
    ShowWindow(editor, SW_SHOW);
    SetFocus(editor);
    SetCursor(LoadCursorW(null_mut(), IDC_IBEAM));
    InvalidateRect(window, &bounds, 0);
    InvalidateRect(window, &frame, 0);
    refresh_options(window);
    refresh_hint(window);
}

pub(super) unsafe fn resume_editor(window: HWND) {
    let layout = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let editor = selection.editor.as_ref()?;
        let rect = text_editor_bounds(selection, &editor.text);
        let editor = selection.editor.as_mut().unwrap();
        let previous = editor.frame().bounds();
        editor.rect = rect;
        Some((
            editor.window,
            editor.rect,
            previous,
            editor.frame().bounds(),
            editor.text.size,
        ))
    });
    let Some((editor, rect, previous, current, size)) = layout else {
        return;
    };
    SendMessageW(
        editor,
        WM_SETFONT,
        typography::annotation_font(size) as usize,
        0,
    );
    position_editor(editor, rect);
    start_text_caret(window, editor);
    ShowWindow(editor, SW_SHOW);
    SetFocus(editor);
    InvalidateRect(editor, null(), 1);
    InvalidateRect(window, &previous, 0);
    InvalidateRect(window, &current, 0);
    update_cursor_at_pointer(window);
}

pub(super) fn editor_input_rect(mut frame: RECT) -> RECT {
    let inset_x = ((frame.right - frame.left - 1) / 2).min(2);
    let inset_y = ((frame.bottom - frame.top - 1) / 2).min(2);
    frame.left += inset_x;
    frame.top += inset_y;
    frame.right -= inset_x;
    frame.bottom -= inset_y;
    frame
}

pub(super) unsafe fn position_editor(editor: HWND, frame: RECT) {
    let rect = editor_input_rect(frame);
    let height = rect.bottom - rect.top;
    SetWindowPos(
        editor,
        null_mut(),
        0,
        -height - 1,
        rect.right - rect.left,
        height,
        SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOCOPYBITS,
    );
    set_editor_format(editor, rect);
}

pub(super) unsafe fn clip_native_editor(editor: HWND) -> bool {
    // Keep native keyboard editing and caret coordinates without a visible child surface.
    let region = CreateRectRgn(0, 0, 0, 0);
    if region.is_null() {
        return false;
    }
    if SetWindowRgn(editor, region, 0) != 0 {
        return true;
    }
    DeleteObject(region);
    false
}

pub(super) unsafe fn start_text_caret(window: HWND, editor: HWND) {
    KillTimer(window, TEXT_CARET_TIMER);
    let blink = GetCaretBlinkTime();
    if blink != u32::MAX {
        SetTimer(window, TEXT_CARET_TIMER, blink, None);
    }
    invalidate_text_input(window, editor);
}

pub(super) unsafe fn invalidate_text_selection(window: HWND) {
    if let Some(rect) =
        SESSION.with_borrow(|session| session.as_ref().map(|selection| selection.rect))
    {
        InvalidateRect(window, &rect, 0);
    }
}

pub(super) unsafe fn invalidate_text_input(window: HWND, input: HWND) {
    suppress_native_caret(input);
    let bounds = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if selection.text_drag.is_some() {
            return None;
        }
        let editor = selection.editor.as_mut()?;
        if editor.window != input {
            return None;
        }
        editor.preview = None;
        editor.caret_visible = true;
        Some(editor.frame().bounds())
    });
    if let Some(bounds) = bounds {
        InvalidateRect(window, &bounds, 0);
    }
}

pub(super) unsafe fn refresh_text_preview() {
    let input = SESSION.with_borrow(|session| {
        let selection = session.as_ref()?;
        let editor = selection.editor.as_ref()?;
        (selection.text_drag.is_none() && editor.preview.is_none()).then_some(editor.window)
    });
    if let Some(input) = input {
        let image = text_render::snapshot_input(input);
        SESSION.with_borrow_mut(|session| {
            if let Some(editor) = session
                .as_mut()
                .and_then(|s| s.editor.as_mut())
                .filter(|e| e.window == input)
            {
                editor.preview = image;
            }
        });
    }
}

pub(super) unsafe fn input_caret(window: HWND) -> Option<RECT> {
    input_caret_info(window).map(|info| info.rcCaret)
}

pub(super) unsafe fn input_caret_info(window: HWND) -> Option<GUITHREADINFO> {
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let thread = GetWindowThreadProcessId(window, null_mut());
    (GetGUIThreadInfo(thread, &mut info) != 0
        && info.hwndCaret == window
        && info.hwndFocus == window)
        .then_some(info)
}

pub(super) unsafe fn suppress_native_caret(window: HWND) {
    if input_caret_info(window).is_some_and(|info| info.flags & GUI_CARETBLINKING != 0) {
        HideCaret(window);
    }
}

pub(super) unsafe fn position_input_method(parent: HWND, editor: HWND) {
    let Some(caret) = input_caret(editor) else {
        return;
    };
    let Some(mut point) = SESSION.with_borrow(|session| {
        let input = session.as_ref()?.editor.as_ref()?;
        Some(input.frame().input_screen_point(POINT {
            x: caret.left,
            y: caret.bottom,
        }))
    }) else {
        return;
    };
    MapWindowPoints(parent, editor, &mut point, 1);
    let context = ImmGetContext(editor);
    if !context.is_null() {
        ImmSetCompositionWindow(
            context,
            &COMPOSITIONFORM {
                dwStyle: CFS_FORCE_POSITION,
                ptCurrentPos: point,
                ..Default::default()
            },
        );
        ImmSetCandidateWindow(
            context,
            &CANDIDATEFORM {
                dwStyle: CFS_CANDIDATEPOS,
                ptCurrentPos: point,
                ..Default::default()
            },
        );
        ImmReleaseContext(editor, context);
    }
}

pub(super) unsafe fn set_editor_format(editor: HWND, rect: RECT) {
    let format_rect = RECT {
        left: 1,
        top: 1,
        right: (rect.right - rect.left - 1).max(2),
        bottom: (rect.bottom - rect.top - 1).max(2),
    };
    SendMessageW(editor, EM_SETRECT, 0, &format_rect as *const RECT as LPARAM);
}

pub(super) fn text_editor_bounds(selection: &Selection, text: &TextMark) -> RECT {
    let clip = selection.rect;
    let mut rect = text.edit_rect(clip);
    if text.is_rotated() {
        return rect;
    }
    let Some(toolbar) = selection
        .toolbar
        .map(|toolbar| toolbar.bounds)
        .filter(|bounds| intersect(*bounds, rect).is_some())
    else {
        return rect;
    };
    let top = toolbar.top;
    let bottom = toolbar.bottom;
    let above = top - clip.top - 2;
    let below = clip.bottom - bottom - 2;
    let height = rect.bottom - rect.top;
    if above >= below && above >= 20 {
        rect.bottom = top - 2;
        rect.top = rect.bottom - height.min(above);
    } else if below >= 20 {
        rect.top = bottom + 2;
        rect.bottom = rect.top + height.min(below);
    }
    rect
}

pub(super) unsafe extern "system" fn text_proc(
    editor: HWND,
    message: u32,
    key: WPARAM,
    lparam: LPARAM,
    _: usize,
    parent: usize,
) -> LRESULT {
    let parent = parent as HWND;
    let from_overlay = message == TEXT_INPUT_CLICK;
    let (message, key) = if from_overlay {
        ((key >> 16) as u32, key & 0xffff)
    } else {
        (message, key)
    };
    if message == WM_SETCURSOR {
        let mut point = POINT::default();
        let handle = if GetCursorPos(&mut point) != 0 && ScreenToClient(parent, &mut point) != 0 {
            SESSION
                .with_borrow(|session| session.as_ref()?.editor.as_ref()?.frame().hit_chrome(point))
        } else {
            None
        };
        SetCursor(LoadCursorW(
            null_mut(),
            handle.map_or(IDC_IBEAM, text_handle_cursor),
        ));
        return 1;
    }
    if !from_overlay
        && matches!(
            message,
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK
        )
    {
        let mut point = mouse_point(lparam);
        if matches!(message, WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK) {
            ScreenToClient(parent, &mut point);
        } else {
            MapWindowPoints(editor, parent, &mut point, 1);
        }
        if begin_editor_drag(parent, point) {
            return 0;
        }
    }
    if message == WM_KEYDOWN {
        let escape = key == VK_ESCAPE as usize;
        let control = GetKeyState(VK_CONTROL as i32) < 0;
        if escape || (key == VK_RETURN as usize && control) {
            PostMessageW(parent, FINISH_TEXT, usize::from(!escape), editor as LPARAM);
            return 0;
        }
    }
    let lparam = if matches!(
        message,
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_LBUTTONUP | WM_MOUSEMOVE
    ) {
        let mut point = mouse_point(lparam);
        if !from_overlay {
            MapWindowPoints(editor, parent, &mut point, 1);
        }
        SESSION
            .with_borrow(|session| {
                let input = session.as_ref()?.editor.as_ref()?;
                Some(point_param(input.frame().input_point(point)))
            })
            .unwrap_or(lparam)
    } else {
        lparam
    };
    let result = DefSubclassProc(editor, message, key, lparam);
    if matches!(message, WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION)
        || (message == WM_IME_NOTIFY && key == IMN_OPENCANDIDATE as usize)
    {
        position_input_method(parent, editor);
    }
    if matches!(
        message,
        WM_KEYDOWN
            | WM_CHAR
            | WM_SETTEXT
            | WM_SETFOCUS
            | WM_KILLFOCUS
            | WM_SETFONT
            | WM_SIZE
            | WM_LBUTTONDOWN
            | WM_LBUTTONDBLCLK
            | WM_LBUTTONUP
            | WM_VSCROLL
            | WM_MOUSEWHEEL
            | WM_IME_COMPOSITION
            | WM_IME_ENDCOMPOSITION
            | EM_SETSEL
            | EM_SETRECT
    ) || (message == WM_MOUSEMOVE && key & 1 != 0)
    {
        invalidate_text_input(parent, editor);
    }
    result
}

pub(super) unsafe fn read_editor_text(editor: HWND) -> Vec<u16> {
    let length = GetWindowTextLengthW(editor);
    let mut value = vec![0; length as usize + 1];
    let read = GetWindowTextW(editor, value.as_mut_ptr(), value.len() as i32);
    value.truncate(read as usize);
    value
}

pub(super) unsafe fn finish_text(window: HWND, save: bool) {
    let dragging_editor = SESSION.with_borrow(|session| {
        session
            .as_ref()
            .and_then(|selection| selection.text_drag.as_ref())
            .is_some_and(|drag| drag.index.is_none())
    });
    if dragging_editor {
        cancel_text_drag(window);
        ReleaseCapture();
    }
    let editor = SESSION.with_borrow_mut(|session| session.as_mut()?.editor.take());
    let Some(editor) = editor else { return };
    KillTimer(window, TEXT_CARET_TIMER);
    let changed = if save {
        let value = read_editor_text(editor.window);
        SESSION.with_borrow_mut(|session| {
            let selection = session.as_mut().unwrap();
            if value.is_empty() {
                if let Some(index) = editor.index {
                    selection.annotations.remove_text(index);
                }
                selection.selected_text = None;
                return None;
            }
            let mut text = editor.text.clone();
            text.text = value;
            text.fit_text(selection.desktop.surface.dc);
            let bounds = text.bounds();
            selection.selected_text = if let Some(index) = editor.index {
                selection.annotations.replace_text(index, text);
                Some(index)
            } else {
                selection.style.text_size = text.size;
                selection.style.text_color = text.color;
                Some(selection.annotations.push(Mark::Text(text)))
            };
            Some(bounds)
        })
    } else {
        None
    };
    DestroyWindow(editor.window);
    SetFocus(window);
    for rect in [
        Some(editor.frame().bounds()),
        Some(editor.text.bounds()),
        changed,
    ]
    .into_iter()
    .flatten()
    {
        InvalidateRect(window, &rect, 0);
    }
    invalidate_text_selection(window);
    refresh_options(window);
    invalidate_toolbar(window);
    refresh_hint(window);
}
