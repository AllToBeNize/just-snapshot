use super::super::*;
use super::clipboard::session;

#[test]
fn select_screen_locks_the_monitor_under_the_cursor() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        let window = session(instance);
        SESSION.with_borrow_mut(|session| {
            let selection = session.as_mut().unwrap();
            selection.automatic = true;
            selection.rect = RECT::default();
        });
        let mut cursor = POINT::default();
        GetCursorPos(&mut cursor);
        let expected = SESSION.with_borrow(|session| {
            let desktop = session.as_ref().unwrap().desktop.bounds;
            monitor_info(cursor).and_then(|info| overlay_rect(info.rcMonitor, desktop))
        });
        select_screen(window);
        let (rect, automatic, toolbar) = SESSION.with_borrow(|session| {
            let selection = session.as_ref().unwrap();
            (selection.rect, selection.automatic, selection.toolbar.is_some())
        });
        if let Some(expected) = expected {
            assert_eq!(
                (rect.left, rect.top, rect.right, rect.bottom),
                (expected.left, expected.top, expected.right, expected.bottom)
            );
            assert!(!automatic, "the screen selection is locked");
            assert!(toolbar, "the toolbar appears right away");
        }
        DestroyWindow(window);
    }
}

#[test]
fn moving_the_selection_can_be_undone_and_redone() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        let window = session(instance);
        let rect = || SESSION.with_borrow(|s| s.as_ref().unwrap().selection_rect());
        let original = rect();
        let mouse = |message, x: i32, y: i32| {
            SendMessageW(window, message, 0, ((y << 16) | x) as LPARAM);
        };
        // Drag from inside the locked selection: a move.
        mouse(WM_LBUTTONDOWN, 300, 150);
        mouse(WM_MOUSEMOVE, 320, 170);
        mouse(WM_LBUTTONUP, 320, 170);
        let moved = rect();
        assert_eq!((moved.0 - original.0, moved.1 - original.1), (20, 20));
        command(window, Action::Undo);
        assert_eq!(rect(), original, "undo restores the selection");
        command(window, Action::Redo);
        assert_eq!(rect(), moved, "redo moves it again");
        DestroyWindow(window);
    }
}
