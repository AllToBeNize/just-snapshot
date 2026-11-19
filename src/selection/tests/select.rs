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
