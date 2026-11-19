//! Themed replacement for `MessageBox`: a small rounded card with an icon,
//! title, wrapped message, "copy" and "OK" buttons. Runs its own modal loop.

use std::{
    cell::RefCell,
    ptr::{null, null_mut},
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{
                EnableWindow, GetKeyState, IsWindowEnabled, ReleaseCapture, SetCapture, SetFocus,
                VK_CONTROL, VK_ESCAPE, VK_RETURN, VK_SPACE,
            },
            WindowsAndMessaging::*,
        },
    },
};

use crate::{
    overlay_ui::monitor_info,
    selection_geometry::{contains, mouse_point},
    gdiplus::Graphics,
    theme::{self, argb, mix},
    ui::{self, Badge, Fonts},
};

const CLASS: *const u16 = w!("Snapshot.Dialog");
const WIDTH: i32 = 420;
const PAD: i32 = 24;
const BADGE: i32 = 36;
const BUTTON_HEIGHT: i32 = 34;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Button {
    Copy,
    Ok,
}

struct State {
    kind: Kind,
    title: String,
    message: String,
    scale: f32,
    fonts: Fonts,
    hovered: Option<Button>,
    pressed: Option<Button>,
    copied: bool,
    closed: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct Layout {
    width: i32,
    height: i32,
    badge: RECT,
    title: RECT,
    message: RECT,
    copy: RECT,
    ok: RECT,
}

impl Layout {
    unsafe fn new(dc: HDC, state: &State) -> Self {
        let s = |value: i32| (value as f32 * state.scale).round() as i32;
        let width = s(WIDTH);
        let text_left = s(PAD + BADGE + 14);
        let text_width = width - text_left - s(PAD);
        let title_height = s(24);
        let message_height = if state.message.is_empty() {
            0
        } else {
            ui::wrapped_height(dc, &state.message, state.fonts.body, text_width).min(s(320))
        };
        let title = RECT {
            left: text_left,
            top: s(PAD),
            right: width - s(PAD),
            bottom: s(PAD) + title_height,
        };
        let message = RECT {
            left: text_left,
            top: title.bottom + s(6),
            right: width - s(PAD),
            bottom: title.bottom + s(6) + message_height,
        };
        let content = message.bottom.max(s(PAD + BADGE));
        let top = content + s(22);
        let ok = RECT {
            left: width - s(PAD) - s(92),
            top,
            right: width - s(PAD),
            bottom: top + s(BUTTON_HEIGHT),
        };
        let copy = RECT {
            left: ok.left - s(10) - s(92),
            right: ok.left - s(10),
            ..ok
        };
        Self {
            width,
            height: ok.bottom + s(20),
            badge: RECT {
                left: s(PAD),
                top: s(PAD),
                right: s(PAD + BADGE),
                bottom: s(PAD + BADGE),
            },
            title,
            message,
            copy,
            ok,
        }
    }

    fn hit(&self, point: POINT) -> Option<Button> {
        if contains(self.ok, point) {
            Some(Button::Ok)
        } else if contains(self.copy, point) {
            Some(Button::Copy)
        } else {
            None
        }
    }
}

/// Shows a modal themed dialog. The first line of `message` becomes the title.
pub unsafe fn show(owner: HWND, kind: Kind, message: &str) {
    if STATE.with_borrow(Option::is_some) {
        return;
    }
    let instance = GetModuleHandleW(null());
    let class = WNDCLASSW {
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        lpszClassName: CLASS,
        ..Default::default()
    };
    // Registration fails harmlessly when the class already exists.
    RegisterClassW(&class);
    let (title, body) = message.split_once('\n').unwrap_or((message, ""));
    let window = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        CLASS,
        w!("Snapshot"),
        WS_POPUP,
        0,
        0,
        WIDTH,
        200,
        owner,
        null_mut(),
        instance,
        null(),
    );
    if window.is_null() {
        return;
    }
    let scale = match GetDpiForWindow(window) {
        0 => 1.0,
        dpi => dpi as f32 / 96.0,
    };
    STATE.set(Some(State {
        kind,
        title: title.trim().trim_end_matches('。').to_string(),
        message: body.trim().to_string(),
        scale,
        fonts: Fonts::new(scale),
        hovered: None,
        pressed: None,
        copied: false,
        closed: false,
    }));
    place(window, owner);
    ui::round_corners(window);
    let owner_enabled = !owner.is_null() && IsWindowEnabled(owner) != 0;
    if owner_enabled {
        EnableWindow(owner, 0);
    }
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    SetFocus(window);
    let mut message = MSG::default();
    while !STATE.with_borrow(|state| state.as_ref().is_none_or(|state| state.closed)) {
        match GetMessageW(&mut message, null_mut(), 0, 0) {
            0 => {
                // Keep the quit request for the outer loop.
                PostQuitMessage(message.wParam as i32);
                break;
            }
            -1 => break,
            _ => {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    if owner_enabled {
        EnableWindow(owner, 1);
        SetForegroundWindow(owner);
    }
    if IsWindow(window) != 0 {
        DestroyWindow(window);
    }
    STATE.take();
}

unsafe fn place(window: HWND, owner: HWND) {
    let dc = GetDC(window);
    let size = STATE.with_borrow(|state| {
        state.as_ref().map(|state| {
            let layout = Layout::new(dc, state);
            (layout.width, layout.height)
        })
    });
    ReleaseDC(window, dc);
    let Some((width, height)) = size else { return };
    let mut anchor = POINT::default();
    let mut bounds = RECT::default();
    if !owner.is_null() && IsWindowVisible(owner) != 0 && GetWindowRect(owner, &mut bounds) != 0 {
        anchor = POINT {
            x: (bounds.left + bounds.right) / 2,
            y: (bounds.top + bounds.bottom) / 2,
        };
    } else {
        GetCursorPos(&mut anchor);
    }
    let (x, y) = monitor_info(anchor).map_or((anchor.x, anchor.y), |info| {
        let area = info.rcWork;
        (
            (area.left + (area.right - area.left - width) / 2).max(area.left),
            (area.top + (area.bottom - area.top - height) / 2).max(area.top),
        )
    });
    SetWindowPos(window, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE);
}

unsafe fn close(window: HWND) {
    STATE.with_borrow_mut(|state| {
        if let Some(state) = state {
            state.closed = true;
        }
    });
    ShowWindow(window, SW_HIDE);
    PostMessageW(window, WM_NULL, 0, 0);
}

unsafe fn copy_message(window: HWND) {
    let Some(text) = STATE.with_borrow(|state| {
        state.as_ref().map(|state| {
            if state.message.is_empty() {
                state.title.clone()
            } else {
                format!("{}\r\n{}", state.title, state.message)
            }
        })
    }) else {
        return;
    };
    if crate::output::copy_text(window, &text).is_err() {
        return;
    }
    STATE.with_borrow_mut(|state| {
        if let Some(state) = state {
            state.copied = true;
        }
    });
    InvalidateRect(window, null(), 0);
}

unsafe fn hit(window: HWND, lparam: LPARAM) -> Option<Button> {
    let point = mouse_point(lparam);
    let dc = GetDC(window);
    let hit = STATE.with_borrow(|state| {
        state
            .as_ref()
            .and_then(|state| Layout::new(dc, state).hit(point))
    });
    ReleaseDC(window, dc);
    hit
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(window, &mut paint);
            paint_window(window, dc);
            EndPaint(window, &paint);
        }
        WM_NCHITTEST => {
            let mut client = mouse_point(lparam);
            ScreenToClient(window, &mut client);
            let lparam = ((client.y as u32 & 0xffff) << 16 | (client.x as u32 & 0xffff)) as LPARAM;
            // Anything outside the buttons drags the dialog.
            return if hit(window, lparam).is_some() {
                HTCLIENT as LRESULT
            } else {
                HTCAPTION as LRESULT
            };
        }
        WM_MOUSEMOVE => {
            let hovered = hit(window, lparam);
            let changed = STATE.with_borrow_mut(|state| {
                state.as_mut().is_some_and(|state| {
                    std::mem::replace(&mut state.hovered, hovered) != hovered
                })
            });
            if changed {
                InvalidateRect(window, null(), 0);
            }
            SetCursor(LoadCursorW(
                null_mut(),
                if hovered.is_some() { IDC_HAND } else { IDC_ARROW },
            ));
        }
        WM_LBUTTONDOWN => {
            let pressed = hit(window, lparam);
            STATE.with_borrow_mut(|state| {
                if let Some(state) = state {
                    state.pressed = pressed;
                }
            });
            SetCapture(window);
            InvalidateRect(window, null(), 0);
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            let released = hit(window, lparam);
            let pressed = STATE.with_borrow_mut(|state| state.as_mut().and_then(|state| state.pressed.take()));
            InvalidateRect(window, null(), 0);
            match pressed.filter(|&button| Some(button) == released) {
                Some(Button::Ok) => close(window),
                Some(Button::Copy) => copy_message(window),
                None => {}
            }
        }
        WM_KEYDOWN => match wparam as u16 {
            VK_RETURN | VK_ESCAPE | VK_SPACE => close(window),
            key if key == u16::from(b'C') && GetKeyState(i32::from(VK_CONTROL)) < 0 => {
                copy_message(window)
            }
            _ => {}
        },
        WM_CLOSE => close(window),
        _ => return DefWindowProcW(window, message, wparam, lparam),
    }
    0
}

unsafe fn paint_window(window: HWND, target: HDC) {
    let mut client = RECT::default();
    GetClientRect(window, &mut client);
    let Ok(surface) = crate::capture::Surface::new(target, client.right, client.bottom) else {
        return;
    };
    STATE.with_borrow(|state| {
        if let Some(state) = state {
            paint_contents(surface.dc, client, state);
        }
    });
    BitBlt(
        target,
        0,
        0,
        client.right,
        client.bottom,
        surface.dc,
        0,
        0,
        SRCCOPY,
    );
}

unsafe fn paint_contents(dc: HDC, client: RECT, state: &State) {
    let palette = theme::palette();
    let layout = Layout::new(dc, state);
    ui::fill(dc, client, palette.card);
    if let Some(graphics) = Graphics::new(dc) {
        // Hairline border; DWM adds the rounded corners and shadow.
        graphics.stroke_round(
            ui::inset(ui::to_f(client), 0.5),
            0.0,
            argb(palette.card_border, 255),
            1.0,
        );
    }
    let (color, badge) = match state.kind {
        Kind::Error => (palette.error, Badge::Exclamation),
    };
    ui::badge(dc, layout.badge, color, badge, palette.on_accent);
    ui::text(
        dc,
        layout.title,
        &state.title,
        state.fonts.heading,
        palette.text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    ui::text(
        dc,
        layout.message,
        &state.message,
        state.fonts.body,
        palette.secondary,
        DT_LEFT | DT_TOP | DT_WORDBREAK | DT_END_ELLIPSIS,
    );
    let radius = 8.0 * state.scale;
    let face = |button: Button, base: u32, hover: u32| {
        if state.pressed == Some(button) {
            mix(hover, 0, 0.12)
        } else if state.hovered == Some(button) {
            hover
        } else {
            base
        }
    };
    ui::button(
        dc,
        layout.copy,
        if state.copied { "已复制" } else { "复制信息" },
        state.fonts.body,
        palette,
        false,
        face(Button::Copy, palette.card, palette.hover),
        radius,
    );
    ui::button(
        dc,
        layout.ok,
        "知道了",
        state.fonts.heading,
        palette,
        true,
        face(Button::Ok, palette.accent, palette.accent_hover),
        radius,
    );
}

/// Renders the dialog without a window, for the preview test.
#[cfg(test)]
pub unsafe fn preview(kind: Kind, message: &str, scale: f32) -> crate::capture::Image {
    use crate::capture::{Desktop, Surface};
    let (title, body) = message.split_once('\n').unwrap_or((message, ""));
    let state = State {
        kind,
        title: title.trim_end_matches('。').into(),
        message: body.into(),
        scale,
        fonts: Fonts::new(scale),
        hovered: Some(Button::Ok),
        pressed: None,
        copied: false,
        closed: false,
    };
    let probe = Surface::new(null_mut(), 1, 1).unwrap();
    let layout = Layout::new(probe.dc, &state);
    let bounds = RECT {
        right: layout.width,
        bottom: layout.height,
        ..Default::default()
    };
    let surface = Surface::new(null_mut(), bounds.right, bounds.bottom).unwrap();
    paint_contents(surface.dc, bounds, &state);
    GdiFlush();
    let mut image = Desktop { bounds, surface }.crop(bounds).unwrap();
    for pixel in image.pixels.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    image
}
