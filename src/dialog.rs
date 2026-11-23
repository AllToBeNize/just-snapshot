//! Themed replacement for `MessageBox`: a small rounded card with an icon,
//! title, wrapped message, "copy" and "OK" buttons. Runs its own modal loop.
//! Text is never cut off: everything wraps, and a message taller than the
//! screen scrolls inside the card.

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
                VK_CONTROL, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_NEXT, VK_PRIOR, VK_RETURN,
                VK_SPACE, VK_UP,
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
const TITLE_HEIGHT: i32 = 24;
/// Margin kept between the card and the edges of the work area.
const SCREEN_MARGIN: i32 = 48;
/// Wrap like an edit control: long words and paths break mid-word instead of
/// overflowing.
const WRAP: u32 = DT_WORDBREAK | DT_EDITCONTROL;

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
    /// Tallest the card may be, from the work area it is shown on.
    max_height: i32,
    /// Pixels of the message scrolled out above the visible area.
    scroll: i32,
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
    /// Height of the whole wrapped message; taller than `message` when it scrolls.
    message_full: i32,
    copy: RECT,
    ok: RECT,
}

impl Layout {
    unsafe fn new(dc: HDC, state: &State) -> Self {
        let s = |value: i32| (value as f32 * state.scale).round() as i32;
        let width = s(WIDTH);
        let text_left = s(PAD + BADGE + 14);
        let text_width = width - text_left - s(PAD);
        let title_height = ui::wrapped_height(dc, &state.title, state.fonts.heading, text_width, WRAP);
        let title_top = s(PAD) + (s(TITLE_HEIGHT) - title_height).max(0) / 2;
        let title = RECT {
            left: text_left,
            top: title_top,
            right: width - s(PAD),
            bottom: title_top + title_height,
        };
        let message_full = if state.message.is_empty() {
            0
        } else {
            ui::wrapped_height(dc, &state.message, state.fonts.body, text_width, WRAP)
        };
        let message_top = title.bottom.max(s(PAD + TITLE_HEIGHT)) + s(6);
        // Everything below the message: gap, buttons and bottom padding.
        let below = s(22) + s(BUTTON_HEIGHT) + s(20);
        let room = (state.max_height - message_top - below).max(s(60));
        let message = RECT {
            left: text_left,
            top: message_top,
            right: width - s(PAD),
            bottom: message_top + message_full.min(room),
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
            message_full,
            copy,
            ok,
        }
    }

    /// Largest scroll offset; 0 when the whole message is visible.
    fn max_scroll(&self) -> i32 {
        (self.message_full - (self.message.bottom - self.message.top)).max(0)
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
        w!("just-snapshot"),
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
        max_height: i32::MAX,
        scroll: 0,
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
    let info = monitor_info(anchor);
    let dc = GetDC(window);
    let size = STATE.with_borrow_mut(|state| {
        state.as_mut().map(|state| {
            if let Some(info) = &info {
                let margin = (SCREEN_MARGIN as f32 * state.scale).round() as i32;
                state.max_height = info.rcWork.bottom - info.rcWork.top - margin;
            }
            let layout = Layout::new(dc, state);
            (layout.width, layout.height)
        })
    });
    ReleaseDC(window, dc);
    let Some((width, height)) = size else { return };
    let (x, y) = info.map_or((anchor.x, anchor.y), |info| {
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

/// Scrolls the message by `delta` pixels (positive moves the text up).
unsafe fn scroll_by(window: HWND, delta: i32) {
    let dc = GetDC(window);
    let changed = STATE.with_borrow_mut(|state| {
        state.as_mut().is_some_and(|state| {
            let max = Layout::new(dc, state).max_scroll();
            let next = (state.scroll + delta).clamp(0, max);
            std::mem::replace(&mut state.scroll, next) != next
        })
    });
    ReleaseDC(window, dc);
    if changed {
        InvalidateRect(window, null(), 0);
    }
}

/// One wheel notch or arrow press, and one page, in pixels.
fn scroll_steps() -> (i32, i32) {
    STATE.with_borrow(|state| {
        state.as_ref().map_or((60, 300), |state| {
            let s = |value: f32| (value * state.scale).round() as i32;
            (s(60.0), (state.max_height / 2).max(s(60.0)))
        })
    })
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
        WM_MOUSEWHEEL => {
            let notches = i32::from((wparam >> 16) as i16) as f32 / WHEEL_DELTA as f32;
            scroll_by(window, -(notches * scroll_steps().0 as f32).round() as i32);
        }
        WM_KEYDOWN => match wparam as u16 {
            VK_RETURN | VK_ESCAPE | VK_SPACE => close(window),
            VK_UP => scroll_by(window, -scroll_steps().0),
            VK_DOWN => scroll_by(window, scroll_steps().0),
            VK_PRIOR => scroll_by(window, -scroll_steps().1),
            VK_NEXT => scroll_by(window, scroll_steps().1),
            VK_HOME => scroll_by(window, i32::MIN / 2),
            VK_END => scroll_by(window, i32::MAX / 2),
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
        DT_LEFT | DT_TOP | WRAP,
    );
    paint_message(dc, &layout, state, palette);
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

/// The message, clipped to its area and shifted by the scroll offset, with a
/// scroll thumb when it does not fit.
unsafe fn paint_message(dc: HDC, layout: &Layout, state: &State, palette: &theme::Palette) {
    let area = layout.message;
    let max_scroll = layout.max_scroll();
    let scroll = state.scroll.min(max_scroll);
    let saved = SaveDC(dc);
    IntersectClipRect(dc, area.left, area.top, area.right, area.bottom);
    ui::text(
        dc,
        RECT {
            top: area.top - scroll,
            bottom: area.top - scroll + layout.message_full,
            ..area
        },
        &state.message,
        state.fonts.body,
        palette.secondary,
        DT_LEFT | DT_TOP | WRAP,
    );
    RestoreDC(dc, saved);
    if max_scroll == 0 {
        return;
    }
    let s = |value: f32| value * state.scale;
    let visible = (area.bottom - area.top) as f32;
    let thumb = (visible * visible / layout.message_full as f32).max(s(24.0));
    let top = area.top as f32 + (visible - thumb) * scroll as f32 / max_scroll as f32;
    let left = area.right as f32 + s(PAD as f32 / 2.0 - 2.0);
    if let Some(graphics) = Graphics::new(dc) {
        graphics.fill_round(
            [left, top, left + s(4.0), top + thumb],
            s(2.0),
            argb(palette.tertiary, 200),
        );
    }
}

/// Renders the dialog without a window, for the preview test.
#[cfg(test)]
pub unsafe fn preview(kind: Kind, message: &str, scale: f32, max_height: i32) -> crate::capture::Image {
    use crate::capture::{Desktop, Surface};
    let (title, body) = message.split_once('\n').unwrap_or((message, ""));
    let state = State {
        kind,
        title: title.trim_end_matches('。').into(),
        message: body.into(),
        scale,
        fonts: Fonts::new(scale),
        max_height,
        scroll: 0,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Surface;

    const LONG: &str = "长截图拼接失败，因为连续多帧找不到重叠区域，可能是页面里有固定悬浮的元素、动画，或者滚动太快。\n\
        C:\\Users\\ERIC\\AppData\\Local\\Temp\\snapshot-ai\\long-capture\\frame-0042-overlap-search-failed.png\n\
        The operation completed but the stitched image would exceed the maximum supported height of 32767 pixels.";

    unsafe fn state(message: &str, max_height: i32) -> State {
        let (title, body) = message.split_once('\n').unwrap_or((message, ""));
        State {
            kind: Kind::Error,
            title: title.into(),
            message: body.into(),
            scale: 1.0,
            fonts: Fonts::new(1.0),
            max_height,
            scroll: 0,
            hovered: None,
            pressed: None,
            copied: false,
            closed: false,
        }
    }

    #[test]
    fn long_text_wraps_in_full() {
        unsafe {
            let probe = Surface::new(null_mut(), 1, 1).unwrap();
            let long = state(LONG, i32::MAX);
            let layout = Layout::new(probe.dc, &long);
            let short = Layout::new(probe.dc, &state("出错了\n短", i32::MAX));
            assert!(
                layout.title.bottom - layout.title.top > short.title.bottom - short.title.top,
                "a long title wraps onto more lines instead of being cut off"
            );
            assert_eq!(layout.max_scroll(), 0, "a message that fits does not scroll");
            assert_eq!(layout.message.bottom - layout.message.top, layout.message_full);
            // The unbreakable path must wrap within the text column, not overflow it.
            let width = layout.message.right - layout.message.left;
            let one_line = ui::wrapped_height(probe.dc, "C", long.fonts.body, width, WRAP);
            assert!(layout.message_full >= one_line * 5, "the path is broken across lines");
        }
    }

    #[test]
    fn a_message_taller_than_the_screen_scrolls() {
        unsafe {
            let probe = Surface::new(null_mut(), 1, 1).unwrap();
            let message = format!("出错了\n{}", "这一行是很长的错误详情。\n".repeat(80));
            let mut state = state(&message, 400);
            let layout = Layout::new(probe.dc, &state);
            assert!(layout.height <= 400, "the card fits the screen: {}", layout.height);
            assert!(layout.max_scroll() > 0);
            assert_eq!(
                layout.max_scroll(),
                layout.message_full - (layout.message.bottom - layout.message.top),
                "scrolling reaches the last line"
            );
            state.scroll = layout.max_scroll();
            assert_eq!(Layout::new(probe.dc, &state).max_scroll(), layout.max_scroll());
        }
    }
}
