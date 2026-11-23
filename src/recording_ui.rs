//! Everything on screen while recording: a red frame just outside the area
//! and a small control bar (time, pause, stop, cancel) that turns into a
//! result card when the file is saved (open, show in folder, make a GIF).
//! Both are excluded from screen capture, so they never end up in the video.
//! Recording options live in `%LOCALAPPDATA%\just-snapshot\recording.txt`.

use std::{
    cell::RefCell,
    env, fs, io,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
        Graphics::Gdi::*,
        UI::{
            HiDpi::GetDpiForWindow,
            Shell::ShellExecuteW,
            WindowsAndMessaging::*,
        },
    },
};

use crate::{
    capture::{Image, Surface},
    gdiplus::rasterize_rect,
    icons::{self, Icon},
    output_settings::{unique_path, OutputConfig},
    recorder::{self, Command, Options, Recording, Summary},
    selection_geometry::{contains, mouse_point},
    show_error,
    theme::{self, argb},
    wide,
};

const FRAME_CLASS: *const u16 = w!("Snapshot.RecordFrame");
const BAR_CLASS: *const u16 = w!("Snapshot.RecordBar");
const TICK_TIMER: usize = 1;
const CLOSE_TIMER: usize = 2;
const COUNT_TIMER: usize = 3;
/// Seconds counted down before an area recording, when that is switched on.
const COUNTDOWN: u64 = 3;
/// Posted when the recording file is finished; `lparam` owns a `Box<Result<Summary, String>>`.
const SAVED: u32 = WM_APP + 1;
/// Posted when the recorder has its first frame ready; `lparam` owns a `Box<Result<Recording, String>>`.
const STARTED: u32 = WM_APP + 4;
/// GIF progress: `wparam` is per mille.
const GIF_PROGRESS: u32 = WM_APP + 2;
/// GIF done; `lparam` owns a `Box<Result<PathBuf, String>>`.
const GIF_DONE: u32 = WM_APP + 3;
/// The result card closes by itself after this long untouched.
const CARD_MS: u32 = 15_000;
const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;
const RED: u32 = 0x0040_40e8;
const AMBER: u32 = 0x0020_a0f0;

// ---------------------------------------------------------------------------
// Options.

fn config_path() -> Option<PathBuf> {
    Some(crate::migrate::app_data().ok()?.join("recording.txt"))
}

pub fn load_options() -> Options {
    config_path().and_then(|path| fs::read_to_string(path).ok()).map_or_else(Options::default, |text| parse_options(&text))
}

fn parse_options(text: &str) -> Options {
    let mut options = Options::default();
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        match name.trim() {
            "fps" => options.fps = value.parse::<u32>().map_or(60, |fps| if fps >= 45 { 60 } else { 30 }),
            "quality" => {
                options.quality = crate::recorder::Quality::ALL.into_iter().find(|quality| quality.key() == value).unwrap_or_default()
            }
            "system_audio" => options.system_audio = value == "1",
            "microphone" => options.microphone = value == "1",
            "cursor" => options.cursor = value == "1",
            "countdown" => options.countdown = value == "1",
            "show_keys" => options.show_keys = value == "1",
            "show_pad" => options.show_pad = value == "1",
            _ => {}
        }
    }
    options
}

pub fn save_options(options: Options) -> Result<(), String> {
    let path = config_path().ok_or("找不到 LOCALAPPDATA。")?;
    let flag = |value: bool| u8::from(value);
    let folder = load_folder();
    let text = format!(
        "folder={folder}\r\nfps={}\r\nquality={}\r\nsystem_audio={}\r\nmicrophone={}\r\ncursor={}\r\ncountdown={}\r\nshow_keys={}\r\nshow_pad={}\r\n",
        options.fps,
        options.quality.key(),
        flag(options.system_audio),
        flag(options.microphone),
        flag(options.cursor),
        flag(options.countdown),
        flag(options.show_keys),
        flag(options.show_pad)
    );
    fs::create_dir_all(path.parent().expect("settings directory")).map_err(|error| error.to_string())?;
    fs::write(&path, text).map_err(|error| format!("无法保存录像设置。\n{error}"))
}

/// The folder recordings go to as saved; empty means the default.
pub fn load_folder() -> String {
    config_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| text.lines().find_map(|line| line.strip_prefix("folder=").map(|folder| folder.trim().to_string())))
        .unwrap_or_default()
}

/// Saves the recording folder next to the options; empty means the default.
pub fn save_folder(folder: &str) -> Result<(), String> {
    let path = config_path().ok_or("找不到 LOCALAPPDATA。")?;
    let text = fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().filter(|line| !line.starts_with("folder=")).map(str::to_string).collect();
    lines.insert(0, format!("folder={}", folder.trim()));
    fs::create_dir_all(path.parent().expect("settings directory")).map_err(|error| error.to_string())?;
    fs::write(&path, lines.join("\r\n") + "\r\n").map_err(|error| format!("无法保存录像设置。\n{error}"))
}

/// Where recordings go: the chosen folder, or a Videos folder next to the program.
pub fn folder() -> PathBuf {
    let chosen = load_folder();
    if !chosen.is_empty() {
        return PathBuf::from(chosen);
    }
    default_folder()
}

/// `Videos` beside snapshot.exe, so recordings stay on the drive the program
/// was put on instead of filling up C:.
fn default_folder() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|folder| folder.join("Videos")))
        .unwrap_or_else(|| env::temp_dir().join(crate::migrate::NAME))
}

// ---------------------------------------------------------------------------
// State.

enum Phase {
    /// Counting down to the start; nothing is being recorded yet.
    Countdown { until: Instant },
    /// The frame and bar are up; the recorder is opening the encoder and devices.
    /// `stop` holds a stop pressed meanwhile (`Some(true)` to throw it away).
    Starting { stop: Option<bool> },
    Recording { paused_since: Option<Instant> },
    Saving,
    Saved(Summary),
    Converting { progress: u32 },
    Converted(PathBuf),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Button {
    Pause,
    Stop,
    Cancel,
    Open,
    Folder,
    Gif,
    Close,
}

/// What is being recorded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// An area picked on screen: red frame around it, bar under it.
    Area,
    /// The whole screen of the active window (a game): no frame, a small bar in its corner.
    Screen,
}

struct State {
    recording: Option<Recording>,
    kind: Kind,
    area: RECT,
    /// Where the file goes and how, kept for a countdown that starts the recorder later.
    path: PathBuf,
    options: Options,
    /// The number the countdown shows, so the frame is redrawn only when it changes.
    count: u64,
    frame: HWND,
    bar: HWND,
    phase: Phase,
    started: Instant,
    paused_total: Duration,
    hovered: Option<Button>,
    scale: f32,
    drag: Option<POINT>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub unsafe fn register(instance: HINSTANCE) -> io::Result<()> {
    for (class, proc, cursor) in [
        (FRAME_CLASS, frame_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT, IDC_ARROW),
        (BAR_CLASS, bar_proc, IDC_ARROW),
    ] {
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), cursor),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&window_class) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A recording is starting, running, being saved or turned into a GIF. A
/// result card that is only showing does not count: a new recording replaces it.
pub fn is_busy() -> bool {
    STATE.with_borrow(|state| {
        state.as_ref().is_some_and(|state| {
            matches!(
                state.phase,
                Phase::Countdown { .. } | Phase::Starting { .. } | Phase::Recording { .. } | Phase::Saving | Phase::Converting { .. }
            )
        })
    })
}

/// A recording is counting down, starting or running, so the record shortcut means stop.
pub fn is_recording() -> bool {
    STATE.with_borrow(|state| {
        state.as_ref().is_some_and(|state| matches!(state.phase, Phase::Countdown { .. } | Phase::Starting { .. } | Phase::Recording { .. }))
    })
}

/// The file a new recording goes to: the recording folder, named by the file name template, as MP4.
fn next_path(width: i32, height: i32) -> Result<PathBuf, String> {
    let config = OutputConfig::load().unwrap_or_default();
    let folder = folder();
    fs::create_dir_all(&folder)
        .map_err(|error| format!("无法创建保存位置 {}，可以在设置「录像与按键显示」里换一个位置。\n{error}", folder.display()))?;
    let name = config.file_name(width, height);
    let name = name.strip_suffix(".png").unwrap_or(&name).to_string() + ".mp4";
    Ok(unique_path(&folder, &name))
}

/// Records the whole screen the active window is on, like a game recorder:
/// no area to pick, no countdown, the bar tucked into the screen's corner.
pub unsafe fn start_screen(owner: HWND) {
    let mut foreground = GetForegroundWindow();
    if foreground.is_null() {
        foreground = owner;
    }
    let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTOPRIMARY);
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    if GetMonitorInfoW(monitor, &mut info) == 0 {
        show_error(owner, "无法开始录像。\n找不到当前屏幕。");
        return;
    }
    start(owner, info.rcMonitor, load_options(), Kind::Screen);
}

/// Starts recording `area` (screen coordinates), shown with a frame and the
/// control bar. They appear at once; the encoder and sound devices open on a
/// helper thread, which can take a moment, and the clock starts when they are
/// ready. An area recording with the countdown on waits three seconds first,
/// and shows the key / pad panels picked on its toolbar inside the area.
pub unsafe fn start(owner: HWND, area: RECT, options: Options, kind: Kind) {
    if is_busy() {
        return;
    }
    close_windows();
    let path = match next_path(area.right - area.left, area.bottom - area.top) {
        Ok(path) => path,
        Err(error) => {
            show_error(owner, &format!("无法开始录像。\n{error}"));
            return;
        }
    };
    let instance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null());
    let style = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    let frame = CreateWindowExW(style | WS_EX_TRANSPARENT, FRAME_CLASS, w!("just-snapshot 录像区域"), WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), instance, null());
    let bar = CreateWindowExW(style, BAR_CLASS, w!("just-snapshot 录像"), WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), instance, null());
    for window in [frame, bar] {
        // Never in the video, and never in screenshots of it either.
        SetWindowDisplayAffinity(window, WDA_EXCLUDEFROMCAPTURE);
    }
    let scale = match GetDpiForWindow(bar) {
        0 => 1.0,
        dpi => dpi as f32 / 96.0,
    };
    if kind == Kind::Area && (options.show_keys || options.show_pad) {
        crate::input_overlay::show_in(area, [options.show_keys, options.show_pad]);
    }
    let countdown = kind == Kind::Area && options.countdown;
    let phase = if countdown {
        Phase::Countdown { until: Instant::now() + Duration::from_secs(COUNTDOWN) }
    } else {
        Phase::Starting { stop: None }
    };
    STATE.set(Some(State {
        recording: None,
        kind,
        area,
        path: path.clone(),
        options,
        count: COUNTDOWN,
        frame,
        bar,
        phase,
        started: Instant::now(),
        paused_total: Duration::ZERO,
        hovered: None,
        scale,
        drag: None,
    }));
    draw_frame();
    draw_bar();
    if kind == Kind::Area {
        ShowWindow(frame, SW_SHOWNOACTIVATE);
    }
    ShowWindow(bar, SW_SHOWNOACTIVATE);
    // The pause key is taken from here on, until the recording ends.
    crate::refresh_shortcuts();
    if countdown {
        SetTimer(bar, COUNT_TIMER, 100, None);
    } else {
        begin_recorder(bar, area, path, options);
    }
}

/// Opens the recorder on a helper thread; it posts `STARTED` when the first frame can be taken.
unsafe fn begin_recorder(bar: HWND, area: RECT, path: PathBuf, options: Options) {
    let target = bar as isize;
    std::thread::spawn(move || {
        let result = Box::into_raw(Box::new(recorder::start(area, &path, options)));
        // A bar closed meanwhile drops the recording, which cancels it and deletes the file.
        if PostMessageW(target as HWND, STARTED, 0, result as LPARAM) == 0 {
            drop(Box::from_raw(result));
        }
    });
}

/// A countdown tick: redraws the number, and starts the recorder at zero.
unsafe fn count_down(bar: HWND) {
    let Some((left, changed, area, path, options)) = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        let Phase::Countdown { until } = state.phase else { return None };
        let remaining = until.saturating_duration_since(Instant::now());
        let left = remaining.as_millis().div_ceil(1000) as u64;
        let changed = left != state.count;
        state.count = left;
        if left == 0 {
            state.phase = Phase::Starting { stop: None };
            state.started = Instant::now();
        }
        Some((left, changed, state.area, state.path.clone(), state.options))
    }) else {
        KillTimer(bar, COUNT_TIMER);
        return;
    };
    if changed {
        draw_frame();
        draw_bar();
    }
    if left == 0 {
        KillTimer(bar, COUNT_TIMER);
        draw_bar();
        begin_recorder(bar, area, path, options);
    }
}

/// The recorder is ready (or could not start): the clock starts now.
unsafe fn started(result: Result<Recording, String>) {
    let recording = match result {
        Ok(recording) => recording,
        Err(error) => {
            close_windows();
            show_error(null_mut(), &format!("无法开始录像。\n{error}"));
            return;
        }
    };
    let Some((bar, stop_now)) = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        let Phase::Starting { stop } = state.phase else { return None };
        state.recording = Some(recording);
        state.phase = Phase::Recording { paused_since: None };
        state.started = Instant::now();
        Some((state.bar, stop))
    }) else {
        return;
    };
    if let Some(cancel) = stop_now {
        stop(cancel);
        return;
    }
    draw_frame();
    draw_bar();
    SetTimer(bar, TICK_TIMER, 250, None);
}

/// Pause or resume, from the bar or the global shortcut.
pub unsafe fn toggle_pause() {
    let changed = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        let recording = state.recording.as_ref()?;
        let Phase::Recording { paused_since } = &mut state.phase else { return None };
        match paused_since.take() {
            Some(since) => {
                state.paused_total += since.elapsed();
                recording.send(Command::Resume);
            }
            None => {
                *paused_since = Some(Instant::now());
                recording.send(Command::Pause);
            }
        }
        Some(())
    });
    if changed.is_some() {
        draw_frame();
        draw_bar();
    }
}

/// Stops and saves (`cancel` throws the recording away). The file is
/// finished on a helper thread; the bar says so and then shows the result.
/// Pressed while the recorder is still starting, it takes effect once it has.
pub unsafe fn stop(cancel: bool) {
    // Stopped during the countdown: nothing was recorded, so there is nothing to keep.
    if STATE.with_borrow(|state| state.as_ref().is_some_and(|state| matches!(state.phase, Phase::Countdown { .. }))) {
        close_windows();
        return;
    }
    let taken = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        if let Phase::Starting { stop } = &mut state.phase {
            *stop = Some(cancel);
            return None;
        }
        if !matches!(state.phase, Phase::Recording { .. }) {
            return None;
        }
        let recording = state.recording.take()?;
        state.phase = Phase::Saving;
        Some((recording, state.bar, state.frame))
    });
    let Some((recording, bar, frame)) = taken else { return };
    KillTimer(bar, TICK_TIMER);
    // The panels shown for this recording go with it.
    crate::input_overlay::end_session();
    crate::refresh_shortcuts();
    ShowWindow(frame, SW_HIDE);
    recording.send(if cancel { Command::Cancel } else { Command::Stop });
    let target = bar as isize;
    std::thread::spawn(move || {
        let result = Box::into_raw(Box::new(recording.finish()));
        if PostMessageW(target as HWND, SAVED, 0, result as LPARAM) == 0 {
            drop(Box::from_raw(result));
        }
    });
    draw_bar();
}

unsafe fn close_windows() {
    let state = STATE.take();
    if let Some(state) = state {
        let recording = matches!(state.phase, Phase::Countdown { .. } | Phase::Starting { .. } | Phase::Recording { .. });
        if let Some(recording) = state.recording {
            recording.send(Command::Cancel);
            let _ = recording.finish();
        }
        DestroyWindow(state.frame);
        DestroyWindow(state.bar);
        crate::input_overlay::end_session();
        if recording {
            crate::refresh_shortcuts();
        }
    }
}

fn elapsed(state: &State) -> Duration {
    let paused = match state.phase {
        Phase::Recording { paused_since: Some(since) } => since.elapsed(),
        _ => Duration::ZERO,
    };
    state.started.elapsed().saturating_sub(state.paused_total + paused)
}

fn clock(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

// ---------------------------------------------------------------------------
// Drawing.

unsafe fn present(window: HWND, image: &Image, position: POINT) {
    let Ok(surface) = Surface::new(null_mut(), image.width, image.height) else { return };
    surface.write_pixels(&image.pixels);
    let screen = GetDC(null_mut());
    let size = SIZE { cx: image.width, cy: image.height };
    let origin = POINT::default();
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    UpdateLayeredWindow(window, screen, &position, &size, surface.dc, &origin, 0, &blend, ULW_ALPHA);
    ReleaseDC(null_mut(), screen);
}

/// A red frame (amber while paused) just outside the area, so it is not
/// recorded; during a countdown, the number in the middle of the area. A
/// full-screen recording has no frame.
unsafe fn draw_frame() {
    let Some((window, area, paused, count)) = STATE.with_borrow(|state| {
        let state = state.as_ref().filter(|state| state.kind == Kind::Area)?;
        let count = matches!(state.phase, Phase::Countdown { .. }).then_some(state.count);
        Some((state.frame, state.area, matches!(state.phase, Phase::Recording { paused_since: Some(_) }), count))
    }) else {
        return;
    };
    let pad = 3;
    let (width, height) = (area.right - area.left + pad * 2, area.bottom - area.top + pad * 2);
    let color = if paused { AMBER } else { RED };
    let Some(image) = rasterize_rect(width, height, |graphics| {
        graphics.stroke_shape([1.0, 1.0, width as f32 - 1.0, height as f32 - 1.0], 3.0, argb(color, 255), 2.0, paused);
        if let Some(count) = count.filter(|&count| count > 0) {
            let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
            let r = (width.min(height) as f32 / 4.0).clamp(24.0, 72.0);
            graphics.fill_ellipse([cx - r, cy - r, cx + r, cy + r], argb(0x0020_1c18, 190));
            graphics.text(&count.to_string(), [cx - r, cy - r, cx + r, cy + r], r * 1.1, true, argb(0x00ff_ffff, 255));
        }
    }) else {
        return;
    };
    present(window, &image, POINT { x: area.left - pad, y: area.top - pad });
}

/// The bar's buttons for the current phase, left to right.
fn buttons(phase: &Phase) -> &'static [Button] {
    match phase {
        // Starting already shows the recording bar, so nothing moves when the clock starts.
        Phase::Countdown { .. } | Phase::Starting { .. } | Phase::Recording { .. } => &[Button::Pause, Button::Stop, Button::Cancel],
        Phase::Saved(_) => &[Button::Open, Button::Folder, Button::Gif, Button::Close],
        Phase::Converted(_) => &[Button::Open, Button::Folder, Button::Close],
        Phase::Failed(_) => &[Button::Close],
        Phase::Saving | Phase::Converting { .. } => &[],
    }
}

/// The bar's size and its buttons' places, in bar pixels.
fn bar_layout(phase: &Phase, scale: f32) -> (i32, i32, Vec<(Button, RECT)>) {
    let s = |value: f32| (value * scale).round() as i32;
    let buttons = buttons(phase);
    let text = match phase {
        Phase::Countdown { .. } | Phase::Starting { .. } | Phase::Recording { .. } => 96.0,
        Phase::Saving => 150.0,
        Phase::Saved(_) | Phase::Converted(_) => 220.0,
        Phase::Converting { .. } => 200.0,
        Phase::Failed(_) => 300.0,
    };
    let button = |button: &Button| match button {
        Button::Open => 64.0,
        Button::Gif => 74.0,
        _ => 34.0,
    };
    let width = s(12.0 + text + buttons.iter().map(|b| button(b) + 4.0).sum::<f32>() + 8.0);
    let height = s(44.0);
    let mut left = s(12.0 + text);
    let places = buttons
        .iter()
        .map(|b| {
            let rect = RECT { left, top: s(6.0), right: left + s(button(b)), bottom: height - s(6.0) };
            left = rect.right + s(4.0);
            (*b, rect)
        })
        .collect();
    (width, height, places)
}

fn button_label(button: Button, phase: &Phase) -> (Option<Icon>, &'static str) {
    match button {
        Button::Pause => match phase {
            Phase::Recording { paused_since: Some(_) } => (Some(Icon::Play), ""),
            _ => (Some(Icon::Pause), ""),
        },
        Button::Stop => (Some(Icon::Stop), ""),
        Button::Cancel | Button::Close => (Some(Icon::Close), ""),
        Button::Open => (None, "打开"),
        Button::Folder => (Some(Icon::Folder), ""),
        Button::Gif => (None, "转 GIF"),
    }
}

unsafe fn draw_bar() {
    let Some((window, (area, kind), position_known, image)) = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        let (width, height, places) = bar_layout(&state.phase, state.scale);
        let palette = theme::palette();
        let scale = state.scale;
        let s = |value: f32| value * scale;
        // The dot stays lit (a blinking one moved the clock text with it).
        let (status, detail, dot) = match &state.phase {
            // Looks like a recording at 00:00, so starting reads as one step.
            Phase::Starting { .. } => (clock(Duration::ZERO), String::new(), Some(RED)),
            Phase::Countdown { .. } => (format!("{} 秒后开始", state.count.max(1)), String::new(), Some(AMBER)),
            Phase::Recording { paused_since } => {
                (clock(elapsed(state)), String::new(), Some(if paused_since.is_some() { AMBER } else { RED }))
            }
            Phase::Saving => ("正在保存录像…".to_string(), String::new(), None),
            Phase::Saved(summary) => {
                // More than 5 % of frames missed: the machine could not keep up.
                let dropped = (summary.dropped * 20 > summary.frames.max(1))
                    .then(|| format!("电脑忙不过来，少录了 {} 帧，可以试试 30 帧或 1080p", summary.dropped));
                (
                    format!("录像已保存 · {}", clock(Duration::from_secs_f64(summary.seconds))),
                    summary.warnings.first().cloned().or(dropped).unwrap_or_default(),
                    None,
                )
            }
            Phase::Converting { progress } => (format!("正在生成 GIF {}%", progress / 10), String::new(), None),
            Phase::Converted(_) => ("GIF 已保存".to_string(), String::new(), None),
            Phase::Failed(error) => (error.lines().next().unwrap_or_default().to_string(), error.lines().nth(1).unwrap_or_default().to_string(), None),
        };
        let image = rasterize_rect(width, height, |graphics| {
            graphics.fill_round([0.0, 0.0, width as f32, height as f32], s(10.0), argb(palette.tooltip, 240));
            let mut text_left = s(14.0);
            if let Some(color) = dot {
                let (cx, cy, r) = (s(20.0), height as f32 / 2.0, s(5.0));
                graphics.fill_ellipse([cx - r, cy - r, cx + r, cy + r], argb(color, 255));
                text_left = s(32.0);
            }
            let text_right = places.first().map_or(width as f32 - s(10.0), |(_, rect)| rect.left as f32 - s(6.0));
            if detail.is_empty() {
                graphics.text(&status, [text_left, 0.0, text_right, height as f32], s(13.0), true, argb(palette.tooltip_text, 255));
            } else {
                graphics.text(&status, [text_left, s(4.0), text_right, height as f32 / 2.0 + s(2.0)], s(12.0), true, argb(palette.tooltip_text, 255));
                graphics.text(&detail, [text_left, height as f32 / 2.0, text_right, height as f32 - s(4.0)], s(10.0), false, argb(AMBER, 255));
            }
            for (button, rect) in &places {
                let bounds = [rect.left as f32, rect.top as f32, rect.right as f32, rect.bottom as f32];
                let hover = state.hovered == Some(*button);
                let face = match button {
                    Button::Stop => argb(RED, if hover { 255 } else { 220 }),
                    _ => argb(0x00ff_ffff, if hover { 50 } else { 22 }),
                };
                graphics.fill_round(bounds, s(7.0), face);
                let (icon, label) = button_label(*button, &state.phase);
                if icon.is_none() {
                    graphics.text(label, bounds, s(12.0), false, argb(palette.tooltip_text, 255));
                }
            }
        })?;
        Some((state.bar, (state.area, state.kind), state.drag.is_some(), (image, places, scale)))
    }) else {
        return;
    };
    let (mut image, places, scale) = image;
    // Icons come from the shared icon renderer, drawn over the buttons.
    if let Ok(surface) = Surface::new(null_mut(), image.width, image.height) {
        surface.write_pixels(&image.pixels);
        let phase_icons: Vec<(RECT, Icon)> = STATE.with_borrow(|state| {
            let Some(state) = state.as_ref() else { return Vec::new() };
            places
                .iter()
                .filter_map(|(button, rect)| button_label(*button, &state.phase).0.map(|icon| (*rect, icon)))
                .collect()
        });
        for (rect, icon) in phase_icons {
            let size = (18.0 * scale).round() as i32;
            let left = (rect.left + rect.right - size) / 2;
            let top = (rect.top + rect.bottom - size) / 2;
            icons::paint(surface.dc, RECT { left, top, right: left + size, bottom: top + size }, icon, 0x00ff_ffff);
        }
        GdiFlush();
        image.pixels.copy_from_slice(surface.bytes(image.width, image.height));
    }
    let mut bounds = RECT::default();
    GetWindowRect(window, &mut bounds);
    let position = if position_known || bounds.right - bounds.left > 1 {
        // Keep its place (the user may have dragged it), re-centered as it grows
        // or shrinks; in a screen's corner it grows leftwards instead.
        match kind {
            Kind::Area => POINT { x: (bounds.left + bounds.right) / 2 - image.width / 2, y: bounds.top },
            Kind::Screen => POINT { x: bounds.right - image.width, y: bounds.top },
        }
    } else {
        bar_position(area, kind, image.width, image.height)
    };
    present(window, &image, position);
}

/// Under the area, or above it, or inside its bottom edge, on the area's
/// monitor. For a whole screen: its top-right corner, out of the game's way.
unsafe fn bar_position(area: RECT, kind: Kind, width: i32, height: i32) -> POINT {
    if kind == Kind::Screen {
        return POINT { x: area.right - width - 16, y: area.top + 16 };
    }
    let monitor = MonitorFromPoint(POINT { x: (area.left + area.right) / 2, y: (area.top + area.bottom) / 2 }, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    GetMonitorInfoW(monitor, &mut info);
    let work = info.rcWork;
    let x = ((area.left + area.right - width) / 2).clamp(work.left, work.right - width);
    let gap = 10;
    let y = if area.bottom + gap + height <= work.bottom {
        area.bottom + gap
    } else if area.top - gap - height >= work.top {
        area.top - gap - height
    } else {
        area.bottom - gap - height
    };
    POINT { x, y }
}

use std::mem::size_of;

// ---------------------------------------------------------------------------
// Input.

unsafe fn hit(point: POINT) -> Option<Button> {
    STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        let (_, _, places) = bar_layout(&state.phase, state.scale);
        places.into_iter().find(|(_, rect)| contains(*rect, point)).map(|(button, _)| button)
    })
}

unsafe fn result_path() -> Option<PathBuf> {
    STATE.with_borrow(|state| match &state.as_ref()?.phase {
        Phase::Saved(summary) => Some(summary.path.clone()),
        Phase::Converted(path) => Some(path.clone()),
        _ => None,
    })
}

unsafe fn open(path: &Path) {
    let file = wide(&path.to_string_lossy());
    ShellExecuteW(null_mut(), w!("open"), file.as_ptr(), null(), null(), SW_SHOWNORMAL);
}

unsafe fn show_in_folder(path: &Path) {
    let arguments = wide(&format!("/select,\"{}\"", path.display()));
    ShellExecuteW(null_mut(), w!("open"), w!("explorer.exe"), arguments.as_ptr(), null(), SW_SHOWNORMAL);
}

unsafe fn make_gif(bar: HWND) {
    let Some(video) = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        let Phase::Saved(summary) = &state.phase else { return None };
        let video = summary.path.clone();
        state.phase = Phase::Converting { progress: 0 };
        Some(video)
    }) else {
        return;
    };
    KillTimer(bar, CLOSE_TIMER);
    draw_bar();
    let target = bar as isize;
    std::thread::spawn(move || {
        let gif = unique_path(video.parent().unwrap_or(Path::new(".")), &format!("{}.gif", video.file_stem().unwrap_or_default().to_string_lossy()));
        let mut last = 0;
        let result = crate::gif_export::convert(&video, &gif, |done| {
            let permille = (done * 1000.0) as usize;
            if permille >= last + 20 || permille == 1000 {
                last = permille;
                unsafe { PostMessageW(target as HWND, GIF_PROGRESS, permille, 0) };
            }
        })
        .map(|()| gif);
        let result = Box::into_raw(Box::new(result));
        unsafe {
            if PostMessageW(target as HWND, GIF_DONE, 0, result as LPARAM) == 0 {
                drop(Box::from_raw(result));
            }
        }
    });
}

unsafe fn press(bar: HWND, button: Button) {
    match button {
        Button::Pause => toggle_pause(),
        Button::Stop => stop(false),
        Button::Cancel => stop(true),
        Button::Open => {
            if let Some(path) = result_path() {
                open(&path);
            }
        }
        Button::Folder => {
            if let Some(path) = result_path() {
                show_in_folder(&path);
            }
        }
        Button::Gif => make_gif(bar),
        Button::Close => close_windows(),
    }
}

unsafe extern "system" fn frame_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    DefWindowProcW(window, message, wparam, lparam)
}

unsafe extern "system" fn bar_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_NCHITTEST => {
            // The empty part of the bar drags it around.
            let mut point = mouse_point(lparam);
            ScreenToClient(window, &mut point);
            return if hit(point).is_some() { HTCLIENT } else { HTCAPTION } as LRESULT;
        }
        WM_SETCURSOR if (lparam & 0xffff) as u32 == HTCLIENT => {
            // Only the buttons are client area.
            SetCursor(LoadCursorW(null_mut(), IDC_HAND));
            return 1;
        }
        WM_MOUSEMOVE => {
            let hovered = hit(mouse_point(lparam));
            let changed = STATE.with_borrow_mut(|state| {
                let state = state.as_mut()?;
                (state.hovered != hovered).then(|| state.hovered = hovered)
            });
            if changed.is_some() {
                let mut track = windows_sys::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT {
                    cbSize: size_of::<windows_sys::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT>() as u32,
                    dwFlags: windows_sys::Win32::UI::Input::KeyboardAndMouse::TME_LEAVE,
                    hwndTrack: window,
                    dwHoverTime: 0,
                };
                windows_sys::Win32::UI::Input::KeyboardAndMouse::TrackMouseEvent(&mut track);
                draw_bar();
            }
            // Hovering keeps the result card open.
            KillTimer(window, CLOSE_TIMER);
        }
        windows_sys::Win32::UI::Controls::WM_MOUSELEAVE => {
            STATE.with_borrow_mut(|state| {
                if let Some(state) = state.as_mut() {
                    state.hovered = None;
                }
            });
            draw_bar();
            if result_path().is_some() {
                SetTimer(window, CLOSE_TIMER, CARD_MS, None);
            }
        }
        WM_LBUTTONUP => {
            if let Some(button) = hit(mouse_point(lparam)) {
                press(window, button);
            }
        }
        WM_TIMER if wparam == TICK_TIMER => {
            if STATE.with_borrow(|state| state.as_ref().is_some_and(|state| matches!(state.phase, Phase::Recording { .. }))) {
                draw_bar();
            }
        }
        WM_TIMER if wparam == CLOSE_TIMER => close_windows(),
        WM_TIMER if wparam == COUNT_TIMER => count_down(window),
        STARTED => {
            started(*Box::from_raw(lparam as *mut Result<Recording, String>));
        }
        SAVED => {
            let result = *Box::from_raw(lparam as *mut Result<Summary, String>);
            let cancelled = matches!(result, Ok(Summary { cancelled: true, .. }));
            if cancelled {
                close_windows();
                return 0;
            }
            STATE.with_borrow_mut(|state| {
                if let Some(state) = state.as_mut() {
                    state.phase = match result {
                        Ok(summary) => Phase::Saved(summary),
                        Err(error) => Phase::Failed(error),
                    };
                }
            });
            KillTimer(window, TICK_TIMER);
            draw_bar();
            SetTimer(window, CLOSE_TIMER, CARD_MS, None);
        }
        GIF_PROGRESS => {
            STATE.with_borrow_mut(|state| {
                if let Some(state) = state.as_mut() {
                    if let Phase::Converting { progress } = &mut state.phase {
                        *progress = wparam as u32;
                    }
                }
            });
            draw_bar();
        }
        GIF_DONE => {
            let result = *Box::from_raw(lparam as *mut Result<PathBuf, String>);
            STATE.with_borrow_mut(|state| {
                if let Some(state) = state.as_mut() {
                    state.phase = match result {
                        Ok(path) => Phase::Converted(path),
                        Err(error) => Phase::Failed(error),
                    };
                }
            });
            draw_bar();
            SetTimer(window, CLOSE_TIMER, CARD_MS, None);
        }
        _ => return DefWindowProcW(window, message, wparam, lparam),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_with_defaults() {
        let options = Options { fps: 30, quality: crate::recorder::Quality::P720, system_audio: false, microphone: true, cursor: false, countdown: true, show_keys: true, show_pad: false };
        assert_eq!(parse_options("folder=D:\\录像\r\nfps=30\r\nquality=720p\r\nsystem_audio=0\r\nmicrophone=1\r\ncursor=0\r\ncountdown=1\r\nshow_keys=1\r\n"), options);
        assert_eq!(parse_options("fps=24\r\n").fps, 30, "only 30 or 60");
        assert_eq!(parse_options("").fps, 60, "60 by default");
        assert_eq!(parse_options(""), Options::default());
        assert_eq!(clock(Duration::from_secs(75)), "01:15");
    }

    #[test]
    fn the_bar_grows_with_its_phase() {
        let (recording, _, buttons) = bar_layout(&Phase::Recording { paused_since: None }, 1.0);
        assert_eq!(buttons.len(), 3);
        let summary = Summary { path: PathBuf::from("a.mp4"), seconds: 3.0, frames: 90, dropped: 0, warnings: Vec::new(), cancelled: false };
        let (saved, _, buttons) = bar_layout(&Phase::Saved(summary), 1.0);
        assert_eq!(buttons.iter().map(|(button, _)| *button).collect::<Vec<_>>(), vec![Button::Open, Button::Folder, Button::Gif, Button::Close]);
        assert!(saved > recording);
    }
}
