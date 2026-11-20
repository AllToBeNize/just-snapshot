use super::super::*;
use super::clipboard::session;
use crate::annotation_options::Flag;

unsafe fn mouse(window: HWND, message: u32, x: i32, y: i32) {
    SendMessageW(window, message, 0, ((y << 16) | x) as LPARAM);
}

unsafe fn key(window: HWND, key: u16) {
    SendMessageW(window, WM_KEYDOWN, key as WPARAM, 0);
}

fn first_mark() -> Option<Mark> {
    SESSION.with_borrow(|session| session.as_ref()?.annotations.mark(0).cloned())
}

#[test]
fn a_drawn_box_can_be_picked_moved_restyled_and_deleted() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        let window = session(instance);
        command(window, Action::Tool(Tool::Rectangle));
        mouse(window, WM_LBUTTONDOWN, 100, 80);
        mouse(window, WM_MOUSEMOVE, 200, 150);
        mouse(window, WM_LBUTTONUP, 200, 150);
        assert!(matches!(first_mark(), Some(Mark::Rectangle { rect: RECT { left: 100, top: 80, .. }, .. })));
        // Leave the tool: a click on the outline still picks the box.
        command(window, Action::Tool(Tool::Rectangle));
        assert!(SESSION.with_borrow(|s| s.as_ref().unwrap().annotations.tool.is_none()));
        mouse(window, WM_LBUTTONDOWN, 100, 110);
        mouse(window, WM_MOUSEMOVE, 120, 130);
        mouse(window, WM_LBUTTONUP, 120, 130);
        let (selected, tool) = SESSION.with_borrow(|s| {
            let selection = s.as_ref().unwrap();
            (selection.selected_mark, selection.annotations.tool)
        });
        assert_eq!(selected, Some(0));
        assert_eq!(tool, Some(Tool::Rectangle), "the option bar shows the box's style");
        assert!(matches!(first_mark(), Some(Mark::Rectangle { rect: RECT { left: 120, top: 100, .. }, .. })));
        assert_eq!(SESSION.with_borrow(|s| s.as_ref().unwrap().selection_rect()), (60, 40, 560, 260), "the selection stays put");

        change_option(window, Choice::Toggle(Flag::Dashed));
        change_option(window, Choice::Primary(2));
        assert!(matches!(first_mark(), Some(Mark::Rectangle { dashed: true, width: 5, .. })));
        assert!(SESSION.with_borrow(|s| s.as_ref().unwrap().style.rect_dashed), "new boxes take the same style");

        key(window, VK_DELETE);
        assert!(first_mark().is_none());
        command(window, Action::Undo);
        assert!(matches!(first_mark(), Some(Mark::Rectangle { dashed: true, .. })), "undo brings it back");
        command(window, Action::Undo);
        command(window, Action::Undo);
        assert!(matches!(first_mark(), Some(Mark::Rectangle { dashed: false, width: 3, .. })));
        command(window, Action::Undo);
        assert!(matches!(first_mark(), Some(Mark::Rectangle { rect: RECT { left: 100, .. }, .. })), "and undoes the move");
        DestroyWindow(window);
    }
}

#[test]
fn clicking_inside_an_old_box_draws_a_new_one() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        let window = session(instance);
        command(window, Action::Tool(Tool::Rectangle));
        for (from, to) in [((100, 80), (300, 220)), ((150, 120), (200, 160))] {
            mouse(window, WM_LBUTTONDOWN, from.0, from.1);
            mouse(window, WM_MOUSEMOVE, to.0, to.1);
            mouse(window, WM_LBUTTONUP, to.0, to.1);
        }
        let count = SESSION.with_borrow(|s| {
            let annotations = &s.as_ref().unwrap().annotations;
            (0..).take_while(|&index| annotations.mark(index).is_some()).count()
        });
        assert_eq!(count, 2);
        // Esc lets go of a picked box before it closes the capture.
        mouse(window, WM_LBUTTONDOWN, 300, 150);
        mouse(window, WM_LBUTTONUP, 300, 150);
        assert_eq!(SESSION.with_borrow(|s| s.as_ref().unwrap().selected_mark), Some(0));
        key(window, VK_ESCAPE);
        assert_eq!(SESSION.with_borrow(|s| s.as_ref().map(|s| s.selected_mark)), Some(None));
        DestroyWindow(window);
    }
}

#[test]
fn sizes_parse_and_stay_on_the_canvas() {
    assert_eq!(parse_size("800 × 600"), Some((800, 600)));
    assert_eq!(parse_size("1280x720"), Some((1280, 720)));
    assert_eq!(parse_size(" 64*48 "), Some((64, 48)));
    assert_eq!(parse_size("800"), None);
    assert_eq!(parse_size("0x5"), None);
    let canvas = RECT { right: 640, bottom: 420, ..Default::default() };
    let rect = RECT { left: 500, top: 300, right: 560, bottom: 360 };
    let moved = sized(rect, 300, 200, canvas);
    assert_eq!((moved.left, moved.top, moved.right, moved.bottom), (340, 220, 640, 420));
    let huge = sized(rect, 5000, 5000, canvas);
    assert_eq!((huge.left, huge.top, huge.right, huge.bottom), (0, 0, 640, 420));
}

#[test]
fn typing_in_the_size_badge_resizes_the_selection() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        let window = session(instance);
        let badge = SESSION.with_borrow(|s| {
            let selection = s.as_ref().unwrap();
            size_badge(selection.rect, selection.desktop.bounds, selection.canvas).unwrap()
        });
        mouse(window, WM_LBUTTONDOWN, (badge.left + badge.right) / 2, (badge.top + badge.bottom) / 2);
        let input = SESSION.with_borrow(|s| s.as_ref().unwrap().size_input).expect("a size box");
        assert_eq!(String::from_utf16_lossy(&read_editor_text(input)), "500 × 220");
        SetWindowTextW(input, crate::wide("320x180").as_ptr());
        SendMessageW(window, SIZE_DONE, 1, input as LPARAM);
        assert_eq!(SESSION.with_borrow(|s| s.as_ref().unwrap().selection_rect()), (60, 40, 380, 220));
        command(window, Action::Undo);
        assert_eq!(SESSION.with_borrow(|s| s.as_ref().unwrap().selection_rect()), (60, 40, 560, 260));
        // Arrow keys move by a pixel; Ctrl grows from the bottom-right corner.
        key(window, VK_RIGHT);
        assert_eq!(SESSION.with_borrow(|s| s.as_ref().unwrap().selection_rect()), (61, 40, 561, 260));
        nudge(window, u32::from(VK_DOWN), MOD_CONTROL | MOD_SHIFT, false);
        assert_eq!(SESSION.with_borrow(|s| s.as_ref().unwrap().selection_rect()), (61, 40, 561, 270));
        DestroyWindow(window);
    }
}

#[test]
fn the_last_capture_comes_back_with_its_annotations() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        LAST.take();
        let window = session(instance);
        command(window, Action::Tool(Tool::Number));
        mouse(window, WM_LBUTTONDOWN, 200, 100);
        mouse(window, WM_LBUTTONUP, 200, 100);
        DestroyWindow(window);
        assert!(has_last());
        let last = LAST.take().unwrap().unpack().unwrap();
        open_selection(instance, Start::Last(last)).unwrap();
        let (rect, automatic, marked, window) = SESSION.with_borrow(|s| {
            let selection = s.as_ref().unwrap();
            (selection.selection_rect(), selection.automatic, selection.annotations.mark(0).is_some(), selection.toolbar.is_some())
        });
        assert_eq!(rect, (60, 40, 560, 260));
        assert!(!automatic && marked && window, "locked, annotated, with the toolbar");
        assert!(SESSION.with_borrow(|s| s.as_ref().unwrap().annotations.can_undo()), "undo history comes back too");
        // The screen comes back pixel for pixel after the PNG round trip.
        let restored = SESSION.with_borrow(|s| GetPixel(s.as_ref().unwrap().desktop.surface.dc, 5, 5));
        assert_eq!(restored, 0x0040_8020);
        let overlay = FindWindowW(CLASS, null());
        DestroyWindow(overlay);
        assert!(has_last(), "closing it again keeps it");
    }
}

#[test]
fn the_select_tool_boxes_marks_and_moves_them_together() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        let window = session(instance);
        // Two numbers and a box.
        command(window, Action::Tool(Tool::Number));
        for x in [120, 200] {
            mouse(window, WM_LBUTTONDOWN, x, 100);
            mouse(window, WM_LBUTTONUP, x, 100);
        }
        command(window, Action::Tool(Tool::Rectangle));
        mouse(window, WM_LBUTTONDOWN, 300, 150);
        mouse(window, WM_MOUSEMOVE, 400, 220);
        mouse(window, WM_LBUTTONUP, 400, 220);
        command(window, Action::Tool(Tool::Select));
        let center = |index: usize| {
            SESSION.with_borrow(|s| match s.as_ref().unwrap().annotations.mark(index) {
                Some(Mark::Number { center, .. }) => (center.x, center.y),
                Some(Mark::Rectangle { rect, .. }) => (rect.left, rect.top),
                _ => panic!("mark {index}"),
            })
        };
        let picked = || SESSION.with_borrow(|s| {
            let mut picked = s.as_ref().unwrap().picked.clone();
            picked.sort();
            picked
        });
        // A box around the two numbers picks just them.
        mouse(window, WM_LBUTTONDOWN, 90, 70);
        mouse(window, WM_MOUSEMOVE, 230, 130);
        mouse(window, WM_LBUTTONUP, 230, 130);
        assert_eq!(picked(), vec![0, 1]);
        // Ctrl+click adds the box.
        SendMessageW(window, WM_LBUTTONDOWN, 0x0008, ((150 << 16) | 350) as LPARAM);
        mouse(window, WM_LBUTTONUP, 350, 150);
        assert_eq!(picked(), vec![0, 1, 2]);
        // Dragging one moves all three.
        mouse(window, WM_LBUTTONDOWN, 120, 100);
        mouse(window, WM_MOUSEMOVE, 130, 120);
        mouse(window, WM_LBUTTONUP, 130, 120);
        assert_eq!((center(0), center(1), center(2)), ((130, 120), (210, 120), (310, 170)));
        key(window, VK_RIGHT);
        assert_eq!(center(1), (211, 120), "arrow keys move them together");
        // One undo per step.
        command(window, Action::Undo);
        command(window, Action::Undo);
        assert_eq!((center(0), center(2)), ((120, 100), (300, 150)));
        assert!(picked().is_empty(), "undo drops the picks");
        // Pick all and delete in one step.
        mouse(window, WM_LBUTTONDOWN, 80, 60);
        mouse(window, WM_MOUSEMOVE, 450, 250);
        mouse(window, WM_LBUTTONUP, 450, 250);
        assert_eq!(picked().len(), 3);
        key(window, VK_DELETE);
        assert!(first_mark().is_none());
        command(window, Action::Undo);
        assert_eq!((center(0), center(1), center(2)), ((120, 100), (200, 100), (300, 150)));
        DestroyWindow(window);
    }
}
