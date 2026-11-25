//! The text recognition window: pick 快速 (Windows OCR) or 准确 (PaddleOCR)
//! and download the accurate engine's pack, with progress. Picking 准确
//! before the pack is there starts the download right away.

use std::{
    cell::{Cell, RefCell},
    io,
    mem::size_of,
    ptr::{null, null_mut},
    sync::atomic::{AtomicBool, Ordering},
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::WM_MOUSELEAVE,
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_RETURN},
            WindowsAndMessaging::*,
        },
    },
};

use crate::{
    gdiplus::Graphics,
    icons::{self, Icon},
    ocr_pack::{self, Engine},
    selection_geometry::{contains, mouse_point},
    theme::{self, argb, mix},
    ui::{self, Fonts},
};

const CLASS: *const u16 = w!("Snapshot.OcrSettings");
const WIDTH: i32 = 480;
const PAD: i32 = 24;
const HEADER: i32 = 52;
const CARD: i32 = 76;
const CARD_GAP: i32 = 10;
const FOOTER: i32 = 60;
const GAP: i32 = 16;
const WRAP: u32 = DT_WORDBREAK | DT_EDITCONTROL;
/// Posted by the download thread: `wparam` is bytes done, `lparam` the total.
const PROGRESS: u32 = WM_APP + 1;
/// Posted when the download ends; `lparam` owns a `Box<Result<(), String>>`.
const DONE: u32 = WM_APP + 2;
const ENGINES: [Engine; 2] = [Engine::Fast, Engine::Accurate];

static DOWNLOADING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Engine(usize),
    Close,
    Download,
    Cancel,
    Save,
}

struct State {
    engine: Engine,
    installed: bool,
    /// Bytes done and total while downloading.
    progress: Option<(u64, u64)>,
    error: Option<String>,
    hovered: Option<Target>,
    pressed: Option<Target>,
    scale: f32,
    fonts: Fonts,
    tracking: bool,
}

thread_local! {
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn title(engine: Engine) -> &'static str {
    match engine {
        Engine::Fast => "快速",
        Engine::Accurate => "准确",
    }
}

fn description(engine: Engine) -> &'static str {
    match engine {
        Engine::Fast => "Windows 自带的识别引擎，立即可用、速度快；小字、反色文字和代码名容易认错。",
        Engine::Accurate => "PaddleOCR（PP-OCRv5），中英混排、代码名和小字明显更准，每次多花几秒；需要先下载识别包。",
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

/// The line under the cards, and whether it is an error.
fn status(state: &State) -> (String, bool) {
    if let Some(error) = &state.error {
        return (error.clone(), true);
    }
    let text = match state.progress {
        Some((done, total)) => format!(
            "正在下载识别包 {}%（{} / {}）",
            done * 100 / total.max(1),
            megabytes(done),
            megabytes(total)
        ),
        None if state.installed => "识别包已就绪，全程离线识别。".into(),
        None => format!("识别包还没下载（约 {}），只有「准确」档需要。", megabytes(ocr_pack::total_size())),
    };
    (text, false)
}

struct Layout {
    scale: f32,
    width: i32,
    height: i32,
    close: RECT,
    cards: [RECT; 2],
    status: RECT,
    footer: i32,
    download: RECT,
    cancel: RECT,
    save: RECT,
}

impl Layout {
    unsafe fn new(state: &State) -> Self {
        let scale = state.scale;
        let s = |value: i32| (value as f32 * scale).round() as i32;
        let dc = GetDC(null_mut());
        let status_height = ui::wrapped_height(dc, &status(state).0, state.fonts.small, s(WIDTH - PAD * 2), WRAP);
        ReleaseDC(null_mut(), dc);
        let close = RECT { left: s(WIDTH - 12 - 36), top: s(10), right: s(WIDTH - 12), bottom: s(46) };
        let mut y = s(HEADER);
        let mut cards = [RECT::default(); 2];
        for card in &mut cards {
            *card = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + s(CARD) };
            y += s(CARD + CARD_GAP);
        }
        y += s(GAP - CARD_GAP);
        let status = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + status_height };
        y += status_height + s(GAP);
        let footer = y;
        let button_top = y + (s(FOOTER) - s(34)) / 2;
        let button = |left: i32, width: i32| RECT { left, top: button_top, right: left + width, bottom: button_top + s(34) };
        let save = button(s(WIDTH - PAD - 84), s(84));
        let cancel = button(save.left - s(10 + 84), s(84));
        let download = button(s(PAD), s(112));
        Self { scale, width: s(WIDTH), height: y + s(FOOTER), close, cards, status, footer, download, cancel, save }
    }

    fn s(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    fn hit(&self, point: POINT, state: &State) -> Option<Target> {
        let download = (!state.installed).then_some((Target::Download, self.download));
        [(Target::Engine(0), self.cards[0]), (Target::Engine(1), self.cards[1]), (Target::Close, self.close)]
            .into_iter()
            .chain(download)
            .chain([(Target::Cancel, self.cancel), (Target::Save, self.save)])
            .find(|(_, rect)| contains(*rect, point))
            .map(|(target, _)| target)
    }
}

pub unsafe fn register() -> io::Result<()> {
    let class = WNDCLASSW {
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(window_proc),
        hInstance: GetModuleHandleW(null()),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        lpszClassName: CLASS,
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Opens the window, or brings it forward if it is already open.
pub unsafe fn open(near: HWND) -> io::Result<()> {
    let existing = WINDOW.get();
    if !existing.is_null() {
        SetForegroundWindow(existing);
        return Ok(());
    }
    let window = CreateWindowExW(
        WS_EX_TOPMOST,
        CLASS,
        w!("just-snapshot 文字识别"),
        WS_POPUP | WS_SYSMENU,
        0,
        0,
        WIDTH,
        WIDTH,
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
    STATE.set(Some(State {
        engine: ocr_pack::engine(),
        installed: ocr_pack::installed(),
        progress: None,
        error: None,
        hovered: None,
        pressed: None,
        scale,
        fonts: Fonts::new(scale),
        tracking: false,
    }));
    WINDOW.set(window);
    place(window, near);
    ui::round_corners(window);
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    Ok(())
}

unsafe fn layout() -> Option<Layout> {
    STATE.with_borrow(|state| state.as_ref().map(|state| Layout::new(state)))
}

fn with_state<R>(action: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with_borrow_mut(|state| state.as_mut().map(action))
}

/// Sizes the window to its content; on first show, centers it on `near`'s monitor.
unsafe fn place(window: HWND, near: HWND) {
    let Some(layout) = layout() else { return };
    let mut bounds = RECT::default();
    GetWindowRect(window, &mut bounds);
    if !near.is_null() {
        let mut monitor = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        GetMonitorInfoW(MonitorFromWindow(near, MONITOR_DEFAULTTONEAREST), &mut monitor);
        let work = monitor.rcWork;
        bounds.left = work.left + (work.right - work.left - layout.width) / 2;
        bounds.top = work.top + ((work.bottom - work.top - layout.height) / 2).max(0);
    }
    SetWindowPos(window, null_mut(), bounds.left, bounds.top, layout.width, layout.height, SWP_NOZORDER | SWP_NOACTIVATE);
    InvalidateRect(window, null(), 0);
}

unsafe fn start_download(window: HWND) {
    if DOWNLOADING.swap(true, Ordering::AcqRel) {
        return;
    }
    CANCEL.store(false, Ordering::Release);
    with_state(|state| {
        state.error = None;
        state.progress = Some((0, ocr_pack::total_size()));
    });
    place(window, null_mut());
    let target = window as isize;
    std::thread::spawn(move || {
        let mut last = u64::MAX;
        let result = ocr_pack::install(|done, total| {
            // About two hundred progress messages for the whole pack.
            if done * 200 / total.max(1) != last {
                last = done * 200 / total.max(1);
                unsafe { PostMessageW(target as HWND, PROGRESS, done as WPARAM, total as LPARAM) };
            }
            !CANCEL.load(Ordering::Acquire)
        });
        DOWNLOADING.store(false, Ordering::Release);
        let result = Box::into_raw(Box::new(result));
        unsafe {
            if PostMessageW(target as HWND, DONE, 0, result as LPARAM) == 0 {
                drop(Box::from_raw(result));
            }
        }
    });
}

unsafe fn save(window: HWND) {
    let Some((engine, installed)) = STATE.with_borrow(|state| state.as_ref().map(|state| (state.engine, state.installed))) else {
        return;
    };
    if engine == Engine::Accurate && !installed {
        with_state(|state| state.error = Some("识别包下载完成后才能用「准确」档。".into()));
        place(window, null_mut());
        return;
    }
    match ocr_pack::set_engine(engine) {
        Ok(()) => {
            DestroyWindow(window);
        }
        Err(error) => {
            with_state(|state| state.error = Some(error));
            place(window, null_mut());
        }
    }
}

unsafe fn activate(window: HWND, target: Target) {
    match target {
        Target::Engine(index) => {
            let engine = ENGINES[index];
            let fetch = with_state(|state| {
                state.engine = engine;
                state.error = None;
                engine == Engine::Accurate && !state.installed && state.progress.is_none()
            });
            place(window, null_mut());
            if fetch == Some(true) {
                start_download(window);
            }
        }
        Target::Download => {
            if DOWNLOADING.load(Ordering::Acquire) {
                CANCEL.store(true, Ordering::Release);
            } else {
                start_download(window);
            }
        }
        Target::Close | Target::Cancel => {
            DestroyWindow(window);
        }
        Target::Save => save(window),
    }
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(window, &mut paint);
            paint_window(window, dc);
            EndPaint(window, &paint);
        }
        WM_PRINTCLIENT => paint_window(window, wparam as HDC),
        WM_NCHITTEST => {
            let mut point = mouse_point(lparam);
            ScreenToClient(window, &mut point);
            let over = STATE.with_borrow(|state| {
                state.as_ref().is_some_and(|state| Layout::new(state).hit(point, state).is_some())
            });
            let header = layout().is_some_and(|layout| point.y < layout.s(HEADER));
            return if header && !over { HTCAPTION as LRESULT } else { HTCLIENT as LRESULT };
        }
        WM_MOUSEMOVE => {
            if with_state(|state| !std::mem::replace(&mut state.tracking, true)) == Some(true) {
                let mut track = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: window, dwHoverTime: 0 };
                TrackMouseEvent(&mut track);
            }
            let point = mouse_point(lparam);
            let changed = with_state(|state| {
                let hit = Layout::new(state).hit(point, state);
                std::mem::replace(&mut state.hovered, hit) != hit
            });
            if changed == Some(true) {
                InvalidateRect(window, null(), 0);
            }
        }
        WM_MOUSELEAVE => {
            with_state(|state| {
                state.tracking = false;
                state.hovered = None;
            });
            InvalidateRect(window, null(), 0);
        }
        WM_SETCURSOR if (lparam & 0xffff) as u32 == HTCLIENT && wparam as HWND == window => {
            let clickable = STATE.with_borrow(|state| state.as_ref().is_some_and(|state| state.hovered.is_some()));
            SetCursor(LoadCursorW(null_mut(), if clickable { IDC_HAND } else { IDC_ARROW }));
            return 1;
        }
        WM_LBUTTONDOWN => {
            let point = mouse_point(lparam);
            with_state(|state| state.pressed = Layout::new(state).hit(point, state));
            SetCapture(window);
            InvalidateRect(window, null(), 0);
        }
        WM_LBUTTONUP => {
            let point = mouse_point(lparam);
            let pressed = with_state(|state| state.pressed.take()).flatten();
            ReleaseCapture();
            InvalidateRect(window, null(), 0);
            let hit = STATE.with_borrow(|state| state.as_ref().and_then(|state| Layout::new(state).hit(point, state)));
            if let Some(target) = pressed.filter(|&target| Some(target) == hit) {
                activate(window, target);
            }
        }
        WM_CAPTURECHANGED => {
            with_state(|state| state.pressed = None);
        }
        WM_KEYDOWN if wparam == VK_ESCAPE as usize => {
            DestroyWindow(window);
        }
        WM_KEYDOWN if wparam == VK_RETURN as usize => save(window),
        PROGRESS => {
            with_state(|state| state.progress = Some((wparam as u64, lparam as u64)));
            InvalidateRect(window, null(), 0);
        }
        DONE => {
            let result = *Box::from_raw(lparam as *mut Result<(), String>);
            with_state(|state| {
                state.progress = None;
                state.installed = ocr_pack::installed();
                state.error = match result {
                    Ok(()) => None,
                    Err(error) if error == crate::http::CANCELLED => None,
                    Err(error) => Some(error),
                };
            });
            place(window, null_mut());
        }
        WM_DPICHANGED => {
            let scale = (wparam & 0xffff) as f32 / 96.0;
            with_state(|state| {
                state.scale = scale;
                state.fonts = Fonts::new(scale);
            });
            let suggested = *(lparam as *const RECT);
            SetWindowPos(window, null_mut(), suggested.left, suggested.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            place(window, null_mut());
        }
        WM_CLOSE => {
            DestroyWindow(window);
        }
        WM_DESTROY => {
            // Closing the window stops a download in progress.
            CANCEL.store(true, Ordering::Release);
            STATE.take();
            if WINDOW.get() == window {
                WINDOW.set(null_mut());
            }
        }
        _ => return DefWindowProcW(window, message, wparam, lparam),
    }
    0
}

unsafe fn paint_window(window: HWND, target: HDC) {
    let mut client = RECT::default();
    GetClientRect(window, &mut client);
    let Ok(buffer) = crate::capture::Surface::new(target, client.right, client.bottom) else { return };
    STATE.with_borrow(|state| {
        if let Some(state) = state.as_ref() {
            paint(buffer.dc, client, state);
        }
    });
    BitBlt(target, 0, 0, client.right, client.bottom, buffer.dc, 0, 0, SRCCOPY);
}

unsafe fn paint(dc: HDC, client: RECT, state: &State) {
    let palette = theme::palette();
    let fonts = &state.fonts;
    let layout = Layout::new(state);
    let s = |value: i32| layout.s(value);
    let radius = 8.0 * layout.scale;
    let face = |target: Target, base: u32| match (state.pressed == Some(target), state.hovered == Some(target)) {
        (true, _) => palette.pressed,
        (_, true) => mix(base, palette.hover, 1.0),
        _ => base,
    };
    ui::fill(dc, client, palette.background);
    if let Some(graphics) = Graphics::new(dc) {
        graphics.stroke_round(ui::inset(ui::to_f(client), 0.5), 0.0, argb(palette.card_border, 255), 1.0);
    }
    let single = DT_SINGLELINE | DT_VCENTER;
    ui::text(dc, RECT { left: s(PAD), top: 0, right: layout.close.left, bottom: s(HEADER) }, "文字识别", fonts.title, palette.text, DT_LEFT | single);
    if state.hovered == Some(Target::Close) || state.pressed == Some(Target::Close) {
        if let Some(graphics) = Graphics::new(dc) {
            graphics.fill_round(ui::to_f(layout.close), radius, argb(face(Target::Close, palette.background), 255));
        }
    }
    icons::paint(dc, ui::inflate(layout.close, -s(9)), Icon::Close, palette.secondary);

    for (index, card) in layout.cards.iter().enumerate() {
        let engine = ENGINES[index];
        let selected = state.engine == engine;
        let target = Target::Engine(index);
        if let Some(graphics) = Graphics::new(dc) {
            let fill = if selected { palette.accent_soft } else { face(target, palette.card) };
            graphics.fill_round(ui::to_f(*card), radius, argb(fill, 255));
            let border = if selected { palette.accent } else { palette.key_border };
            graphics.stroke_round(ui::inset(ui::to_f(*card), 0.5), radius, argb(border, 255), if selected { 1.5 } else { 1.0 });
            // A radio dot on the left.
            let center = (card.left + s(22), (card.top + card.bottom) / 2);
            let ring = [center.0 as f32 - 8.0 * layout.scale, center.1 as f32 - 8.0 * layout.scale, center.0 as f32 + 8.0 * layout.scale, center.1 as f32 + 8.0 * layout.scale];
            graphics.stroke_round(ring, 8.0 * layout.scale, argb(if selected { palette.accent } else { palette.secondary }, 255), 1.5);
            if selected {
                graphics.fill_round(ui::inset(ring, 4.0 * layout.scale), 4.0 * layout.scale, argb(palette.accent, 255));
            }
        }
        let text_left = card.left + s(44);
        ui::text(
            dc,
            RECT { left: text_left, top: card.top + s(10), right: card.right - s(14), bottom: card.top + s(32) },
            title(engine),
            fonts.heading,
            if selected { palette.accent } else { palette.text },
            DT_LEFT | single,
        );
        ui::text(
            dc,
            RECT { left: text_left, top: card.top + s(32), right: card.right - s(14), bottom: card.bottom - s(6) },
            description(engine),
            fonts.small,
            palette.secondary,
            DT_LEFT | DT_TOP | WRAP,
        );
    }

    let (text, error) = status(state);
    let color = if error {
        palette.error
    } else if state.installed && state.progress.is_none() {
        palette.success
    } else {
        palette.secondary
    };
    ui::text(dc, layout.status, &text, fonts.small, color, DT_LEFT | DT_TOP | WRAP);
    if let Some((done, total)) = state.progress {
        // A thin bar under the status line.
        let bar = RECT { left: layout.status.left, top: layout.status.bottom + s(4), right: layout.status.right, bottom: layout.status.bottom + s(8) };
        if let Some(graphics) = Graphics::new(dc) {
            graphics.fill_round(ui::to_f(bar), 2.0 * layout.scale, argb(palette.key_border, 255));
            let filled = RECT { right: bar.left + ((bar.right - bar.left) as u64 * done / total.max(1)) as i32, ..bar };
            graphics.fill_round(ui::to_f(filled), 2.0 * layout.scale, argb(palette.accent, 255));
        }
    }

    ui::fill(dc, RECT { left: 1, top: layout.footer, right: layout.width - 1, bottom: layout.footer + 1 }, palette.card_border);
    if !state.installed {
        let label = if state.progress.is_some() { "取消下载" } else { "下载识别包" };
        ui::button(dc, layout.download, label, fonts.body, palette, false, face(Target::Download, palette.card), radius);
    }
    ui::button(dc, layout.cancel, "取消", fonts.body, palette, false, face(Target::Cancel, palette.card), radius);
    let save_face = match (state.pressed == Some(Target::Save), state.hovered == Some(Target::Save)) {
        (true, _) => mix(palette.accent_hover, 0, 0.12),
        (_, true) => palette.accent_hover,
        _ => palette.accent,
    };
    ui::button(dc, layout.save, "保存", fonts.heading, palette, true, save_face, radius);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens the real window and saves pictures of its states.
    /// Run with `<test binary> ocr_settings --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        unsafe {
            let _ = register();
            let folder = std::env::temp_dir().join("snapshot-preview");
            std::fs::create_dir_all(&folder).unwrap();
            for step in 0..3 {
                open(null_mut()).unwrap();
                let window = WINDOW.get();
                ShowWindow(window, SW_SHOW);
                with_state(|state| match step {
                    0 => {
                        state.engine = Engine::Fast;
                        state.installed = false;
                    }
                    1 => {
                        state.engine = Engine::Accurate;
                        state.installed = false;
                        state.progress = Some((15_000_000, ocr_pack::total_size()));
                    }
                    _ => {
                        state.engine = Engine::Accurate;
                        state.installed = true;
                    }
                });
                place(window, null_mut());
                let mut message = MSG::default();
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    DispatchMessageW(&message);
                }
                let mut bounds = RECT::default();
                GetWindowRect(window, &mut bounds);
                let (width, height) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
                let surface = crate::capture::Surface::new(null_mut(), width, height).unwrap();
                SendMessageW(window, WM_PRINTCLIENT, surface.dc as WPARAM, PRF_CLIENT as LPARAM);
                GdiFlush();
                let bounds = RECT { left: 0, top: 0, right: width, bottom: height };
                let mut image = crate::capture::Desktop { bounds, surface }.crop(bounds).unwrap();
                for pixel in image.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                image.save_png(&folder.join(format!("ocr-settings-{step}.png"))).unwrap();
                DestroyWindow(window);
            }
        }
    }
}
