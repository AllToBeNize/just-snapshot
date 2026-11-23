//! Toolbar, option bar and hint panel state, plus toolbar command dispatch.

use super::*;

pub(super) unsafe fn refresh_toolbar(window: HWND) {
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let previous = selection.toolbar.take();
        if !selection.automatic && selection.drag.is_none() && !outline_empty(selection.rect) {
            let bounds = selection.desktop.bounds;
            let point = POINT {
                x: selection.rect.right - 1 + bounds.left,
                y: selection.rect.bottom - 1 + bounds.top,
            };
            let area = monitor_info(point)
                .and_then(|info| overlay_rect(info.rcMonitor, bounds))
                .unwrap_or(selection.canvas);
            selection.toolbar = if selection.record.is_some() {
                Toolbar::recording(selection.rect, area)
            } else {
                Toolbar::new(selection.rect, area)
            };
        }
        if let (Some(old), Some(new)) = (previous, selection.toolbar) {
            if EqualRect(&old.bounds, &new.bounds) != 0 {
                return None;
            }
        }
        let old_tip = previous
            .zip(selection.toolbar_hover.take())
            .and_then(|(toolbar, action)| toolbar.tooltip(action).map(|(bounds, _)| bounds));
        Some([
            previous.map(|toolbar| toolbar.bounds),
            selection.toolbar.map(|toolbar| toolbar.bounds),
            old_tip,
        ])
    });
    if let Some(changed) = changed {
        for bounds in changed.into_iter().flatten() {
            invalidate(window, &bounds, 0);
        }
    }
    refresh_options(window);
}

pub(super) unsafe fn refresh_options(window: HWND) {
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let previous = selection.options.take();
        let rotating = selection
            .text_drag
            .as_ref()
            .is_some_and(|drag| matches!(drag.handle, TextHandle::Rotate));
        selection.options = selection
            .toolbar
            .filter(|_| !rotating)
            .zip(selection.annotations.tool)
            .and_then(|(toolbar, tool)| toolbar.options(tool, selection.rect));
        selection.options_hover = None;
        let previous_bar = selection.panel_bar.take();
        selection.panel_bar = selection.toolbar.zip(selection.record).and_then(|(toolbar, options)| {
            let area = toolbar.area();
            crate::panel_options::PanelBar::new([options.show_keys, options.show_pad], toolbar.bounds, area, selection.rect)
        });
        selection.panel_bar_hover = None;
        let tip = selection
            .toolbar
            .zip(selection.toolbar_hover)
            .and_then(|(toolbar, action)| toolbar.tooltip(action))
            .map(|(bounds, _)| bounds);
        Some([
            previous.map(|options| options.bounds),
            selection.options.map(|options| options.bounds),
            tip,
            previous_bar.map(|bar| bar.bounds),
            selection.panel_bar.map(|bar| bar.bounds),
        ])
    });
    if let Some(changed) = changed {
        for bounds in changed.into_iter().flatten() {
            invalidate(window, &bounds, 0);
        }
    }
}

pub(super) unsafe fn update_options_hover(window: HWND, point: Option<POINT>) {
    let bar = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let bar = selection.panel_bar?;
        let hovered = point.and_then(|point| bar.hit(point));
        if selection.panel_bar_hover == hovered {
            return None;
        }
        selection.panel_bar_hover = hovered;
        Some(bar.bounds)
    });
    if let Some(bounds) = bar {
        invalidate(window, &bounds, 0);
    }
    let bounds = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let options = selection.options?;
        let hovered = point.and_then(|point| options.hit(point));
        if selection.options_hover == hovered {
            return None;
        }
        selection.options_hover = hovered;
        Some(options.bounds)
    });
    if let Some(bounds) = bounds {
        invalidate(window, &bounds, 0);
    }
}

/// A click in the recording picker's panel bar: a place or the opacity of a panel.
pub(super) unsafe fn change_panel(window: HWND, choice: crate::panel_options::PanelChoice) {
    use crate::panel_options::{PanelChoice, STEP};
    match choice {
        PanelChoice::Anchor(panel, anchor) => crate::input_overlay::pick_anchor(panel, crate::input_overlay::ANCHORS[anchor]),
        PanelChoice::Opacity(panel, step) => crate::input_overlay::step_opacity(panel, step * STEP),
    }
    if let Some(bounds) = SESSION.with_borrow(|session| Some(session.as_ref()?.panel_bar?.bounds)) {
        invalidate(window, &bounds, 0);
    }
}

pub(super) unsafe fn change_option(window: HWND, choice: Choice) {
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let tool = selection.annotations.tool?;
        if selection.annotations.draft.is_some() || selection.text_drag.is_some() || selection.mark_drag.is_some() {
            return None;
        }
        let mut style = selection.option_style();
        choice.adjust(tool, &mut style);
        let mut dirty = [None, None];
        let mut editor_font = None;
        let mut realign = false;
        if let Some(index) = selection.selected_mark {
            // Restyle the picked shape; new shapes keep using the same style.
            let mark = selection.annotations.mark(index)?;
            let restyled = mark.with_style(&style);
            dirty = [Some(mark.bounds()), Some(restyled.bounds())];
            selection.annotations.replace(index, restyled);
            selection.style = style;
        } else if tool != Tool::Text {
            selection.style = style;
        } else if let Some(editor) = &selection.editor {
            let mut text = editor.text.clone();
            text.size = style.text_size;
            text.color = style.text_color;
            text.look = style.text_look;
            realign = text.align != style.text_align;
            text.align = style.text_align;
            text.valign = style.text_valign;
            let rect = text_editor_bounds(selection, &text);
            let resized = (EqualRect(&editor.rect, &rect) == 0).then_some(rect);
            let editor = selection.editor.as_mut().unwrap();
            editor.text = text;
            if let Some(rect) = resized {
                dirty[0] = Some(editor.frame().bounds());
                editor.rect = rect;
                dirty[1] = Some(editor.frame().bounds());
            }
            editor_font = Some((
                editor.window,
                typography::annotation_font(style.text_size),
                resized,
            ));
        } else if let Some(index) = selection.selected_text {
            if let Some(mut text) = selection.annotations.text(index).cloned() {
                dirty[0] = Some(text.bounds());
                text.size = style.text_size;
                text.color = style.text_color;
                text.look = style.text_look;
                text.align = style.text_align;
                text.valign = style.text_valign;
                text.fit_text(selection.desktop.surface.dc);
                dirty[1] = Some(text.bounds());
                selection.annotations.replace_text(index, text);
            }
        } else {
            selection.style = style;
        }
        Some((dirty, editor_font, selection.options, realign))
    });
    if let Some((dirty, editor_font, options, realign)) = changed {
        if realign {
            realign_editor(window);
        }
        for rect in dirty.into_iter().flatten() {
            invalidate(window, &rect, 0);
        }
        if let Some((editor, font, resized)) = editor_font {
            SendMessageW(editor, WM_SETFONT, font as usize, 0);
            if let Some(rect) = resized {
                position_editor(editor, rect);
            }
            invalidate(editor, null(), 1);
            SetFocus(editor);
            invalidate_text_input(window, editor);
        }
        if let Some(options) = options {
            invalidate(window, &options.bounds, 0);
        }
        refresh_options(window);
        invalidate_toolbar(window);
        update_cursor_at_pointer(window);
    }
}

pub(super) unsafe fn update_toolbar_hover(window: HWND, point: Option<POINT>) {
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let toolbar = selection.toolbar?;
        let hovered = point.and_then(|point| toolbar.hit(point));
        if hovered == selection.toolbar_hover {
            return None;
        }
        let previous = selection.toolbar_hover;
        let entered = previous.is_none() && hovered.is_some();
        selection.toolbar_hover = hovered;
        let tip_bounds = |action: Option<Action>| {
            action.and_then(|action| toolbar.tooltip(action).map(|(bounds, _)| bounds))
        };
        Some((
            [
                Some(toolbar.bounds),
                tip_bounds(previous),
                tip_bounds(hovered),
            ],
            entered,
        ))
    });
    if let Some((bounds, entered)) = changed {
        for rect in bounds.into_iter().flatten() {
            invalidate(window, &rect, 0);
        }
        if entered {
            let mut tracking = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: window,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut tracking);
        }
    }
}

pub(super) unsafe fn refresh_hint(window: HWND) {
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let mode = if selection.record.is_some() && !selection.automatic {
            HintMode::Record
        } else if selection.editor.is_some() {
            HintMode::EditText
        } else if selection.selected_text().is_some() {
            HintMode::SelectedText
        } else if selection.selected_mark.is_some() {
            HintMode::SelectedMark
        } else if selection.automatic {
            HintMode::Automatic
        } else if let Some(tool) = selection.annotations.tool {
            HintMode::Annotate(tool)
        } else {
            HintMode::Selected
        };
        selection.hint.set_mode(selection.desktop.surface.dc, mode)
    });
    if let Some(bounds) = changed {
        for rect in bounds {
            invalidate(window, &rect, 0);
        }
    }
}

pub(super) unsafe fn invalidate_toolbar(window: HWND) {
    if let Some(toolbar) = SESSION.with_borrow(|session| session.as_ref()?.toolbar) {
        invalidate(window, &toolbar.bounds, 0);
    }
}

pub(super) unsafe fn command(window: HWND, action: Action) {
    if SESSION.with_borrow(|session| session.as_ref().is_some_and(|selection| selection.record.is_some())) {
        record_command(window, action);
        return;
    }
    finish_text(window, true);
    if action == Action::ScrollCapture {
        start_scroll_capture(window);
        return;
    }
    if matches!(action, Action::Copy | Action::Save | Action::Pin | Action::ExtractText | Action::Translate) {
        output(window, action);
        return;
    }
    if action == Action::Cancel {
        DestroyWindow(window);
        return;
    }
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let toolbar = selection.toolbar?;
        if selection.annotations.draft.is_some()
            || selection.text_drag.is_some()
            || !action.enabled(&selection.annotations)
        {
            return None;
        }
        if selection.mark_drag.is_some() || selection.pick_drag.is_some() {
            return None;
        }
        // Undo and tool changes renumber or leave the picked marks.
        let picks = pick_bounds(selection);
        selection.picked.clear();
        let previous = selection
            .selected_text()
            .map(TextMark::bounds)
            .or_else(|| selection.take_selected_mark());
        let region = selection.rect;
        match action {
            Action::Tool(tool) => {
                selection.annotations.tool =
                    (selection.annotations.tool != Some(tool)).then_some(tool);
                selection.selected_text = None;
            }
            Action::Undo | Action::Redo => {
                let region = &mut selection.rect;
                let text = if action == Action::Undo {
                    selection.annotations.undo(region)
                } else {
                    selection.annotations.redo(region)
                };
                selection.selected_text =
                    text.filter(|_| selection.annotations.tool == Some(Tool::Text));
            }
            _ => unreachable!(),
        }
        if selection.selected_text().is_none() {
            selection.selected_text = None;
        }
        let picks_union = picks.into_iter().reduce(|a, b| RECT {
            left: a.left.min(b.left),
            top: a.top.min(b.top),
            right: a.right.max(b.right),
            bottom: a.bottom.max(b.bottom),
        });
        let previous = match (previous, picks_union) {
            (Some(a), Some(b)) => Some(RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) }),
            (a, b) => a.or(b),
        };
        Some((
            selection.rect,
            toolbar.bounds,
            previous,
            selection.selected_text().map(TextMark::bounds),
            region,
        ))
    });
    if let Some((rect, toolbar, previous, current, region)) = changed {
        if EqualRect(&rect, &region) == 0 {
            // Undoing a selection move: repaint both outlines and move the toolbar.
            let canvas = SESSION.with_borrow(|session| session.as_ref().map(|s| s.canvas));
            if let Some(canvas) = canvas {
                invalidate_change(window, region, rect, canvas);
            }
            invalidate(window, &region, 0);
            refresh_toolbar(window);
        }
        invalidate(window, &rect, 0);
        invalidate(window, &toolbar, 0);
        for bounds in [previous, current].into_iter().flatten() {
            invalidate(window, &bounds, 0);
        }
        refresh_options(window);
        refresh_hint(window);
        if SESSION.with_borrow(|session| {
            session
                .as_ref()
                .is_some_and(|selection| selection.annotations.tool.is_some())
        }) {
            hide_loupe(window);
        }
        update_cursor_at_pointer(window);
    }
}

/// The recording area picker's toolbar and keys: start, the two sound
/// switches, cancel. Enter and double-click start recording.
unsafe fn record_command(window: HWND, action: Action) {
    match action {
        Action::Record | Action::Copy => {
            let picked = SESSION.with_borrow(|session| {
                let selection = session.as_ref()?;
                if selection.automatic || outline_empty(selection.rect) || selection.drag.is_some() {
                    return None;
                }
                let desktop = selection.desktop.bounds;
                let rect = selection.rect;
                let screen = RECT {
                    left: rect.left + desktop.left,
                    top: rect.top + desktop.top,
                    right: rect.right + desktop.left,
                    bottom: rect.bottom + desktop.top,
                };
                Some((screen, selection.record?))
            });
            if let Some((screen, options)) = picked {
                // The recording takes over first (it is busy from here on, so the
                // overlay closing keeps the panels); the overlay is left out of
                // capture, and gone long before the first frame.
                crate::recording_ui::start(null_mut(), screen, options, crate::recording_ui::Kind::Area);
                DestroyWindow(window);
            }
        }
        Action::SystemAudio | Action::Microphone | Action::KeysPanel | Action::PadPanel => {
            let toolbar = SESSION.with_borrow_mut(|session| {
                let selection = session.as_mut()?;
                let options = selection.record.as_mut()?;
                match action {
                    Action::SystemAudio => options.system_audio = !options.system_audio,
                    Action::Microphone => options.microphone = !options.microphone,
                    Action::KeysPanel => options.show_keys = !options.show_keys,
                    _ => options.show_pad = !options.show_pad,
                }
                let _ = crate::recording_ui::save_options(*options);
                selection.toolbar.map(|toolbar| toolbar.bounds)
            });
            if let Some(bounds) = toolbar {
                invalidate(window, &bounds, 0);
            }
            // The panel bar under the toolbar gains or loses a row.
            refresh_options(window);
            sync_record_panels();
        }
        Action::Cancel => {
            DestroyWindow(window);
        }
        _ => {}
    }
}

/// The panels switched on in the recording toolbar sit inside the locked
/// area and follow it as it moves; with no locked area they are not shown.
pub(super) unsafe fn sync_record_panels() {
    let Some(placement) = SESSION.with_borrow(|session| {
        let selection = session.as_ref()?;
        let options = selection.record?;
        if selection.automatic || outline_empty(selection.rect) || !(options.show_keys || options.show_pad) {
            return Some(None);
        }
        let desktop = selection.desktop.bounds;
        let rect = selection.rect;
        let screen = RECT {
            left: rect.left + desktop.left,
            top: rect.top + desktop.top,
            right: rect.right + desktop.left,
            bottom: rect.bottom + desktop.top,
        };
        Some(Some((screen, [options.show_keys, options.show_pad])))
    }) else {
        return;
    };
    match placement {
        Some((screen, shown)) => crate::input_overlay::show_in(screen, shown),
        None => crate::input_overlay::end_session(),
    }
}
