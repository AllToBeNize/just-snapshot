use crate::{
    capture::{Image, Surface},
    hotkey::DEFAULT_OPACITY_STEP,
    ocr,
    output::copy_text,
    pin_text::{Status, TextLayer, View},
    pin_translate::{self, Phase, Target as PaneTarget, TranslatePane},
    selection_geometry::{contains, mouse_point},
    show_error,
    theme::{self, mix},
    toolbar::paint_size_badge,
    translate::{self, Service, LANGUAGES},
    translate_settings,
    ui::{self, Fonts},
};
use std::{
    cell::Cell,
    io,
    mem::size_of,
    ptr::{null, null_mut},
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::WM_MOUSELEAVE,
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{
                GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE,
                TME_NONCLIENT, TRACKMOUSEEVENT, VK_CONTROL, VK_ESCAPE,
            },
            WindowsAndMessaging::*,
        },
    },
};

const CLASS: *const u16 = w!("Snapshot.Pinned");
const GLOW: i32 = 5;
const BADGE_TIMER: usize = 1;
const BADGE_DURATION_MS: u32 = 1200;
const MIN_OPACITY: i32 = 15;
const WHEEL_NOTCH: i32 = 120;
const TOAST_TIMER: usize = 2;
const TOAST_MS: u32 = 2400;
/// Posted by the recognition thread; `lparam` owns a `Box<Result<Vec<Line>, String>>`.
const OCR_DONE: u32 = WM_APP + 1;
/// Posted by the translation thread; `wparam` is the request id and `lparam`
/// owns a `Box<Outcome>`.
const TRANSLATE_DONE: u32 = WM_APP + 2;
/// The translation settings changed; pins waiting for them try again.
const RETRANSLATE: u32 = WM_APP + 3;
const SHIMMER_TIMER: usize = 3;
const SHIMMER_FRAME_MS: u32 = 33;
const WHEEL_SCROLL: i32 = 48;
const WM_NCMOUSELEAVE: u32 = 0x02a2;

thread_local! {
    static OPACITY_STEP: Cell<u8> = const { Cell::new(DEFAULT_OPACITY_STEP) };
}

pub fn opacity_step() -> u8 {
    OPACITY_STEP.get()
}

pub fn set_opacity_step(step: u8) {
    OPACITY_STEP.set(step);
}

/// What a pin does with its image besides showing it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Plain,
    /// Recognize the text so it can be selected and copied.
    Text,
    /// Recognize the text and translate it in a panel beside the image.
    Translate,
}

enum Outcome {
    Done(String),
    Unconfigured,
    Failed(String),
}

#[derive(Clone, Copy)]
enum Badge {
    Zoom,
    Opacity,
}

struct Pin {
    surface: Surface,
    width: i32,
    height: i32,
    zoom: f64,
    opacity: i32,
    opacity_wheel_remainder: i32,
    badge: Option<Badge>,
    /// Present on pins made with "extract text" or "translate".
    text: Option<TextLayer>,
    /// Present on pins made with "translate".
    translate: Option<TranslatePane>,
    /// Kept for recognizing again after a failure.
    source: Option<Image>,
    /// Whether the current text drag has moved past its first word.
    dragged: bool,
    scale: f32,
    fonts: Fonts,
}

impl Pin {
    fn s(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    /// The image size at the current zoom.
    fn display_size(&self) -> (i32, i32) {
        let size = |value: i32| (f64::from(value) * self.zoom).round().max(1.0) as i32;
        (size(self.width), size(self.height))
    }

    fn panel_width(&self) -> i32 {
        let (width, _) = self.display_size();
        width.clamp(self.s(pin_translate::MIN_WIDTH), self.s(pin_translate::MAX_WIDTH))
    }

    /// The whole window: the image, the translation panel if any, and the glow.
    fn window_size(&self) -> (i32, i32) {
        let (width, height) = self.display_size();
        if self.translate.is_some() {
            (
                width + 1 + self.panel_width() + GLOW * 2,
                height.max(self.s(pin_translate::MIN_HEIGHT)) + GLOW * 2,
            )
        } else {
            (width + GLOW * 2, height + GLOW * 2)
        }
    }

    /// The image area inside the glow, in client pixels.
    fn image_rect(&self) -> RECT {
        let (width, height) = self.display_size();
        RECT {
            left: GLOW,
            top: GLOW,
            right: GLOW + width,
            bottom: GLOW + height,
        }
    }

    fn panel_rect(&self) -> RECT {
        let (width, height) = self.window_size();
        RECT {
            left: self.image_rect().right + 1,
            top: GLOW,
            right: width - GLOW,
            bottom: height - GLOW,
        }
    }

    unsafe fn pane_layout(&self, window: HWND) -> Option<pin_translate::Layout> {
        let pane = self.translate.as_ref()?;
        let dc = GetDC(window);
        let layout = pane.layout(self.panel_rect(), self.scale, dc, &self.fonts);
        ReleaseDC(window, dc);
        Some(layout)
    }

    unsafe fn pane_hit(&self, window: HWND, point: POINT) -> Option<PaneTarget> {
        let layout = self.pane_layout(window)?;
        self.translate.as_ref()?.hit(&layout, point)
    }

    unsafe fn view(&self, _window: HWND) -> View {
        let image = self.image_rect();
        View {
            origin: POINT {
                x: image.left,
                y: image.top,
            },
            zoom: (image.right - image.left) as f32 / self.width as f32,
            scale: self.scale,
        }
    }

    unsafe fn over_button(&self, window: HWND, point: POINT) -> bool {
        let Some(text) = &self.text else { return false };
        let dc = GetDC(window);
        let button = text.button(self.view(window), self.image_rect(), &self.fonts, dc);
        ReleaseDC(window, dc);
        button.is_some_and(|button| contains(button, point))
    }
}

pub unsafe fn register(instance: HINSTANCE) -> io::Result<()> {
    let class = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hCursor: LoadCursorW(null_mut(), IDC_SIZEALL),
        lpszClassName: CLASS,
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Pins `image` at `bounds`. Text and translate pins recognize the text in
/// the background; translate pins then translate it beside the image.
pub unsafe fn open(image: Image, bounds: RECT, mode: Mode) -> io::Result<()> {
    let source = (mode != Mode::Plain).then(|| image.clone());
    let (surface, width, height) = image.into_surface()?;
    let window = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
        CLASS,
        w!("Snapshot 贴图"),
        WS_POPUP,
        bounds.left - GLOW,
        bounds.top - GLOW,
        width + GLOW * 2,
        height + GLOW * 2,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if window.is_null() {
        return Err(io::Error::last_os_error());
    }
    let scale = match GetDpiForWindow(window) {
        0 => 1.0,
        dpi => dpi as f32 / 96.0,
    };
    let pin = Box::new(Pin {
        surface,
        width,
        height,
        zoom: 1.0,
        opacity: 100,
        opacity_wheel_remainder: 0,
        badge: None,
        text: source.is_some().then(TextLayer::new),
        translate: (mode == Mode::Translate).then(TranslatePane::new),
        source,
        dragged: false,
        scale,
        fonts: Fonts::new(scale),
    });
    if pin.translate.is_some() {
        // The panel widens the pin; keep it on the monitor.
        let (width, height) = pin.window_size();
        let mut monitor = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(MonitorFromRect(&bounds, MONITOR_DEFAULTTONEAREST), &mut monitor);
        let work = monitor.rcWork;
        let left = (bounds.left - GLOW).min(work.right - width).max(work.left);
        let top = (bounds.top - GLOW).min(work.bottom - height).max(work.top);
        SetWindowPos(window, null_mut(), left, top, width, height, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    let recognize = pin.source.is_some();
    let pin = Box::into_raw(pin);
    SetWindowLongPtrW(window, GWLP_USERDATA, pin as isize);
    if SetLayeredWindowAttributes(window, 0, 255, LWA_ALPHA) == 0 {
        let error = io::Error::last_os_error();
        DestroyWindow(window);
        return Err(error);
    }
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    if recognize {
        start_recognition(window, &mut *pin);
    }
    Ok(())
}

/// A millisecond clock for animations; only differences matter.
fn now_ms() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_millis() as u32)
}

/// Tells every translate pin that the translation settings changed.
pub unsafe fn translation_settings_changed() {
    let mut window = FindWindowExW(null_mut(), null_mut(), CLASS, null());
    while !window.is_null() {
        PostMessageW(window, RETRANSLATE, 0, 0);
        window = FindWindowExW(null_mut(), window, CLASS, null());
    }
}

unsafe fn start_recognition(window: HWND, pin: &mut Pin) {
    let Some(source) = pin.source.clone() else { return };
    if let Some(text) = &mut pin.text {
        text.status = Status::Recognizing;
    }
    if let Some(pane) = &mut pin.translate {
        pane.phase = Phase::Recognizing;
        SetTimer(window, SHIMMER_TIMER, SHIMMER_FRAME_MS, None);
    }
    InvalidateRect(window, null(), 0);
    let target = window as isize;
    std::thread::spawn(move || {
        let result = Box::into_raw(Box::new(ocr::recognize(&source)));
        if PostMessageW(target as HWND, OCR_DONE, 0, result as LPARAM) == 0 {
            // The pin was closed before recognition finished.
            drop(Box::from_raw(result));
        }
    });
}

/// Translates the recognized text into the chosen language in the background.
unsafe fn start_translation(window: HWND, pin: &mut Pin) {
    let Some(pane) = &mut pin.translate else { return };
    let lines = pin.text.as_ref().map_or(&[][..], TextLayer::lines);
    pane.scroll = 0;
    if lines.is_empty() {
        pane.phase = Phase::NoText;
        InvalidateRect(window, null(), 0);
        return;
    }
    let text = ocr::text(lines);
    let to = pane.language.resolve(&text);
    let config = translate::Config::load();
    pane.request = pane.request.wrapping_add(1);
    pane.phase = Phase::Translating {
        to,
        provider: config.as_ref().ok().and_then(|config| config.provider),
    };
    let request = pane.request;
    SetTimer(window, SHIMMER_TIMER, SHIMMER_FRAME_MS, None);
    InvalidateRect(window, null(), 0);
    let target = window as isize;
    std::thread::spawn(move || {
        let outcome = match config.and_then(|config| Service::from_config(&config)) {
            Ok(Some(service)) => match translate::translate(&service, &text, to) {
                Ok(translation) => Outcome::Done(translation),
                Err(error) => Outcome::Failed(error),
            },
            Ok(None) => Outcome::Unconfigured,
            Err(error) => Outcome::Failed(error),
        };
        let outcome = Box::into_raw(Box::new(outcome));
        if PostMessageW(target as HWND, TRANSLATE_DONE, request as WPARAM, outcome as LPARAM) == 0 {
            drop(Box::from_raw(outcome));
        }
    });
}

unsafe fn copy_translation(window: HWND, pin: &mut Pin) {
    let Some(translation) = pin.translate.as_ref().and_then(TranslatePane::translation) else {
        return;
    };
    match copy_text(window, &translation.replace('\n', "\r\n")) {
        Ok(()) => toast(window, pin, "已复制译文".into()),
        Err(error) => show_error(window, &format!("无法复制译文。\n{error}")),
    }
}

unsafe fn open_translation_settings(window: HWND) {
    if let Err(error) = translate_settings::open(window) {
        show_error(window, &format!("无法打开翻译设置。\n{error}"));
    }
}

/// The language picker: a menu under its button.
unsafe fn pick_language(window: HWND, pin: &mut Pin) {
    let Some(layout) = pin.pane_layout(window) else { return };
    let Some(pane) = &pin.translate else { return };
    let menu = CreatePopupMenu();
    for (index, language) in LANGUAGES.iter().enumerate() {
        let label = match language {
            translate::Language::Auto => "自动（中文译成英文，其他译成中文）".to_string(),
            language => language.label().to_string(),
        };
        let flags = if *language == pane.language { MF_STRING | MF_CHECKED } else { MF_STRING };
        AppendMenuW(menu, flags, index + 1, crate::wide(&label).as_ptr());
    }
    let anchor = layout.language();
    let mut point = POINT {
        x: anchor.left,
        y: anchor.bottom + pin.s(4),
    };
    ClientToScreen(window, &mut point);
    let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY, point.x, point.y, 0, window, null());
    DestroyMenu(menu);
    let Some(&language) = (chosen as usize).checked_sub(1).and_then(|index| LANGUAGES.get(index)) else {
        return;
    };
    if let Some(pane) = &mut pin.translate {
        pane.language = language;
    }
    start_translation(window, pin);
}

unsafe fn activate_pane(window: HWND, pin: &mut Pin, target: PaneTarget) {
    match target {
        PaneTarget::Language => pick_language(window, pin),
        PaneTarget::Copy => copy_translation(window, pin),
        PaneTarget::Settings | PaneTarget::Secondary => open_translation_settings(window),
        PaneTarget::Primary => {
            let phase = pin.translate.as_ref().map(|pane| &pane.phase);
            if matches!(phase, Some(Phase::Unconfigured)) {
                open_translation_settings(window);
            } else if pin.text.as_ref().is_some_and(|text| matches!(text.status, Status::Failed)) {
                start_recognition(window, pin);
            } else {
                start_translation(window, pin);
            }
        }
    }
}

unsafe fn toast(window: HWND, pin: &mut Pin, message: String) {
    if let Some(text) = &mut pin.text {
        text.toast = Some(message);
        SetTimer(window, TOAST_TIMER, TOAST_MS, None);
        InvalidateRect(window, null(), 0);
    }
}

unsafe fn copy(window: HWND, pin: &mut Pin, all: bool) {
    let Some(text) = &pin.text else { return };
    if text.lines().is_empty() {
        return;
    }
    let value = if all {
        ocr::text(text.lines())
    } else {
        text.copy_text()
    };
    match copy_text(window, &value) {
        Ok(()) => {
            let count = value.chars().filter(|c| !c.is_whitespace()).count();
            toast(window, pin, format!("已复制 {count} 个字"));
        }
        Err(error) => show_error(window, &format!("无法复制文字。\n{error}")),
    }
}

/// Starts leave tracking for the client area or the frame.
unsafe fn track_leave(window: HWND, client: bool) {
    let mut track = TRACKMOUSEEVENT {
        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: if client { TME_LEAVE } else { TME_LEAVE | TME_NONCLIENT },
        hwndTrack: window,
        dwHoverTime: 0,
    };
    TrackMouseEvent(&mut track);
}

unsafe fn set_hover(window: HWND, pin: &mut Pin, hovering: bool, button: bool) {
    if let Some(text) = &mut pin.text {
        if text.hovering != hovering || text.button_hovered != button {
            text.hovering = hovering;
            text.button_hovered = button;
            InvalidateRect(window, null(), 0);
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pin = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Pin;
    match message {
        WM_NCHITTEST => {
            if !pin.is_null() && (*pin).text.is_some() {
                let mut point = mouse_point(lparam);
                ScreenToClient(window, &mut point);
                if (*pin).pane_hit(window, point).is_some() {
                    return HTCLIENT as LRESULT;
                }
                let view = (*pin).view(window);
                let text = (*pin).text.as_ref().unwrap();
                if text.selecting || text.over_text(view, point) || (*pin).over_button(window, point) {
                    return HTCLIENT as LRESULT;
                }
            }
            return HTCAPTION as LRESULT;
        }
        WM_SETCURSOR if !pin.is_null() && (lparam & 0xffff) as u32 == HTCLIENT => {
            let mut point = POINT::default();
            GetCursorPos(&mut point);
            ScreenToClient(window, &mut point);
            let clickable = (*pin).over_button(window, point) || (*pin).pane_hit(window, point).is_some();
            let cursor = if clickable { IDC_HAND } else { IDC_IBEAM };
            SetCursor(LoadCursorW(null_mut(), cursor));
            return 1;
        }
        WM_MOUSEMOVE if !pin.is_null() => {
            let point = mouse_point(lparam);
            track_leave(window, true);
            let button = (*pin).over_button(window, point);
            set_hover(window, &mut *pin, true, button);
            let hit = (*pin).pane_hit(window, point);
            if let Some(pane) = &mut (*pin).translate {
                if std::mem::replace(&mut pane.hovered, hit) != hit {
                    InvalidateRect(window, null(), 0);
                }
            }
            let view = (*pin).view(window);
            if let Some(text) = &mut (*pin).text {
                if text.selecting && text.extend(view, point) {
                    (*pin).dragged = true;
                    InvalidateRect(window, null(), 0);
                }
            }
        }
        WM_NCMOUSEMOVE if !pin.is_null() => {
            track_leave(window, false);
            set_hover(window, &mut *pin, true, false);
        }
        WM_MOUSELEAVE | WM_NCMOUSELEAVE if !pin.is_null() => {
            // Moving between the frame and the text also "leaves"; only the real exit counts.
            let mut point = POINT::default();
            let mut bounds = RECT::default();
            GetCursorPos(&mut point);
            GetWindowRect(window, &mut bounds);
            if !contains(bounds, point) {
                set_hover(window, &mut *pin, false, false);
            }
            if let Some(pane) = &mut (*pin).translate {
                if pane.hovered.take().is_some() {
                    InvalidateRect(window, null(), 0);
                }
            }
        }
        WM_LBUTTONDOWN if !pin.is_null() => {
            let point = mouse_point(lparam);
            let hit = (*pin).pane_hit(window, point);
            if let (Some(hit), Some(pane)) = (hit, &mut (*pin).translate) {
                pane.pressed = Some(hit);
                SetCapture(window);
                InvalidateRect(window, null(), 0);
            } else if (*pin).over_button(window, point) {
                copy(window, &mut *pin, true);
            } else {
                let view = (*pin).view(window);
                if let Some(text) = &mut (*pin).text {
                    text.begin(view, point);
                    (*pin).dragged = false;
                    SetCapture(window);
                    InvalidateRect(window, null(), 0);
                }
            }
        }
        WM_LBUTTONUP if !pin.is_null() => {
            let pressed = (*pin).translate.as_mut().and_then(|pane| pane.pressed.take());
            if let Some(pressed) = pressed {
                ReleaseCapture();
                InvalidateRect(window, null(), 0);
                if (*pin).pane_hit(window, mouse_point(lparam)) == Some(pressed) {
                    activate_pane(window, &mut *pin, pressed);
                }
                return 0;
            }
            if let Some(text) = &mut (*pin).text {
                if text.selecting {
                    text.selecting = false;
                    // A click without dragging clears the selection.
                    if !(*pin).dragged {
                        text.clear();
                    }
                    ReleaseCapture();
                    InvalidateRect(window, null(), 0);
                }
            }
        }
        WM_LBUTTONDBLCLK if !pin.is_null() => {
            let view = (*pin).view(window);
            if let Some(text) = &mut (*pin).text {
                text.select_line(view, mouse_point(lparam));
                InvalidateRect(window, null(), 0);
            }
        }
        WM_CAPTURECHANGED if !pin.is_null() => {
            if let Some(text) = &mut (*pin).text {
                text.selecting = false;
            }
            if let Some(pane) = &mut (*pin).translate {
                if pane.pressed.take().is_some() {
                    InvalidateRect(window, null(), 0);
                }
            }
        }
        OCR_DONE if !pin.is_null() => {
            let result = *Box::from_raw(lparam as *mut Result<Vec<ocr::Line>, String>);
            let translating = (*pin).translate.is_some();
            match result {
                Ok(lines) => {
                    let message = if lines.is_empty() {
                        "没有识别到文字".to_string()
                    } else {
                        format!("识别到 {} 行文字，拖动选择，Ctrl+C 复制", lines.len())
                    };
                    if let Some(text) = &mut (*pin).text {
                        text.status = Status::Ready(lines);
                    }
                    if translating {
                        // The panel reports progress; Ctrl+C copies the translation.
                        start_translation(window, &mut *pin);
                    } else {
                        toast(window, &mut *pin, message);
                    }
                }
                Err(error) => {
                    if let Some(text) = &mut (*pin).text {
                        text.status = Status::Failed;
                    }
                    InvalidateRect(window, null(), 0);
                    match &mut (*pin).translate {
                        // The panel shows the whole error with a retry button.
                        Some(pane) => pane.phase = Phase::Failed(error),
                        None => show_error(window, &error),
                    }
                }
            }
        }
        TRANSLATE_DONE if !pin.is_null() => {
            let outcome = *Box::from_raw(lparam as *mut Outcome);
            if let Some(pane) = &mut (*pin).translate {
                let to = match pane.phase {
                    Phase::Translating { to, .. } if pane.request == wparam as u32 => to,
                    // A newer request replaced this one.
                    _ => return 0,
                };
                pane.scroll = 0;
                pane.phase = match outcome {
                    Outcome::Done(text) => Phase::Done { text, to },
                    Outcome::Unconfigured => Phase::Unconfigured,
                    Outcome::Failed(error) => Phase::Failed(error),
                };
                InvalidateRect(window, null(), 0);
            }
        }
        RETRANSLATE if !pin.is_null() => {
            let waiting = (*pin)
                .translate
                .as_ref()
                .is_some_and(|pane| matches!(pane.phase, Phase::Unconfigured | Phase::Failed(_)));
            let recognized = (*pin).text.as_ref().is_some_and(|text| matches!(text.status, Status::Ready(_)));
            if waiting && recognized {
                start_translation(window, &mut *pin);
            }
        }
        WM_TIMER if !pin.is_null() && wparam == SHIMMER_TIMER => {
            if !(*pin).translate.as_ref().is_some_and(TranslatePane::busy) {
                KillTimer(window, SHIMMER_TIMER);
            }
            InvalidateRect(window, null(), 0);
        }
        WM_TIMER if !pin.is_null() && wparam == TOAST_TIMER => {
            KillTimer(window, TOAST_TIMER);
            if let Some(text) = &mut (*pin).text {
                text.toast = None;
            }
            InvalidateRect(window, null(), 0);
        }
        WM_ERASEBKGND => return 1,
        WM_NCDESTROY if !pin.is_null() => {
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            drop(Box::from_raw(pin));
            return DefWindowProcW(window, message, wparam, lparam);
        }
        WM_NCRBUTTONUP | WM_RBUTTONUP | WM_NCLBUTTONDBLCLK => {
            DestroyWindow(window);
        }
        WM_KEYDOWN if wparam == VK_ESCAPE as usize => {
            let cleared = !pin.is_null() && (*pin).text.as_mut().is_some_and(TextLayer::clear);
            if cleared {
                InvalidateRect(window, null(), 0);
            } else {
                DestroyWindow(window);
            }
        }
        WM_KEYDOWN if !pin.is_null() && GetKeyState(i32::from(VK_CONTROL)) < 0 => {
            match wparam as u8 {
                b'C' => {
                    // Without a selection, a translate pin copies its translation.
                    let selected = (*pin).text.as_ref().is_some_and(TextLayer::has_selection);
                    let translated = (*pin).translate.as_ref().and_then(TranslatePane::translation).is_some();
                    if translated && !selected {
                        copy_translation(window, &mut *pin);
                    } else {
                        copy(window, &mut *pin, false);
                    }
                }
                b'A' => {
                    if let Some(text) = &mut (*pin).text {
                        text.select_all();
                        InvalidateRect(window, null(), 0);
                    }
                }
                _ => {}
            }
        }
        WM_MOUSEWHEEL if !pin.is_null() => {
            let delta = i32::from((wparam >> 16) as i16);
            let mut point = mouse_point(lparam);
            ScreenToClient(window, &mut point);
            if let Some(layout) = (*pin).pane_layout(window) {
                if wparam & 8 == 0 && contains(layout.panel, point) {
                    let dc = GetDC(window);
                    let step = -delta * (*pin).s(WHEEL_SCROLL) / WHEEL_NOTCH;
                    let fonts = &(*pin).fonts;
                    let moved = (*pin)
                        .translate
                        .as_mut()
                        .is_some_and(|pane| pane.scroll_by(&layout, dc, fonts, step));
                    ReleaseDC(window, dc);
                    if moved {
                        InvalidateRect(window, null(), 0);
                    }
                    return 0;
                }
            }
            let badge = if wparam & 8 != 0 {
                let pending = (*pin).opacity_wheel_remainder + delta;
                (*pin).opacity_wheel_remainder = pending % WHEEL_NOTCH;
                let notches = pending / WHEEL_NOTCH;
                if notches == 0 {
                    return 0;
                }
                let opacity =
                    ((*pin).opacity + notches * i32::from(opacity_step())).clamp(MIN_OPACITY, 100);
                let alpha = ((opacity * 255 + 50) / 100) as u8;
                if SetLayeredWindowAttributes(window, 0, alpha, LWA_ALPHA) != 0 {
                    (*pin).opacity = opacity;
                }
                Badge::Opacity
            } else {
                let zoom = ((*pin).zoom * 1.15_f64.powf(f64::from(delta) / f64::from(WHEEL_NOTCH)))
                    .clamp(0.15, 4.0);
                (*pin).zoom = zoom;
                let (width, height) = (*pin).window_size();
                SetWindowPos(
                    window,
                    null_mut(),
                    0,
                    0,
                    width,
                    height,
                    SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER,
                );
                Badge::Zoom
            };
            if SetTimer(window, BADGE_TIMER, BADGE_DURATION_MS, None) != 0 {
                (*pin).badge = Some(badge);
            }
            InvalidateRect(window, null(), 0);
        }
        WM_TIMER if !pin.is_null() && wparam == BADGE_TIMER => {
            KillTimer(window, BADGE_TIMER);
            (*pin).badge = None;
            InvalidateRect(window, null(), 0);
        }
        WM_PAINT if !pin.is_null() => {
            let mut paint = PAINTSTRUCT::default();
            let target = BeginPaint(window, &mut paint);
            let mut client = RECT::default();
            GetClientRect(window, &mut client);
            // Paint off screen so the selection does not flicker while dragging.
            let Ok(buffer) = Surface::new(target, client.right, client.bottom) else {
                EndPaint(window, &paint);
                return 0;
            };
            let dc = buffer.dc;
            let mut bounds = client;
            let brush = GetStockObject(DC_BRUSH);
            // A glow that fades from the theme accent into its shadow.
            let palette = theme::palette();
            for step in 0..5 {
                let color = mix(palette.shadow, palette.accent_hover, step as f32 / 4.0);
                SetDCBrushColor(dc, color);
                FrameRect(dc, &bounds, brush);
                InflateRect(&mut bounds, -1, -1);
            }
            let bounds = (*pin).image_rect();
            if (*pin).translate.is_some() {
                // The panel may be taller than the image; fill below it too.
                ui::fill(dc, ui::inflate(client, -GLOW), palette.card);
                ui::fill(
                    dc,
                    RECT { left: bounds.right, right: bounds.right + 1, top: GLOW, bottom: client.bottom - GLOW },
                    palette.divider,
                );
            }
            SetStretchBltMode(dc, HALFTONE);
            SetBrushOrgEx(dc, 0, 0, null_mut());
            StretchBlt(
                dc,
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                (*pin).surface.dc,
                0,
                0,
                (*pin).width,
                (*pin).height,
                SRCCOPY,
            );
            if let Some(text) = &(*pin).text {
                text.paint(dc, (*pin).view(window), bounds, &(*pin).fonts);
            }
            if let (Some(pane), Some(layout)) = (&(*pin).translate, (*pin).pane_layout(window)) {
                pane.paint(dc, &layout, &(*pin).fonts, now_ms());
            }
            if let Some(badge) = (*pin).badge {
                paint_badge(dc, bounds, &*pin, badge);
            }
            BitBlt(target, 0, 0, client.right, client.bottom, dc, 0, 0, SRCCOPY);
            drop(buffer);
            EndPaint(window, &paint);
        }
        _ => return DefWindowProcW(window, message, wparam, lparam),
    }
    0
}

unsafe fn paint_badge(dc: HDC, image: RECT, pin: &Pin, badge: Badge) {
    const WIDTH: i32 = 110;
    const HEIGHT: i32 = 24;

    let width = image.right - image.left;
    if width < 44 || image.bottom - image.top < HEIGHT {
        return;
    }
    let (name, percent) = match badge {
        Badge::Zoom => ("大小", (pin.zoom * 100.0).round() as i32),
        Badge::Opacity => ("不透明度", pin.opacity),
    };
    let text = if width >= WIDTH {
        format!("{name} {percent}%")
    } else {
        format!("{percent}%")
    };
    let label: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let bounds = RECT {
        left: image.left,
        top: image.top,
        right: image.left + width.min(WIDTH),
        bottom: image.top + HEIGHT,
    };
    paint_size_badge(dc, bounds, label.as_ptr());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens a text pin and pumps messages until background recognition lands.
    /// Run with `<test binary> pinned --ignored`.
    #[test]
    #[ignore]
    fn text_pin_becomes_selectable_after_recognition() {
        unsafe {
            let _ = register(GetModuleHandleW(null()));
            let image = crate::ocr::tests::sample("识别文字 Snapshot 保存到 C:\\Users\\eric\\Pictures", 18, true);
            let bounds = RECT { left: 100, top: 100, right: 100 + image.width, bottom: 100 + image.height };
            open(image, bounds, Mode::Text).unwrap();
            let window = FindWindowW(CLASS, null());
            assert!(!window.is_null());
            let pin = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Pin;
            let ready = || matches!((*pin).text.as_ref().unwrap().status, Status::Ready(_));
            assert!(!ready(), "recognition runs in the background");
            let start = std::time::Instant::now();
            let mut message = MSG::default();
            while !ready() && start.elapsed().as_secs() < 10 {
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let text = (*pin).text.as_mut().unwrap();
            assert!(ready(), "recognized within 10 s");
            println!("{:?} in {:?}", text.copy_text(), start.elapsed());
            assert!(text.copy_text().contains("Pictures"));
            assert!(text.toast.as_deref().is_some_and(|toast| toast.contains("1 行")));
            // Text is a selection target; the margin around it still drags the pin.
            let view = (*pin).view(window);
            let word = text.lines()[0].words[0].rect;
            let on_word = POINT {
                x: view.origin.x + ((word[0] + word[2]) / 2.0) as i32,
                y: view.origin.y + ((word[1] + word[3]) / 2.0) as i32,
            };
            assert!(text.over_text(view, on_word));
            assert!(!text.over_text(view, POINT { x: view.origin.x + 2, y: view.origin.y + 2 }));
            text.select_all();
            SendMessageW(window, WM_KEYDOWN, VK_ESCAPE as usize, 0);
            assert!(IsWindow(window) != 0, "Esc first clears the selection");
            assert!(!(*pin).text.as_ref().unwrap().has_selection());
            SendMessageW(window, WM_KEYDOWN, VK_ESCAPE as usize, 0);
            assert!(IsWindow(window) == 0, "then closes the pin");
        }
    }
}
