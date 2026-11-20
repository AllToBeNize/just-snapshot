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

pub(super) unsafe fn output(window: HWND, action: Action) {
    let image = SESSION.with_borrow(|session| {
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
    });
    let Some((image, screen)) = image else { return };
    let target = match action {
        Action::Save => output::Target::Save("Snapshot.png"),
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
