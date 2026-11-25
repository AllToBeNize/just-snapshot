use super::super::*;

#[test]
fn new_text_can_be_resized_before_typing_and_reopened_after_moving() {
    unsafe {
        let instance = GetModuleHandleW(null());
        // Tests share the process-wide window class; another test may have registered it.
        let _ = register(instance);
        let canvas = RECT {
            right: 480,
            bottom: 320,
            ..Default::default()
        };
        let surface = || Surface::new(null_mut(), 480, 320).unwrap();
        let desktop = Desktop {
            bounds: canvas,
            surface: surface(),
        };
        FillRect(desktop.surface.dc, &canvas, GetStockObject(BLACK_BRUSH));
        let hint = HintPanel::new(desktop.surface.dc, canvas).unwrap();
        let window = CreateWindowExW(
            0,
            CLASS,
            null(),
            WS_POPUP | WS_CLIPCHILDREN,
            0,
            0,
            480,
            320,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        assert!(!window.is_null());
        let mut annotations = History::default();
        annotations.tool = Some(Tool::Text);
        SESSION.replace(Some(Selection {
            desktop,
            dimmed: surface(),
            back_buffer: surface(),
            canvas,
            targets: Vec::new(),
            probe: None,
            target: None,
            pointer: None,
            visual_probe: None,
            automatic: false,
            drag: None,
            rect: RECT {
                left: 15,
                top: 30,
                right: 460,
                bottom: 260,
            },
            hint,
            loupe: None,
            pointer_monitor: canvas,
            annotations,
            toolbar: None,
            toolbar_hover: None,
            options: None,
            options_hover: None,
        panel_bar: None,
        panel_bar_hover: None,
            style: Style::default(),
            selected_text: None,
            text_drag: None,
        selected_mark: None,
        mark_drag: None,
        size_input: None,
        element_layers: Vec::new(),
        layer: None,
        picked: Vec::new(),
        pick_drag: None,
        search: None,
        record: None,
        live: None,
        pointer_layer: None,
            editor: None,
            brush_cursor: None,
            mosaic_cursor: None,
        }));
        refresh_toolbar(window);
        let mouse = |message, x: i32, y: i32| {
            SendMessageW(window, message, 0, ((y << 16) | x) as LPARAM);
        };
        mouse(WM_LBUTTONDOWN, 80, 110);
        mouse(WM_LBUTTONUP, 80, 110);
        let (editor, initial_size) = SESSION.with_borrow(|s| {
            let editor = s.as_ref().unwrap().editor.as_ref().unwrap();
            (editor.window, editor.text.size)
        });
        let assert_input_off_canvas = |editor| {
            let mut overlay = RECT::default();
            let mut input = RECT::default();
            assert_ne!(GetWindowRect(window, &mut overlay), 0);
            assert_ne!(GetWindowRect(editor, &mut input), 0);
            assert!(
                intersect(overlay, input).is_none(),
                "native EDIT must stay outside the drawable overlay"
            );
        };
        assert_input_off_canvas(editor);
        let region = CreateRectRgn(0, 0, 0, 0);
        let kind = GetWindowRgn(editor, region);
        DeleteObject(region);
        assert_eq!(
            kind, NULLREGION,
            "the horizontal editor must have no visible native surface"
        );
        refresh_text_preview();
        SESSION.with_borrow(|session| {
            let editor = session.as_ref().unwrap().editor.as_ref().unwrap();
            let blank = editor.preview.as_ref().unwrap();
            assert!(blank.pixels.chunks_exact(4).all(|pixel| pixel[3] == 0));
            let composite = surface();
            FillRect(composite.dc, &canvas, GetStockObject(BLACK_BRUSH));
            editor.frame().paint_input(composite.dc, blank, None);
            for x in [82, 90, 165, 250, 257] {
                assert_eq!(
                    GetPixel(composite.dc, x, 145),
                    0,
                    "empty editor leaves no white strips"
                );
            }
        });
        let (frame, input, options, toolbar) = SESSION.with_borrow(|s| {
            let selection = s.as_ref().unwrap();
            let editor = selection.editor.as_ref().unwrap();
            (
                editor.frame().into_owned(),
                editor_input_rect(editor.rect),
                selection.options.unwrap().bounds,
                selection.toolbar.unwrap().bounds,
            )
        });
        assert!(
            options.left >= toolbar.left && options.right <= toolbar.right,
            "text settings hang centered under the toolbar: {:?} in {:?}",
            (options.left, options.right),
            (toolbar.left, toolbar.right)
        );
        assert!(options.top >= toolbar.bottom || options.bottom <= toolbar.top);
        let composed = surface();
        FillRect(composed.dc, &canvas, GetStockObject(WHITE_BRUSH));
        frame.paint_frame(composed.dc);
        let saved = SaveDC(composed.dc);
        SetViewportOrgEx(composed.dc, input.left, input.top, null_mut());
        SendMessageW(
            editor,
            WM_PRINTCLIENT,
            composed.dc as WPARAM,
            (PRF_CLIENT | PRF_ERASEBKGND) as LPARAM,
        );
        RestoreDC(composed.dc, saved);
        // Each edge is a thin dashed line: blue dashes with gaps between them.
        for (edge, horizontal, band, range) in [
            ("top", true, 109..=110, 100..240),
            ("left", false, 79..=80, 120..170),
            ("bottom", true, 177..=178, 100..240),
            ("right", false, 259..=260, 120..170),
        ] {
            let (mut dashes, mut gaps) = (0, 0);
            for along in range {
                let blue = band.clone().any(|across| {
                    let (x, y) = if horizontal { (along, across) } else { (across, along) };
                    GetPixel(composed.dc, x, y) == BORDER_BLUE
                });
                if blue {
                    dashes += 1;
                } else {
                    gaps += 1;
                }
            }
            assert!(dashes > 10 && gaps > 5, "{edge}: {dashes} dash, {gaps} gap pixels");
        }
        let mut corner = POINT { x: 80, y: 110 };
        ClientToScreen(window, &mut corner);
        SendMessageW(
            editor,
            WM_NCLBUTTONDOWN,
            HTBORDER as usize,
            ((corner.y << 16) | (corner.x & 0xffff)) as LPARAM,
        );
        mouse(WM_MOUSEMOVE, 56, 93);
        mouse(WM_LBUTTONUP, 56, 93);
        let editor = SESSION.with_borrow(|s| {
            let editor = s
                .as_ref()
                .unwrap()
                .editor
                .as_ref()
                .expect("blank editor stays open while resizing");
            assert!(editor.text.size > initial_size);
            let moved = s.as_ref().unwrap().options.unwrap().bounds;
            assert_ne!(EqualRect(&moved, &options), 0, "settings stay with the toolbar");
            editor.window
        });
        SetWindowTextW(editor, w!("Snapshot"));
        refresh_text_preview();
        let native = input_caret_info(editor).expect("the editor retains the insertion point");
        assert_eq!(
            native.flags & GUI_CARETBLINKING,
            0,
            "only the overlay may draw the caret"
        );
        assert!(SESSION.with_borrow(|session| {
            let editor = session.as_ref().unwrap().editor.as_ref().unwrap();
            editor
                .preview
                .as_ref()
                .unwrap()
                .pixels
                .chunks_exact(4)
                .any(|pixel| pixel[3] > 128)
        }));
        SendMessageW(window, FINISH_TEXT, 1, editor as LPARAM);
        mouse(WM_LBUTTONDOWN, 148, 143);
        mouse(WM_MOUSEMOVE, 181, 160);
        mouse(WM_LBUTTONUP, 181, 160);
        mouse(WM_LBUTTONDOWN, 181, 160);
        mouse(WM_LBUTTONUP, 181, 160);
        mouse(WM_LBUTTONDBLCLK, 181, 160);
        assert!(SESSION.with_borrow(|s| s.as_ref().unwrap().editor.is_some()));
        assert_eq!(GetCursor(), LoadCursorW(null_mut(), IDC_IBEAM));
        finish_text(window, false);
        mouse(WM_LBUTTONDOWN, 30, 50);
        mouse(WM_LBUTTONUP, 30, 50);
        let frame_bounds =
            || SESSION.with_borrow(|s| s.as_ref().unwrap().text_frame().unwrap().bounds());
        let before_rotation = frame_bounds();
        mouse(WM_LBUTTONDOWN, 120, 36);
        assert!(SESSION.with_borrow(|s| s.as_ref().unwrap().options.is_none()));
        mouse(WM_MOUSEMOVE, 170, 55);
        let preview = frame_bounds();
        let hidden = SESSION.with_borrow(|s| s.as_ref().unwrap().options.is_none());
        mouse(WM_LBUTTONUP, 170, 55);
        assert_eq!(EqualRect(&before_rotation, &preview), 0);
        assert_ne!(
            EqualRect(&preview, &frame_bounds()),
            0,
            "empty frame keeps the rotation preview on release"
        );
        assert!(hidden, "hide the text settings while rotating");
        assert!(SESSION.with_borrow(|s| s.as_ref().unwrap().options.is_some()));
        let editor =
            SESSION.with_borrow(|s| s.as_ref().unwrap().editor.as_ref().unwrap().window);
        let region = CreateRectRgn(0, 0, 0, 0);
        let kind = GetWindowRgn(editor, region);
        DeleteObject(region);
        assert_eq!(
            kind, NULLREGION,
            "the rotated editor must not draw a horizontal layer"
        );
        refresh_text_preview();
        assert!(
            SESSION.with_borrow(|session| {
                let editor = session.as_ref().unwrap().editor.as_ref().unwrap();
                editor
                    .preview
                    .as_ref()
                    .unwrap()
                    .pixels
                    .chunks_exact(4)
                    .all(|pixel| pixel[3] == 0)
            }),
            "rotating an empty editor cannot introduce a background"
        );
        SetWindowTextW(editor, w!("Rotated"));
        assert_input_off_canvas(editor);
        SendMessageW(editor, EM_SETSEL, 7, 7);
        let click = SESSION.with_borrow(|session| {
            let input = session.as_ref().unwrap().editor.as_ref().unwrap();
            input.frame().input_screen_point(POINT { x: 6, y: 12 })
        });
        mouse(WM_LBUTTONDOWN, click.x, click.y);
        let mut native_click = click;
        MapWindowPoints(window, editor, &mut native_click, 1);
        SendMessageW(editor, WM_LBUTTONUP, 0, point_param(native_click));
        let selected = SendMessageW(editor, windows_sys::Win32::UI::Controls::EM_GETSEL, 0, 0);
        assert!(
            (selected as u16) <= 1,
            "click near the start of rotated text moves the caret there"
        );
        SendMessageW(editor, EM_SETSEL, 3, 3);
        SendMessageW(editor, WM_CHAR, '测' as usize, 1);
        assert_eq!(
            String::from_utf16_lossy(&read_editor_text(editor)),
            "Rot测ated"
        );
        assert_input_off_canvas(editor);
        let native =
            input_caret_info(editor).expect("the native insertion point is still available");
        assert_eq!(native.flags & GUI_CARETBLINKING, 0);
        let caret = native.rcCaret;
        refresh_text_preview();
        SESSION.with_borrow(|session| {
            let input = session.as_ref().unwrap().editor.as_ref().unwrap();
            let preview = input.preview.as_ref().unwrap();
            assert!(
                preview.pixels.chunks_exact(4).any(|pixel| pixel[3] > 128),
                "native text is visible in the rotated preview"
            );
            assert_eq!(
                preview.pixels[3], 0,
                "native editor background stays transparent"
            );
            let render = |caret| {
                let surface = surface();
                FillRect(surface.dc, &canvas, GetStockObject(WHITE_BRUSH));
                input.text.paint_input(surface.dc, preview, caret);
                GdiFlush();
                Desktop {
                    bounds: canvas,
                    surface,
                }
                .crop(canvas)
                .unwrap()
            };
            let text_only = render(None);
            let with_caret = render(Some(caret));
            assert!(text_only
                .pixels
                .chunks_exact(4)
                .any(|pixel| pixel[..3] != [255; 3]));
            let mut affected = RECT {
                left: canvas.right,
                top: canvas.bottom,
                ..Default::default()
            };
            for (index, (text, active)) in text_only
                .pixels
                .chunks_exact(4)
                .zip(with_caret.pixels.chunks_exact(4))
                .enumerate()
            {
                if text[..3] != active[..3] {
                    let x = index as i32 % canvas.right;
                    let y = index as i32 / canvas.right;
                    affected.left = affected.left.min(x);
                    affected.right = affected.right.max(x);
                    affected.top = affected.top.min(y);
                    affected.bottom = affected.bottom.max(y);
                }
            }
            assert!(
                affected.right - affected.left > 2 && affected.bottom - affected.top > 2,
                "the caret should run diagonally"
            );
        });
        SendMessageW(window, FINISH_TEXT, 1, editor as LPARAM);
        assert!(SESSION.with_borrow(|s| s
            .as_ref()
            .unwrap()
            .selected_text()
            .unwrap()
            .is_rotated()));
        DestroyWindow(window);
    }
}
