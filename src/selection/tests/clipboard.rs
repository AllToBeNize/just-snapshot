use super::super::*;
use windows_sys::Win32::System::{
    DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW,
    },
    Memory::{GlobalLock, GlobalUnlock},
};

const CF_DIB: u32 = 8;

pub(super) unsafe fn session(instance: HINSTANCE) -> HWND {
    let canvas = RECT {
        right: 640,
        bottom: 420,
        ..Default::default()
    };
    let surface = || Surface::new(null_mut(), 640, 420).unwrap();
    let desktop = Desktop {
        bounds: canvas,
        surface: surface(),
    };
    SetDCBrushColor(desktop.surface.dc, 0x0040_8020);
    FillRect(desktop.surface.dc, &canvas, GetStockObject(DC_BRUSH));
    let hint = HintPanel::new(desktop.surface.dc, canvas).unwrap();
    let window = CreateWindowExW(
        0,
        CLASS,
        null(),
        WS_POPUP | WS_CLIPCHILDREN,
        0,
        0,
        640,
        420,
        null_mut(),
        null_mut(),
        instance,
        null(),
    );
    assert!(!window.is_null());
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
            left: 60,
            top: 40,
            right: 560,
            bottom: 260,
        },
        hint,
        loupe: None,
        pointer_monitor: canvas,
        annotations: History::default(),
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
        record: None,
        live: None,
        pointer_layer: None,
        editor: None,
        brush_cursor: None,
        mosaic_cursor: None,
    }));
    refresh_toolbar(window);
    window
}

unsafe fn clear_clipboard() {
    assert_ne!(OpenClipboard(null_mut()), 0);
    EmptyClipboard();
    CloseClipboard();
}

/// An opaque, bottom-up DIB plus a PNG copy.
unsafe fn has_image() -> bool {
    let png = RegisterClipboardFormatW(w!("PNG"));
    if IsClipboardFormatAvailable(CF_DIB) == 0 || IsClipboardFormatAvailable(png) == 0 {
        return false;
    }
    if OpenClipboard(null_mut()) == 0 {
        return false;
    }
    let memory = GetClipboardData(CF_DIB);
    let data = GlobalLock(memory).cast::<u8>();
    let opaque = !data.is_null() && {
        let header = data.cast::<BITMAPINFOHEADER>().read_unaligned();
        let first_alpha = *data.add(size_of::<BITMAPINFOHEADER>() + 3);
        header.biHeight > 0 && first_alpha == 255
    };
    if !data.is_null() {
        GlobalUnlock(memory);
    }
    CloseClipboard();
    opaque
}

/// Writes to the real clipboard, so it only runs on demand in a desktop
/// session: `<test binary> clipboard --ignored --test-threads=1`.
#[test]
#[ignore]
fn every_copy_trigger_puts_an_image_on_the_clipboard() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        let lparam = |x: i32, y: i32| ((y << 16) | x) as LPARAM;
        let mut results = Vec::new();

        clear_clipboard();
        let window = session(instance);
        command(window, Action::Copy);
        results.push(("command(Copy)", has_image()));

        clear_clipboard();
        let window = session(instance);
        SendMessageW(window, WM_KEYDOWN, VK_RETURN as usize, 0);
        results.push(("Enter", has_image()));

        clear_clipboard();
        let window = session(instance);
        let toolbar = SESSION.with_borrow(|s| s.as_ref().unwrap().toolbar.unwrap());
        let (tip, _) = toolbar.tooltip(Action::Copy).unwrap();
        let x = (tip.left + tip.right) / 2;
        let y = (toolbar.bounds.top + toolbar.bounds.bottom) / 2;
        assert!(toolbar.hit(POINT { x, y }) == Some(Action::Copy));
        SendMessageW(window, WM_MOUSEMOVE, 0, lparam(x, y));
        SendMessageW(window, WM_LBUTTONDOWN, 0, lparam(x, y));
        SendMessageW(window, WM_LBUTTONUP, 0, lparam(x, y));
        results.push(("toolbar button", has_image()));

        clear_clipboard();
        let window = session(instance);
        SendMessageW(window, WM_LBUTTONDOWN, 0, lparam(300, 150));
        SendMessageW(window, WM_LBUTTONUP, 0, lparam(300, 150));
        SendMessageW(window, WM_LBUTTONDBLCLK, 0, lparam(300, 150));
        SendMessageW(window, WM_LBUTTONUP, 0, lparam(300, 150));
        results.push(("double click", has_image()));

        println!("clipboard results: {results:?}");
        assert!(results.iter().all(|(_, ok)| *ok), "{results:?}");
    }
}
