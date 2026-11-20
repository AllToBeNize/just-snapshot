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

#[test]
fn side_buttons_step_through_layers_until_the_mouse_moves() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        let window = session(instance);
        let rect = |left, top, right, bottom| RECT { left, top, right, bottom };
        let edges = || SESSION.with_borrow(|s| s.as_ref().unwrap().selection_rect());
        SESSION.with_borrow_mut(|session| {
            let selection = session.as_mut().unwrap();
            selection.automatic = true;
            selection.rect = rect(100, 100, 200, 200);
            selection.element_layers = vec![rect(100, 100, 200, 200), rect(101, 99, 201, 200), rect(50, 50, 300, 300)];
            selection.targets = vec![targets::WindowTarget { id: 1, rect: rect(20, 20, 400, 400) }];
            selection.target = Some(1);
            selection.pointer = Some(POINT { x: 150, y: 150 });
        });
        let point = POINT { x: 150, y: 150 };
        assert!(step_layer_at(window, point, true));
        assert_eq!(edges(), (50, 50, 300, 300), "near duplicates count once");
        step_layer_at(window, point, true);
        assert_eq!(edges(), (20, 20, 400, 400), "then the window");
        step_layer_at(window, point, true);
        step_layer_at(window, point, true);
        let outermost = edges();
        assert!(outermost.2 - outermost.0 >= 380, "the screen is the last layer: {outermost:?}");
        step_layer_at(window, point, false);
        assert_eq!(edges(), (20, 20, 400, 400));
        // A tiny drift keeps the choice; a real move goes back to snapping.
        hover(window, POINT { x: 152, y: 151 });
        assert_eq!(edges(), (20, 20, 400, 400));
        assert!(SESSION.with_borrow(|s| s.as_ref().unwrap().layer.is_some()));
        hover(window, POINT { x: 170, y: 170 });
        assert!(SESSION.with_borrow(|s| s.as_ref().unwrap().layer.is_none()));
        // A locked selection ignores the side buttons.
        SESSION.with_borrow_mut(|s| s.as_mut().unwrap().automatic = false);
        assert!(!step_layer_at(window, point, true));
        DestroyWindow(window);
    }
}

/// Paint cost of dimming on the fly versus copying a dimmed copy, at the
/// size of a 2560 + 1920 px desktop. Run with `<test binary> dim_cost --ignored --nocapture`.
#[test]
#[ignore]
fn dim_cost() {
    unsafe {
        let (width, height) = (4480, 1440);
        let source = Surface::new(null_mut(), width, height).unwrap();
        let dimmed = Surface::new(null_mut(), width, height).unwrap();
        let target = Surface::new(null_mut(), width, height).unwrap();
        let row = Surface::new(null_mut(), width, 1).unwrap();
        let canvas = RECT { right: width, bottom: height, ..Default::default() };
        FillRect(row.dc, &RECT { right: width, bottom: 1, ..Default::default() }, GetStockObject(BLACK_BRUSH));
        let blend = BLENDFUNCTION { SourceConstantAlpha: 153, ..Default::default() };
        let time = |label: &str, paint: &dyn Fn()| {
            paint();
            let start = std::time::Instant::now();
            for _ in 0..10 {
                paint();
                GdiFlush();
            }
            println!("{label}: {:.1} ms per full-screen paint", start.elapsed().as_secs_f64() * 100.0);
        };
        let _ = canvas;
        time("copy dimmed", &|| {
            BitBlt(target.dc, 0, 0, width, height, dimmed.dc, 0, 0, SRCCOPY);
        });
        time("copy + blend black row", &|| {
            BitBlt(target.dc, 0, 0, width, height, source.dc, 0, 0, SRCCOPY);
            AlphaBlend(target.dc, 0, 0, width, height, row.dc, 0, 0, width, 1, blend);
        });
    }
}
