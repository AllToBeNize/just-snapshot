use super::super::*;
use super::clipboard::session;
use std::time::{Duration, Instant};

unsafe fn pump(time: Duration) {
    let until = Instant::now() + time;
    let mut message = MSG::default();
    while Instant::now() < until {
        while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_recording_picker_shows_the_live_screen_through_a_dim_mask() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        open_record(instance).unwrap();
        let window = FindWindowW(CLASS, null());
        assert!(!window.is_null());
        let rect = RECT { left: 100, top: 100, right: 300, bottom: 300 };
        SESSION.with_borrow_mut(|session| {
            let selection = session.as_mut().unwrap();
            assert!(selection.live.is_some(), "the picker is a see-through layered window");
            assert!(selection.pointer_layer.is_none(), "no captured pointer when recording");
            selection.automatic = false;
            selection.rect = rect;
        });
        // The first show of a test process may be ignored (it can start hidden).
        ShowWindow(window, SW_SHOW);
        if IsWindowVisible(window) == 0 {
            // A desktop-less session (a service): nothing can be shown, so there is nothing to check.
            println!("skipped: no visible desktop in this session");
            DestroyWindow(window);
            return;
        }
        invalidate(window, null(), 0);
        // Run messages for a moment, as the app's loop would (the element probe
        // also talks to this window and needs it answered).
        pump(Duration::from_millis(300));
        let (inside, outside) = SESSION.with_borrow(|session| {
            let selection = session.as_ref().unwrap();
            let (width, height) = (selection.canvas.right, selection.canvas.bottom);
            let pixels = selection.live.as_ref().unwrap().bytes(width, height);
            let at = |x: i32, y: i32| {
                let index = ((y * width + x) * 4) as usize;
                [pixels[index], pixels[index + 1], pixels[index + 2], pixels[index + 3]]
            };
            (at(200, 200), at(450, 150))
        });
        assert_eq!(inside, [0, 0, 0, 1], "inside the area: see-through, still takes the mouse");
        assert_eq!(outside, [0, 0, 0, 102], "outside: the dim mask over the live screen");
        DestroyWindow(window);
        pump(Duration::from_millis(50));
    }
}

#[test]
fn the_pointer_shortcut_draws_the_pointer_in_and_takes_it_out_again() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        let window = session(instance);
        let patch = RECT { left: 10, top: 10, right: 10 + PATCH, bottom: 10 + PATCH };
        let read = || {
            SESSION.with_borrow(|session| {
                let selection = session.as_ref().unwrap();
                let (width, height) = (selection.canvas.right, selection.canvas.bottom);
                let pixels = selection.desktop.surface.bytes(width, height);
                (patch.top..patch.bottom)
                    .flat_map(|y| pixels[((y * width + patch.left) * 4) as usize..((y * width + patch.right) * 4) as usize].to_vec())
                    .collect::<Vec<u8>>()
            })
        };
        // A plain picture under the pointer, so drawing it surely changes pixels.
        SESSION.with_borrow_mut(|session| {
            let selection = session.as_mut().unwrap();
            let (width, height) = (selection.canvas.right, selection.canvas.bottom);
            selection.desktop.surface.fill(width, height, 0x0080_8080);
            selection.pointer_layer = Some(PointerLayer {
                cursor: CopyIcon(LoadCursorW(null_mut(), IDC_ARROW)),
                at: POINT { x: patch.left, y: patch.top },
                under: None,
            });
        });
        let before = read();
        toggle_pointer(window);
        assert_ne!(read(), before, "the pointer is drawn into the picture");
        toggle_pointer(window);
        assert_eq!(read(), before, "and taken out without a trace");
        DestroyWindow(window);
    }
}
