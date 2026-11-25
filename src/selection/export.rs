//! Leaving the overlay: long capture or delivering the image.

use super::*;

pub(super) unsafe fn start_scroll_capture(window: HWND) {
    let selected = SESSION.with_borrow(|session| {
        let selection = session.as_ref()?;
        let rect = selection.rect;
        if selection.automatic
            || selection.drag.is_some()
            || selection.text_drag.is_some()
            || selection.annotations.draft.is_some()
            || outline_empty(rect)
        {
            return None;
        }
        let desktop = selection.desktop.bounds;
        let screen = RECT {
            left: rect.left + desktop.left,
            top: rect.top + desktop.top,
            right: rect.right + desktop.left,
            bottom: rect.bottom + desktop.top,
        };
        Some(if selection.annotations.has_marks() {
            Err("请先撤回选区里的标注，再开始长截图。".into())
        } else {
            selection
                .desktop
                .crop(rect)
                .map(|image| {
                    let center = POINT {
                        x: screen.left + (screen.right - screen.left) / 2,
                        y: screen.top + (screen.bottom - screen.top) / 2,
                    };
                    let target = selection
                        .targets
                        .iter()
                        .find(|target| contains(target.rect, center));
                    (screen, image, target.map(|target| target.id as HWND))
                })
                .map_err(|error| error.to_string())
        })
    });
    let Some(selected) = selected else {
        return;
    };
    match selected
        .and_then(|(screen, image, target)| scroll_capture::start(screen, image).map(|()| target))
    {
        Ok(target) => {
            if let Some(target) = target {
                SetForegroundWindow(target);
            }
            DestroyWindow(window);
        }
        Err(error) => show_error(window, &error),
    }
}

/// The selected area with its annotations, and where it is on the screen.
/// `None` while something is being drawn or dragged.
unsafe fn composed() -> Option<(io::Result<Image>, RECT)> {
    SESSION.with_borrow(|session| {
        let selection = session.as_ref()?;
        let rect = selection.rect;
        if selection.drag.is_some()
            || selection.text_drag.is_some()
            || selection.annotations.draft.is_some()
            || outline_empty(rect)
        {
            return None;
        }
        let image = selection.desktop.composite_crop(rect, |dc| {
            selection.annotations.paint(dc, rect, false, None)
        });
        let desktop = selection.desktop.bounds;
        let screen = RECT {
            left: rect.left + desktop.left,
            top: rect.top + desktop.top,
            right: rect.right + desktop.left,
            bottom: rect.bottom + desktop.top,
        };
        Some((image, screen))
    })
}

pub(super) unsafe fn output(window: HWND, action: Action) {
    let Some((image, screen)) = composed() else { return };
    let target = match action {
        Action::Save => output::Target::Save,
        Action::Pin => output::Target::Pin(screen),
        Action::ExtractText => output::Target::TextPin(screen),
        Action::Translate => output::Target::TranslatePin(screen),
        _ => output::Target::Copy,
    };
    let result = image
        .map_err(|error| format!("无法生成截图。\n{error}"))
        .and_then(|image| output::deliver(window, &image, target));
    match result {
        Ok(true) => {
            DestroyWindow(window);
        }
        Ok(false) => {}
        Err(error) => show_error(window, &error),
    }
}

/// Asks which engine to use under the 识图 button, then uploads the picture
/// in the background; the toolbar says so meanwhile.
pub(super) unsafe fn search_image(window: HWND) {
    let anchor = SESSION.with_borrow(|session| {
        let selection = session.as_ref()?;
        if selection.search.is_some() {
            return None;
        }
        Some(selection.toolbar.and_then(|toolbar| toolbar.button_bounds(Action::SearchImage)))
    });
    let Some(anchor) = anchor else { return };
    let mut point = POINT::default();
    match anchor {
        Some(button) => {
            point = POINT { x: button.left, y: button.bottom + 6 };
            ClientToScreen(window, &mut point);
        }
        None => {
            GetCursorPos(&mut point);
        }
    }
    match crate::image_search::choose(window, point) {
        Err(error) => show_error(window, &error),
        Ok(None) => {}
        Ok(Some(crate::image_search::Pick::Settings)) => {
            if let Err(error) = crate::image_search_settings::open(window) {
                show_error(window, &format!("无法打开识图设置。\n{error}"));
            }
        }
        Ok(Some(crate::image_search::Pick::Engines(engines))) => {
            let Some((image, _)) = composed() else { return };
            match image {
                Ok(image) => {
                    set_search(window, Some(crate::image_search::busy_label(&engines)));
                    crate::image_search::start(window, SEARCH_DONE, engines, image);
                }
                Err(error) => show_error(window, &format!("无法生成截图。\n{error}")),
            }
        }
    }
}

/// Shows or clears the progress label over the toolbar.
pub(super) unsafe fn set_search(window: HWND, label: Option<String>) {
    let changed = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        let old = current_tip(selection).map(|(bounds, _)| bounds);
        selection.search = label;
        let new = current_tip(selection).map(|(bounds, _)| bounds);
        Some([old, new])
    });
    for bounds in changed.into_iter().flatten().flatten() {
        invalidate(window, &bounds, 0);
    }
}

/// Opens the result pages and leaves the capture; a failure is reported and
/// the capture stays for another try unless some engine succeeded.
pub(super) unsafe fn finish_search(window: HWND, outcome: crate::image_search::Outcome) {
    set_search(window, None);
    let (opened, failure) = outcome.open();
    if let Some(message) = failure {
        show_error(window, &message);
    }
    if opened > 0 {
        DestroyWindow(window);
    }
}
