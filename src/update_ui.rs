//! The update card in the bottom-right corner, above the tray: a new version
//! with what it fixes, the download progress, "already up to date", errors,
//! and after a restart what the update brought. It slides up and fades in,
//! leaves with the mirrored motion, never takes the focus and stays above
//! other windows.

use std::{
    cell::{Cell, RefCell},
    mem::size_of,
    ptr::{null, null_mut},
    time::Instant,
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
            Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT},
            Shell::ShellExecuteW,
            WindowsAndMessaging::*,
        },
    },
};

use crate::{
    capture::Surface,
    gdiplus::Graphics,
    selection_geometry::{contains, mouse_point},
    theme::{self, argb, mix},
    ui::{self, Badge, Fonts},
    update::{self, Event, Release, Updated},
};

const CLASS: *const u16 = w!("Snapshot.Update");
const WIDTH: i32 = 372;
const PAD: i32 = 18;
const BADGE: i32 = 30;
const BUTTON_HEIGHT: i32 = 32;
const CLOSE: i32 = 28;
/// Gap to the work area's edges, above the taskbar.
const MARGIN: i32 = 16;
/// Lines of release notes shown before "完整说明" takes over.
const NOTES_LINES: i32 = 9;
/// Where bullet text starts, and the space between items.
const NOTE_INDENT: i32 = 14;
const NOTE_GAP: i32 = 2;
const TIMER: usize = 1;
const FRAME_MS: u32 = 15;
const SHOW_MS: f32 = 240.0;
const HIDE_MS: f32 = 180.0;
/// How far the card travels while it slides in and out.
const TRAVEL: f32 = 22.0;
/// Cards that need no answer leave by themselves unless the mouse is on them.
const LATEST_MS: u128 = 4_000;
const UPDATED_MS: u128 = 20_000;
const WRAP: u32 = DT_WORDBREAK | DT_EDITCONTROL;

#[derive(Clone)]
enum View {
    Available(Release),
    Downloading { release: Release, done: u64, total: u64 },
    Installing(Release),
    Info { title: String, body: String },
    Failed { title: String, body: String, retry: Retry },
    Updated(Updated),
}

#[derive(Clone)]
enum Retry {
    Check,
    Download(Release),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Close,
    Skip,
    Later,
    Update,
    Cancel,
    Retry,
    Notes,
    Ok,
}

struct Button {
    action: Action,
    label: &'static str,
    primary: bool,
    enabled: bool,
}

struct State {
    view: View,
    scale: f32,
    fonts: Fonts,
    /// Shown fraction, 0 hidden to 1 in place; eased when drawn.
    shown: f32,
    leaving: bool,
    last_frame: Instant,
    /// The progress bar glides toward the real value.
    progress: f32,
    hovered: Option<Action>,
    pressed: Option<Action>,
    mouse_inside: bool,
    since: Instant,
    /// What quitting takes along: pinned images, a running recording.
    pins: usize,
    recording: bool,
    tray: HWND,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
}

/// Release notes as items: `(is a bullet, text)`. `- ` and `* ` start
/// bullets; heading marks are dropped.
fn note_items(notes: &str) -> Vec<(bool, String)> {
    notes
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| match line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            Some(item) => (true, item.trim().to_string()),
            None => (false, line.trim_start_matches('#').trim().to_string()),
        })
        .collect()
}

/// Each item's rectangle inside `area` from its top, wrapped with a hanging
/// indent so continued lines align with the text, not the bullet.
unsafe fn note_rects(dc: HDC, items: &[(bool, String)], font: HFONT, area: RECT, indent: i32, gap: i32) -> Vec<RECT> {
    let mut top = area.top;
    items
        .iter()
        .map(|(bullet, text)| {
            let left = if *bullet { area.left + indent } else { area.left };
            let height = ui::wrapped_height(dc, text, font, area.right - left, WRAP);
            let rect = RECT { left, top, right: area.right, bottom: top + height };
            top = rect.bottom + gap;
            rect
        })
        .collect()
}

impl State {
    fn s(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    fn badge(&self) -> (Badge, u32) {
        let palette = theme::palette();
        match self.view {
            View::Available(_) | View::Downloading { .. } | View::Installing(_) => (Badge::Arrow, palette.accent),
            View::Info { .. } | View::Updated(_) => (Badge::Check, palette.success),
            View::Failed { .. } => (Badge::Exclamation, palette.error),
        }
    }

    fn title(&self) -> String {
        match &self.view {
            View::Available(release) => format!("发现新版本 v{}", release.version),
            View::Downloading { release, .. } => format!("正在下载 v{}", release.version),
            View::Installing(release) => format!("正在安装 v{}", release.version),
            View::Info { title, .. } | View::Failed { title, .. } => title.clone(),
            View::Updated(updated) => format!("已更新到 v{}", updated.version),
        }
    }

    fn subtitle(&self) -> String {
        match &self.view {
            View::Available(_) => format!("当前 v{}，这一版修复和改进了：", update::VERSION),
            View::Downloading { done, total, .. } => {
                format!("{:.1} / {:.1} MB", *done as f64 / 1048576.0, *total as f64 / 1048576.0)
            }
            View::Installing(_) => "马上重启，托盘图标会短暂消失".into(),
            View::Info { body, .. } | View::Failed { body, .. } => body.clone(),
            View::Updated(updated) if updated.from.is_empty() => "这一版修复和改进了：".into(),
            View::Updated(updated) => format!("从 v{} 升级，这一版修复和改进了：", updated.from),
        }
    }

    fn notes(&self) -> Option<Vec<(bool, String)>> {
        match &self.view {
            View::Available(release) => Some(note_items(&release.notes)),
            View::Updated(updated) => Some(note_items(&updated.notes)),
            _ => None,
        }
        .filter(|notes| !notes.is_empty())
    }

    /// A line about what restarting closes, when it closes anything.
    fn warning(&self) -> Option<String> {
        if !matches!(self.view, View::Available(_)) {
            return None;
        }
        if self.recording {
            return Some("正在录像，录完再更新".into());
        }
        (self.pins > 0).then(|| format!("更新会重启程序，屏幕上的 {} 张贴图会关闭", self.pins))
    }

    fn buttons(&self, notes_clipped: bool) -> Vec<Button> {
        let button = |action, label, primary| Button { action, label, primary, enabled: true };
        match &self.view {
            View::Available(_) => vec![
                button(Action::Skip, "跳过此版本", false),
                button(Action::Later, "稍后", false),
                Button { enabled: !self.recording, ..button(Action::Update, "立即更新", true) },
            ],
            View::Downloading { .. } => vec![button(Action::Cancel, "取消", false)],
            View::Installing(_) => Vec::new(),
            View::Info { .. } => vec![button(Action::Ok, "好的", true)],
            View::Failed { .. } => vec![button(Action::Close, "关闭", false), button(Action::Retry, "重试", true)],
            View::Updated(_) if notes_clipped => {
                vec![button(Action::Notes, "完整说明", false), button(Action::Ok, "好的", true)]
            }
            View::Updated(_) => vec![button(Action::Ok, "好的", true)],
        }
    }
}

struct Layout {
    width: i32,
    height: i32,
    badge: RECT,
    close: RECT,
    title: RECT,
    subtitle: RECT,
    progress: Option<RECT>,
    notes: Option<RECT>,
    notes_clipped: bool,
    warning: Option<RECT>,
    buttons: Vec<(RECT, Button)>,
}

impl Layout {
    unsafe fn new(dc: HDC, state: &State) -> Self {
        let s = |value: i32| state.s(value);
        let width = s(WIDTH);
        let pad = s(PAD);
        let text_left = pad + s(BADGE) + s(12);
        let text_width = width - text_left - pad;
        let badge = RECT { left: pad, top: pad, right: pad + s(BADGE), bottom: pad + s(BADGE) };
        let close = RECT { left: width - s(10) - s(CLOSE), top: s(10), right: width - s(10), bottom: s(10) + s(CLOSE) };
        let title_height = ui::wrapped_height(dc, &state.title(), state.fonts.heading, width - text_left - s(CLOSE) - s(14), WRAP);
        let title = RECT { left: text_left, top: pad + s(1), right: width - s(CLOSE) - s(14), bottom: pad + s(1) + title_height };
        let mut y = title.bottom + s(3);
        let subtitle_height = ui::wrapped_height(dc, &state.subtitle(), state.fonts.small, text_width, WRAP);
        let subtitle = RECT { left: text_left, top: y, right: text_left + text_width, bottom: y + subtitle_height };
        y = subtitle.bottom.max(badge.bottom);
        let progress = matches!(state.view, View::Downloading { .. } | View::Installing(_)).then(|| {
            y += s(12);
            let bar = RECT { left: text_left, top: y, right: text_left + text_width, bottom: y + s(6) };
            y = bar.bottom;
            bar
        });
        let mut notes_clipped = false;
        let notes = state.notes().map(|items| {
            y += s(10);
            let area = RECT { left: text_left, top: y, right: text_left + text_width, bottom: y };
            let full = note_rects(dc, &items, state.fonts.body, area, s(NOTE_INDENT), s(NOTE_GAP))
                .last()
                .map_or(y, |last| last.bottom);
            let line = ui::wrapped_height(dc, "字", state.fonts.body, text_width, WRAP);
            let shown = (full - y).min(line * NOTES_LINES + s(NOTE_GAP) * (NOTES_LINES - 1));
            notes_clipped = full - y > shown;
            let rect = RECT { bottom: y + shown, ..area };
            y = rect.bottom;
            rect
        });
        let warning = state.warning().map(|warning| {
            y += s(10);
            let height = ui::wrapped_height(dc, &warning, state.fonts.small, text_width, WRAP);
            let rect = RECT { left: text_left, top: y, right: text_left + text_width, bottom: y + height };
            y = rect.bottom;
            rect
        });
        let buttons = state.buttons(notes_clipped);
        let mut placed = Vec::new();
        if !buttons.is_empty() {
            y += s(16);
            let mut right = width - pad;
            for button in buttons.into_iter().rev() {
                let label_width = ui::measure(dc, button.label, state.fonts.body);
                let button_width = (label_width + s(28)).max(s(72));
                placed.push((RECT { left: right - button_width, top: y, right, bottom: y + s(BUTTON_HEIGHT) }, button));
                right -= button_width + s(8);
            }
            placed.reverse();
            y += s(BUTTON_HEIGHT);
        }
        Self {
            width,
            height: y + pad,
            badge,
            close,
            title,
            subtitle,
            progress,
            notes,
            notes_clipped,
            warning,
            buttons: placed,
        }
    }

    fn hit(&self, point: POINT) -> Option<(Action, bool)> {
        if contains(self.close, point) {
            return Some((Action::Close, true));
        }
        self.buttons
            .iter()
            .find(|(rect, _)| contains(*rect, point))
            .map(|(_, button)| (button.action, button.enabled))
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn ease_in(t: f32) -> f32 {
    t * t * t
}

unsafe fn work_area() -> RECT {
    let mut area = RECT::default();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut area as *mut RECT).cast(), 0);
    area
}

/// Where the card rests: the bottom-right corner of the primary work area.
unsafe fn resting(state: &State, layout: &Layout) -> RECT {
    let area = work_area();
    let margin = state.s(MARGIN);
    RECT {
        left: area.right - margin - layout.width,
        top: area.bottom - margin - layout.height,
        right: area.right - margin,
        bottom: area.bottom - margin,
    }
}

unsafe fn measure(state: &State) -> Layout {
    let probe = Surface::new(null_mut(), 1, 1);
    match probe {
        Ok(probe) => Layout::new(probe.dc, state),
        Err(_) => Layout::new(GetDC(null_mut()), state),
    }
}

/// Shows `view`, replacing whatever the card showed; opens the card if needed.
unsafe fn show(tray: HWND, view: View) {
    let window = WINDOW.get();
    if !window.is_null() {
        let resized = STATE.with_borrow_mut(|state| {
            let state = state.as_mut()?;
            state.view = view.clone();
            state.leaving = false;
            state.since = Instant::now();
            state.hovered = None;
            state.pressed = None;
            Some(())
        });
        if resized.is_some() {
            place(window);
            SetTimer(window, TIMER, FRAME_MS, None);
            InvalidateRect(window, null(), 0);
            return;
        }
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
    RegisterClassW(&class);
    let area = work_area();
    // Created in the corner so it picks up that monitor's DPI.
    let window = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
        CLASS,
        w!("just-snapshot 更新"),
        WS_POPUP,
        area.right - 2,
        area.bottom - 2,
        1,
        1,
        null_mut(),
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
    let pins = crate::pinned::groups().iter().map(|(_, count, _)| count).sum();
    STATE.set(Some(State {
        view,
        scale,
        fonts: Fonts::new(scale),
        shown: 0.0,
        leaving: false,
        last_frame: Instant::now(),
        progress: 0.0,
        hovered: None,
        pressed: None,
        mouse_inside: false,
        since: Instant::now(),
        pins,
        recording: crate::recording_ui::is_busy(),
        tray,
    }));
    WINDOW.set(window);
    SetLayeredWindowAttributes(window, 0, 0, LWA_ALPHA);
    ui::round_corners(window);
    place(window);
    ShowWindow(window, SW_SHOWNOACTIVATE);
    SetTimer(window, TIMER, FRAME_MS, None);
}

/// Sizes and moves the card for its current view and animation frame.
unsafe fn place(window: HWND) {
    let Some((rect, alpha)) = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        let layout = measure(state);
        let rest = resting(state, &layout);
        let eased = if state.leaving { ease_in(state.shown) } else { ease_out(state.shown) };
        let offset = ((1.0 - eased) * TRAVEL * state.scale).round() as i32;
        Some((RECT { top: rest.top + offset, bottom: rest.bottom + offset, ..rest }, (eased * 255.0) as u8))
    }) else {
        return;
    };
    SetWindowPos(
        window,
        HWND_TOPMOST,
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
        SWP_NOACTIVATE,
    );
    SetLayeredWindowAttributes(window, 0, alpha, LWA_ALPHA);
}

unsafe fn leave(window: HWND) {
    STATE.with_borrow_mut(|state| {
        if let Some(state) = state.as_mut() {
            state.leaving = true;
            state.last_frame = Instant::now();
        }
    });
    SetTimer(window, TIMER, FRAME_MS, None);
}

/// One animation frame; returns whether more are needed.
unsafe fn tick(window: HWND) -> bool {
    enum Next {
        Continue,
        Idle,
        Destroy,
        Leave,
    }
    let next = STATE.with_borrow_mut(|state| {
        let Some(state) = state.as_mut() else {
            return Next::Destroy;
        };
        let now = Instant::now();
        let elapsed = now.duration_since(state.last_frame).as_secs_f32() * 1000.0;
        state.last_frame = now;
        if state.leaving {
            state.shown = (state.shown - elapsed / HIDE_MS).max(0.0);
            if state.shown == 0.0 {
                return Next::Destroy;
            }
        } else {
            state.shown = (state.shown + elapsed / SHOW_MS).min(1.0);
        }
        let goal = match state.view {
            View::Downloading { done, total, .. } => done as f32 / total.max(1) as f32,
            View::Installing(_) => 1.0,
            _ => 0.0,
        };
        let gliding = (goal - state.progress).abs() > 0.001;
        state.progress += (goal - state.progress) * 0.25;
        let timeout = match state.view {
            View::Info { .. } => Some(LATEST_MS),
            View::Updated(_) => Some(UPDATED_MS),
            _ => None,
        };
        if !state.leaving && !state.mouse_inside && timeout.is_some_and(|ms| state.since.elapsed().as_millis() > ms) {
            return Next::Leave;
        }
        // Timed cards keep ticking to notice their timeout.
        if state.leaving || state.shown < 1.0 || gliding || timeout.is_some() {
            Next::Continue
        } else {
            Next::Idle
        }
    });
    match next {
        Next::Destroy => {
            DestroyWindow(window);
            false
        }
        Next::Leave => {
            leave(window);
            true
        }
        Next::Continue | Next::Idle => {
            place(window);
            InvalidateRect(window, null(), 0);
            matches!(next, Next::Continue)
        }
    }
}

unsafe fn act(window: HWND, action: Action) {
    let Some((view, tray)) = STATE.with_borrow(|state| state.as_ref().map(|state| (state.view.clone(), state.tray))) else {
        return;
    };
    match (action, view) {
        (Action::Close | Action::Cancel, View::Downloading { .. }) => update::cancel_download(),
        (_, View::Downloading { .. } | View::Installing(_)) => {}
        (Action::Close | Action::Later | Action::Ok, _) => leave(window),
        (Action::Skip, View::Available(release)) => {
            if let Err(error) = update::skip(&release.version) {
                show(tray, View::Failed { title: "没能记住跳过的版本".into(), body: error, retry: Retry::Check });
                return;
            }
            leave(window);
        }
        (Action::Update, View::Available(release)) | (Action::Retry, View::Failed { retry: Retry::Download(release), .. }) => {
            STATE.with_borrow_mut(|state| {
                if let Some(state) = state.as_mut() {
                    state.progress = 0.0;
                }
            });
            show(tray, View::Downloading { release: release.clone(), done: 0, total: release.size });
            update::download(release);
        }
        (Action::Retry, View::Failed { retry: Retry::Check, .. }) => {
            show(tray, View::Info { title: "正在检查更新…".into(), body: format!("当前版本 v{}", update::VERSION) });
            update::check(true);
        }
        (Action::Notes, View::Updated(updated)) => open_page(&format!("{}/releases/tag/v{}", update::REPOSITORY, updated.version)),
        _ => {}
    }
}

unsafe fn open_page(url: &str) {
    let url = crate::wide(url);
    ShellExecuteW(null_mut(), w!("open"), url.as_ptr(), null(), null(), SW_SHOWNORMAL);
}

/// Handles update events posted to the tray window.
pub unsafe fn handle(tray: HWND) {
    for event in update::take_events() {
        match event {
            Event::Checked { manual, result } => match result {
                Ok(Some(release)) => show(tray, View::Available(release)),
                Ok(None) if manual => show(
                    tray,
                    View::Info { title: "已是最新版本".into(), body: format!("当前版本 v{}", update::VERSION) },
                ),
                Err(error) if manual => {
                    let (title, body) = error.split_once('\n').unwrap_or((error.as_str(), ""));
                    show(tray, View::Failed { title: title.trim_end_matches('。').into(), body: body.into(), retry: Retry::Check });
                }
                // Automatic checks stay quiet when there is nothing to do.
                _ => {}
            },
            Event::Progress { done, total } => {
                let window = WINDOW.get();
                let updated = STATE.with_borrow_mut(|state| {
                    let state = state.as_mut()?;
                    if let View::Downloading { done: shown, total: known, .. } = &mut state.view {
                        *shown = done;
                        *known = total;
                        return Some(());
                    }
                    None
                });
                if updated.is_some() && !window.is_null() {
                    SetTimer(window, TIMER, FRAME_MS, None);
                    InvalidateRect(window, null(), 0);
                }
            }
            Event::Downloaded(result) => {
                let release = STATE.with_borrow(|state| match &state.as_ref()?.view {
                    View::Downloading { release, .. } => Some(release.clone()),
                    _ => None,
                });
                let Some(release) = release else { continue };
                match result {
                    Ok(path) => {
                        show(tray, View::Installing(release.clone()));
                        // Let the card show "installing" before the app goes away.
                        UpdateWindow(WINDOW.get());
                        match update::install(&path, &release) {
                            Ok(()) => {
                                PostMessageW(tray, WM_CLOSE, 0, 0);
                            }
                            Err(error) => show(
                                tray,
                                View::Failed { title: "更新没有完成，旧版本照常可用".into(), body: error, retry: Retry::Download(release) },
                            ),
                        }
                    }
                    Err(error) if error == crate::http::CANCELLED => show(tray, View::Available(release)),
                    Err(error) => {
                        let (title, body) = error.split_once('\n').unwrap_or((error.as_str(), ""));
                        show(
                            tray,
                            View::Failed { title: title.trim_end_matches('。').into(), body: body.into(), retry: Retry::Download(release) },
                        );
                    }
                }
            }
        }
    }
}

/// A manual check from the tray menu: shows progress right away.
pub unsafe fn check_now(tray: HWND) {
    if update::is_dev_build() {
        show(
            tray,
            View::Info {
                title: "开发版不检查更新".into(),
                body: "这个程序是从编译输出的 target 文件夹运行的，更新会覆盖刚编译好的程序。".into(),
            },
        );
        return;
    }
    show(tray, View::Info { title: "正在检查更新…".into(), body: format!("当前版本 v{}", update::VERSION) });
    // The card would leave by itself before a slow check returns.
    STATE.with_borrow_mut(|state| {
        if let Some(state) = state.as_mut() {
            state.since = Instant::now() + std::time::Duration::from_secs(60);
        }
    });
    update::check(true);
}

/// After a restart into a new version: what it brought.
pub unsafe fn show_updated(tray: HWND, updated: Updated) {
    show(tray, View::Updated(updated));
}

unsafe fn update_hover(window: HWND, point: Option<POINT>) {
    let changed = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        let hovered = point.and_then(|point| {
            let layout = measure(state);
            layout.hit(point).filter(|(_, enabled)| *enabled).map(|(action, _)| action)
        });
        let entering = point.is_some() && !state.mouse_inside;
        state.mouse_inside = point.is_some();
        if point.is_none() {
            state.pressed = None;
            // Give a timed card its full time again after the mouse leaves.
            if matches!(state.view, View::Info { .. } | View::Updated(_)) && state.since.elapsed().as_millis() > 1_000 {
                state.since = Instant::now();
            }
        }
        let changed = hovered != state.hovered;
        state.hovered = hovered;
        Some((changed, entering, hovered.is_some()))
    });
    if let Some((changed, entering, on_button)) = changed {
        if entering {
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: window,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut track);
        }
        if changed {
            InvalidateRect(window, null(), 0);
        }
        if point.is_some() {
            SetCursor(LoadCursorW(null_mut(), if on_button { IDC_HAND } else { IDC_ARROW }));
        }
    }
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(window, &mut paint);
            paint_window(window, dc);
            EndPaint(window, &paint);
        }
        WM_TIMER if wparam == TIMER => {
            if !tick(window) {
                KillTimer(window, TIMER);
            }
        }
        WM_MOUSEMOVE => update_hover(window, Some(mouse_point(lparam))),
        WM_MOUSELEAVE => update_hover(window, None),
        WM_LBUTTONDOWN => {
            STATE.with_borrow_mut(|state| {
                if let Some(state) = state.as_mut() {
                    state.pressed = state.hovered;
                }
            });
            InvalidateRect(window, null(), 0);
        }
        WM_LBUTTONUP => {
            let action = STATE.with_borrow_mut(|state| {
                let state = state.as_mut()?;
                let pressed = state.pressed.take()?;
                let layout = measure(state);
                layout
                    .hit(mouse_point(lparam))
                    .filter(|&(action, enabled)| enabled && action == pressed)
                    .map(|(action, _)| action)
            });
            InvalidateRect(window, null(), 0);
            if let Some(action) = action {
                act(window, action);
            }
        }
        WM_DESTROY => {
            KillTimer(window, TIMER);
            STATE.take();
            WINDOW.set(null_mut());
        }
        _ => return DefWindowProcW(window, message, wparam, lparam),
    }
    0
}

unsafe fn paint_window(window: HWND, target: HDC) {
    let mut client = RECT::default();
    GetClientRect(window, &mut client);
    let Ok(surface) = Surface::new(target, client.right, client.bottom) else {
        return;
    };
    STATE.with_borrow(|state| {
        if let Some(state) = state {
            paint(surface.dc, client, state);
        }
    });
    BitBlt(target, 0, 0, client.right, client.bottom, surface.dc, 0, 0, SRCCOPY);
}

unsafe fn paint(dc: HDC, client: RECT, state: &State) {
    let palette = theme::palette();
    let layout = Layout::new(dc, state);
    ui::fill(dc, client, palette.card);
    if let Some(graphics) = Graphics::new(dc) {
        // Hairline border; DWM adds the rounded corners and shadow.
        graphics.stroke_round(ui::inset(ui::to_f(client), 0.5), 0.0, argb(palette.card_border, 255), 1.0);
    }
    let (badge, color) = state.badge();
    ui::badge(dc, layout.badge, color, badge, palette.on_accent);
    ui::text(dc, layout.title, &state.title(), state.fonts.heading, palette.text, DT_LEFT | DT_TOP | WRAP);
    ui::text(dc, layout.subtitle, &state.subtitle(), state.fonts.small, palette.secondary, DT_LEFT | DT_TOP | WRAP);

    // Close: a quiet × that lights up under the mouse.
    let close_hover = state.hovered == Some(Action::Close);
    if let Some(graphics) = Graphics::new(dc) {
        if close_hover {
            let face = if state.pressed == Some(Action::Close) { palette.pressed } else { palette.hover };
            graphics.fill_round(ui::to_f(layout.close), state.scale * 6.0, argb(face, 255));
        }
        let [left, top, right, bottom] = ui::to_f(layout.close);
        let (cx, cy) = ((left + right) / 2.0, (top + bottom) / 2.0);
        let half = 4.5 * state.scale;
        let color = argb(if close_hover { palette.text } else { palette.tertiary }, 255);
        graphics.line((cx - half, cy - half), (cx + half, cy + half), color, 1.4 * state.scale);
        graphics.line((cx - half, cy + half), (cx + half, cy - half), color, 1.4 * state.scale);
    }

    if let (Some(bar), Some(graphics)) = (layout.progress, Graphics::new(dc)) {
        let bounds = ui::to_f(bar);
        let radius = (bounds[3] - bounds[1]) / 2.0;
        graphics.fill_round(bounds, radius, argb(palette.track_off, 255));
        let filled = bounds[0] + (bounds[2] - bounds[0]) * state.progress.clamp(0.0, 1.0);
        if filled > bounds[0] + radius {
            graphics.fill_round([bounds[0], bounds[1], filled, bounds[3]], radius, argb(palette.accent, 255));
        }
    }

    if let (Some(rect), Some(notes)) = (layout.notes, state.notes()) {
        let saved = SaveDC(dc);
        IntersectClipRect(dc, rect.left, rect.top, rect.right, rect.bottom);
        let items = note_rects(dc, &notes, state.fonts.body, rect, state.s(NOTE_INDENT), state.s(NOTE_GAP));
        for ((bullet, text), item) in notes.iter().zip(items) {
            if *bullet {
                let mark = RECT { left: rect.left, right: item.left, ..item };
                ui::text(dc, mark, "•", state.fonts.body, palette.secondary, DT_LEFT | DT_TOP | DT_SINGLELINE);
            }
            ui::text(dc, item, text, state.fonts.body, palette.text, DT_LEFT | DT_TOP | WRAP);
        }
        RestoreDC(dc, saved);
        // Fade the last lines into the card instead of cutting a line in half.
        if let (true, Some(graphics)) = (layout.notes_clipped, Graphics::new(dc)) {
            let fade = state.s(28);
            for step in 0..fade {
                let row = (rect.bottom - fade + step) as f32;
                let alpha = (step + 1) as f32 / fade as f32;
                graphics.fill_round(
                    [rect.left as f32, row, rect.right as f32, row + 1.0],
                    0.0,
                    argb(palette.card, (alpha * 255.0) as u8),
                );
            }
        }
    }

    if let Some(rect) = layout.warning {
        let color = if state.recording { palette.warning } else { palette.secondary };
        ui::text(dc, rect, &state.warning().unwrap_or_default(), state.fonts.small, color, DT_LEFT | DT_TOP | WRAP);
    }

    let radius = 8.0 * state.scale;
    for (rect, button) in &layout.buttons {
        let (base, hover) = if button.primary { (palette.accent, palette.accent_hover) } else { (palette.card, palette.hover) };
        let face = if !button.enabled {
            palette.track_off
        } else if state.pressed == Some(button.action) && state.hovered == Some(button.action) {
            mix(hover, 0, 0.14)
        } else if state.hovered == Some(button.action) {
            hover
        } else {
            base
        };
        ui::button(dc, *rect, button.label, state.fonts.body, palette, button.primary && button.enabled, face, radius);
    }
}

/// Renders every card state without a window, for the preview test.
#[cfg(test)]
pub unsafe fn preview(scale: f32) -> Vec<(&'static str, crate::capture::Image)> {
    use crate::capture::Desktop;
    let release = Release {
        version: "0.5.0".into(),
        notes: "- 长截图支持半透明、带渐变背景的聊天窗口（如 QQ、网页版聊天）\n- 长截图里悬浮的按钮和工具条只保留一份\n- 复制长截图不再卡顿，快了约 20 倍\n- 保存图片改在后台进行，截图窗口立即关闭\n- 新增自动更新：开机后检查新版本，可在设置里关闭".into(),
        url: String::new(),
        sha256: String::new(),
        size: 6_291_456,
    };
    let long_notes = (1..=14).map(|index| format!("- 第 {index} 条改进，说明文字稍微长一点看看换行")).collect::<Vec<_>>().join("\n");
    let views = [
        ("available", View::Available(release.clone()), 0, false),
        ("available-pins", View::Available(release.clone()), 3, false),
        ("available-recording", View::Available(release.clone()), 0, true),
        ("downloading", View::Downloading { release: release.clone(), done: 2_621_440, total: 6_291_456 }, 0, false),
        ("latest", View::Info { title: "已是最新版本".into(), body: "当前版本 v0.4.1".into() }, 0, false),
        (
            "failed",
            View::Failed {
                title: "检查更新失败".into(),
                body: "GitHub：连不上服务器，请检查网络或代理（发送请求，错误码 12029）".into(),
                retry: Retry::Check,
            },
            0,
            false,
        ),
        ("updated", View::Updated(Updated { from: "0.4.1".into(), version: "0.5.0".into(), notes: release.notes.clone() }), 0, false),
        ("updated-long", View::Updated(Updated { from: "0.4.1".into(), version: "0.5.0".into(), notes: long_notes }), 0, false),
    ];
    views
        .into_iter()
        .map(|(name, view, pins, recording)| {
            let progress = if matches!(view, View::Downloading { .. }) { 0.42 } else { 0.0 };
            let state = State {
                view,
                scale,
                fonts: Fonts::new(scale),
                shown: 1.0,
                leaving: false,
                last_frame: Instant::now(),
                progress,
                hovered: (name == "available").then_some(Action::Update),
                pressed: None,
                mouse_inside: false,
                since: Instant::now(),
                pins,
                recording,
                tray: null_mut(),
            };
            let layout = measure(&state);
            let bounds = RECT { right: layout.width, bottom: layout.height, ..Default::default() };
            let surface = Surface::new(null_mut(), bounds.right, bounds.bottom).unwrap();
            paint(surface.dc, bounds, &state);
            GdiFlush();
            let mut image = Desktop { bounds, surface }.crop(bounds).unwrap();
            for pixel in image.pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            (name, image)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_become_bullets_and_headings_lose_their_marks() {
        assert_eq!(
            note_items("## v0.5.0\n- 修好了\n\n* 也修好了\n普通一行"),
            vec![
                (false, "v0.5.0".to_string()),
                (true, "修好了".to_string()),
                (true, "也修好了".to_string()),
                (false, "普通一行".to_string()),
            ]
        );
    }

    #[test]
    fn eases_start_and_end_in_place() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert_eq!(ease_in(1.0), 1.0);
        assert!(ease_out(0.5) > 0.5 && ease_in(0.5) < 0.5, "out is fast first, in is slow first");
    }

    #[test]
    #[ignore]
    fn preview_cards() {
        let folder = std::env::temp_dir().join("snapshot-preview");
        std::fs::create_dir_all(&folder).unwrap();
        for scale in [1.0, 1.5] {
            for (name, image) in unsafe { preview(scale) } {
                image.save_png(&folder.join(format!("update-{name}-{}.png", (scale * 100.0) as i32))).unwrap();
            }
        }
    }
}
