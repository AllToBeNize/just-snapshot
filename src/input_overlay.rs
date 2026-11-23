//! Floating panels that show input for recordings and streams: a keyboard
//! and mouse panel (top-left by default) and an Xbox gamepad panel (top-right).
//! Keys light up as they are pressed. The panels are click-through, topmost
//! and drawn with per-pixel alpha. Each has a small grip (three bars) at its
//! bottom edge that drags it anywhere; the grip takes the mouse but is left
//! out of screen capture, so it never shows in a recording. Position, size
//! and opacity are stored in `%LOCALAPPDATA%\just-snapshot\overlay.txt`.
//!
//! Input comes from Raw Input on a message-only window, registered only while
//! the key panel is shown; the gamepad is polled only while its panel is
//! shown, so a hidden overlay costs nothing.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs, io,
    mem::size_of,
    path::PathBuf,
    ptr::{null, null_mut},
};
#[cfg(test)]
use std::env;
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
        Graphics::Gdi::*,
        UI::{
            HiDpi::GetDpiForWindow,
            Input::{
                GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
                RIDEV_INPUTSINK, RIDEV_REMOVE, RID_INPUT, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
                KeyboardAndMouse::{
                    GetAsyncKeyState, GetKeyNameTextW, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE,
                    TRACKMOUSEEVENT,
                },
                XboxController::{XInputGetState, XINPUT_GAMEPAD, XINPUT_STATE},
            },
            WindowsAndMessaging::*,
        },
    },
};

use crate::{
    capture::{Image, Surface},
    gdiplus::{rasterize_rect, Graphics},
    theme::{self, argb},
};

const PANEL_CLASS: *const u16 = w!("Snapshot.InputPanel");
const GRIP_CLASS: *const u16 = w!("Snapshot.InputGrip");
const SINK_CLASS: *const u16 = w!("Snapshot.InputSink");
const PAD_TIMER: usize = 1;
const STALE_TIMER: usize = 2;
const FLASH_TIMER: usize = 3;
const PAD_MS: u32 = 16;
const PAD_IDLE_MS: u32 = 1000;
const FLASH_MS: u32 = 260;
const OTHER_MS: u32 = 1400;
/// Panel sizes at 100 %, in pixels at 96 DPI.
const KEYS_SIZE: (f32, f32) = (360.0, 186.0);
const PAD_SIZE: (f32, f32) = (320.0, 186.0);
/// The collapsed grip at 96 DPI; it sits just under its panel.
const GRIP_SIZE: (f32, f32) = (64.0, 18.0);
/// Distance from the screen edge for the default places.
const MARGIN: i32 = 16;
const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panel {
    Keys,
    Pad,
}

impl Panel {
    fn index(self) -> usize {
        self as usize
    }

    fn key(self) -> &'static str {
        match self {
            Self::Keys => "keys",
            Self::Pad => "pad",
        }
    }

    fn base(self) -> (f32, f32) {
        match self {
            Self::Keys => KEYS_SIZE,
            Self::Pad => PAD_SIZE,
        }
    }
}

pub const PANELS: [Panel; 2] = [Panel::Keys, Panel::Pad];

/// A place on the primary screen: a corner, an edge's middle or the center.
/// Picked in the settings; dragging the panel afterwards moves it freely.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

pub const ANCHORS: [Anchor; 9] = [
    Anchor::TopLeft,
    Anchor::Top,
    Anchor::TopRight,
    Anchor::Left,
    Anchor::Center,
    Anchor::Right,
    Anchor::BottomLeft,
    Anchor::Bottom,
    Anchor::BottomRight,
];

impl Anchor {
    fn key(self) -> &'static str {
        ["top_left", "top", "top_right", "left", "center", "right", "bottom_left", "bottom", "bottom_right"][self as usize]
    }

    fn parse(value: &str) -> Option<Self> {
        ANCHORS.into_iter().find(|anchor| anchor.key() == value)
    }

    /// The panel's top-left corner at this anchor on `screen`.
    fn place(self, screen: RECT, width: i32, height: i32, margin: i32) -> POINT {
        let column = self as i32 % 3;
        let row = self as i32 / 3;
        let x = match column {
            0 => screen.left + margin,
            1 => (screen.left + screen.right - width) / 2,
            _ => screen.right - margin - width,
        };
        let y = match row {
            0 => screen.top + margin,
            1 => (screen.top + screen.bottom - height) / 2,
            _ => screen.bottom - margin - height,
        };
        POINT { x, y }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PanelConfig {
    pub visible: bool,
    /// Where the panel was dragged to (its top-left corner, screen pixels);
    /// `None` puts it at `anchor` (moved by `dx`, `dy` in files from before dragging).
    pub position: Option<(i32, i32)>,
    pub anchor: Anchor,
    dx: i32,
    dy: i32,
    /// Size in percent, 50 to 200.
    pub scale: i32,
    /// Opacity in percent, 20 to 100; 100 is fully opaque.
    pub opacity: i32,
}

impl PanelConfig {
    fn default_for(panel: Panel) -> Self {
        Self {
            visible: false,
            position: None,
            anchor: if panel == Panel::Keys { Anchor::TopLeft } else { Anchor::TopRight },
            dx: 0,
            dy: 0,
            scale: 100,
            opacity: 90,
        }
    }

    /// Puts the panel at `anchor`, forgetting where it was dragged.
    pub fn at_anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self.position = None;
        (self.dx, self.dy) = (0, 0);
        self
    }

    /// Sitting exactly at its anchor (not dragged away from it).
    pub fn anchored(&self) -> Option<Anchor> {
        (self.position.is_none() && self.dx == 0 && self.dy == 0).then_some(self.anchor)
    }

    pub fn clamped(mut self) -> Self {
        self.scale = self.scale.clamp(50, 200);
        self.opacity = self.opacity.clamp(20, 100);
        self
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OverlayConfig {
    pub panels: [PanelConfig; 2],
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self { panels: PANELS.map(PanelConfig::default_for) }
    }
}

#[cfg(test)]
thread_local! {
    /// Tests keep their overlay.txt here instead of the real one.
    static TEST_FOLDER: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn config_path() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(folder) = TEST_FOLDER.with_borrow(Clone::clone) {
        return Ok(folder.join("overlay.txt"));
    }
    Ok(crate::migrate::app_data()?.join("overlay.txt"))
}

impl OverlayConfig {
    pub fn load() -> Self {
        config_path().ok().and_then(|path| fs::read_to_string(path).ok()).map(|text| Self::parse(&text)).unwrap_or_default()
    }

    fn parse(text: &str) -> Self {
        let mut config = Self::default();
        let mut points = [(None, None); 2];
        for line in text.trim_start_matches('\u{feff}').lines() {
            let Some((name, value)) = line.split_once('=') else { continue };
            let Some((panel, field)) = name.trim().split_once('.') else { continue };
            let Some(panel) = PANELS.into_iter().find(|candidate| candidate.key() == panel) else { continue };
            let target = &mut config.panels[panel.index()];
            let value = value.trim();
            let number = value.parse::<i32>().ok();
            match field {
                "visible" => target.visible = value == "1",
                "x" => points[panel.index()].0 = number,
                "y" => points[panel.index()].1 = number,
                "anchor" => target.anchor = Anchor::parse(value).unwrap_or(target.anchor),
                "dx" => target.dx = number.unwrap_or(0),
                "dy" => target.dy = number.unwrap_or(0),
                "size" => target.scale = number.unwrap_or(100),
                "opacity" => target.opacity = number.unwrap_or(90),
                _ => {}
            }
            *target = target.clamped();
        }
        for (panel, point) in config.panels.iter_mut().zip(points) {
            if let (Some(x), Some(y)) = point {
                panel.position = Some((x, y));
            }
        }
        config
    }

    fn text(&self) -> String {
        PANELS
            .iter()
            .map(|&panel| {
                let c = self.panels[panel.index()];
                let key = panel.key();
                let position = c.position.map_or_else(String::new, |(x, y)| format!("{key}.x={x}\r\n{key}.y={y}\r\n"));
                format!(
                    "{key}.visible={}\r\n{position}{key}.anchor={}\r\n{key}.dx={}\r\n{key}.dy={}\r\n{key}.size={}\r\n{key}.opacity={}\r\n",
                    u8::from(c.visible),
                    c.anchor.key(),
                    c.dx,
                    c.dy,
                    c.scale,
                    c.opacity
                )
            })
            .collect()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path()?;
        fs::create_dir_all(path.parent().expect("settings directory")).map_err(|error| error.to_string())?;
        fs::write(&path, self.text()).map_err(|error| format!("无法保存按键显示设置。\n{error}"))
    }
}

/// What the panels show right now.
#[derive(Default)]
struct Input {
    keys: BTreeSet<u16>,
    /// Left, right, middle, back, forward.
    buttons: [bool; 5],
    /// Up (1) or down (-1) for a moment after the wheel turns.
    wheel: i8,
    /// The last key outside the drawn layout, like "Enter" or "F5".
    other: Option<String>,
    pad: Option<XINPUT_GAMEPAD>,
    pad_packet: u32,
}

struct Drag {
    panel: Panel,
    /// The cursor's offset from the panel's top-left corner.
    grab: POINT,
}

/// Panels turned on from the recording toolbar: shown inside the recording
/// area at their anchors, for that recording only. Nothing here is saved.
struct Session {
    /// The area, in screen pixels.
    area: RECT,
    shown: [bool; 2],
    /// Where a panel was dragged to during this recording.
    moved: [Option<(i32, i32)>; 2],
}

struct State {
    config: OverlayConfig,
    session: Option<Session>,
    windows: [HWND; 2],
    grips: [HWND; 2],
    sink: HWND,
    input: Input,
    drag: Option<Drag>,
    /// The grip under the mouse.
    hovered: Option<Panel>,
    listening: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub unsafe fn register(instance: HINSTANCE) -> io::Result<()> {
    for (class, proc, cursor) in [
        (PANEL_CLASS, panel_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT, IDC_ARROW),
        (GRIP_CLASS, grip_proc, IDC_SIZEALL),
        (SINK_CLASS, sink_proc, IDC_ARROW),
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

/// Creates the panels and shows the ones that were on last time.
pub unsafe fn start(instance: HINSTANCE) -> io::Result<()> {
    let sink = CreateWindowExW(0, SINK_CLASS, null(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), instance, null());
    if sink.is_null() {
        return Err(io::Error::last_os_error());
    }
    let style = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    let mut windows = [null_mut(); 2];
    let mut grips = [null_mut(); 2];
    for index in 0..2 {
        let title = if index == 0 { w!("just-snapshot 键鼠") } else { w!("just-snapshot 手柄") };
        let window = CreateWindowExW(style | WS_EX_TRANSPARENT, PANEL_CLASS, title, WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), instance, null());
        if window.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Owned by its panel, so it always stays in front of it.
        let grip = CreateWindowExW(style, GRIP_CLASS, w!("just-snapshot 拖动"), WS_POPUP, 0, 0, 1, 1, window, null_mut(), instance, null());
        if grip.is_null() {
            return Err(io::Error::last_os_error());
        }
        SetWindowDisplayAffinity(grip, WDA_EXCLUDEFROMCAPTURE);
        windows[index] = window;
        grips[index] = grip;
    }
    STATE.set(Some(State {
        config: OverlayConfig::load(),
        session: None,
        windows,
        grips,
        sink,
        input: Input::default(),
        drag: None,
        hovered: None,
        listening: false,
    }));
    for panel in PANELS {
        apply(panel);
    }
    Ok(())
}

pub fn config() -> OverlayConfig {
    STATE.with_borrow(|state| state.as_ref().map(|state| state.config).unwrap_or_default())
}

pub fn is_visible(panel: Panel) -> bool {
    config().panels[panel.index()].visible
}

impl State {
    /// The recording session shows this panel inside its area.
    fn in_session(&self, panel: Panel) -> Option<&Session> {
        self.session.as_ref().filter(|session| session.shown[panel.index()])
    }

    fn shown(&self, panel: Panel) -> bool {
        self.config.panels[panel.index()].visible || self.in_session(panel).is_some()
    }
}

/// Shows the panels picked on the recording toolbar inside `area` (screen
/// pixels), or moves them along with it; panels not picked go back to normal.
pub unsafe fn show_in(area: RECT, shown: [bool; 2]) {
    let changed = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        // A panel dragged during this recording stays where it was put, even as the area moves.
        let moved = state.session.as_ref().map_or([None; 2], |session| session.moved);
        let same = state.session.as_ref().is_some_and(|session| EqualRect(&session.area, &area) != 0 && session.shown == shown);
        state.session = Some(Session { area, shown, moved });
        Some(!same)
    });
    if changed == Some(true) {
        for panel in PANELS {
            apply(panel);
        }
    }
}

/// Puts the shown panels (and their grips) back in front, after a window that
/// came up over them (the recording area picker).
pub unsafe fn raise() {
    let windows = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        Some(PANELS.map(|panel| (state.shown(panel), state.windows[panel.index()], state.grips[panel.index()])))
    });
    for (shown, window, grip) in windows.into_iter().flatten() {
        if shown {
            SetWindowPos(window, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            SetWindowPos(grip, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

/// The recording is over: its panels go away; panels that were on anyway return to their place.
pub unsafe fn end_session() {
    let ended = STATE.with_borrow_mut(|state| state.as_mut()?.session.take());
    if ended.is_some() {
        for panel in PANELS {
            apply(panel);
        }
    }
}

/// Changes one panel's settings; shown and saved right away.
pub unsafe fn set(panel: Panel, config: PanelConfig) {
    let changed = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        let config = config.clamped();
        if state.config.panels[panel.index()] == config {
            return None;
        }
        state.config.panels[panel.index()] = config;
        Some(state.config)
    });
    if let Some(config) = changed {
        let _ = config.save();
        apply(panel);
    }
}

pub unsafe fn toggle(panel: Panel) {
    let mut config = config().panels[panel.index()];
    config.visible = !config.visible;
    set(panel, config);
}

/// Screen size of a panel at its scale and the window's DPI, shrunk to fit
/// `fit` (width, height) when given.
unsafe fn panel_size(window: HWND, panel: Panel, scale: i32, fit: Option<(i32, i32)>) -> (i32, i32, f32) {
    let dpi = match GetDpiForWindow(window) {
        0 => 96,
        dpi => dpi,
    };
    let mut factor = dpi as f32 / 96.0 * scale as f32 / 100.0;
    if let Some((width, height)) = fit {
        let (base_width, base_height) = panel.base();
        factor = factor.min(width as f32 / base_width).min(height as f32 / base_height).max(0.3);
    }
    let (width, height) = panel.base();
    ((width * factor).round() as i32, (height * factor).round() as i32, factor)
}

unsafe fn primary_screen() -> RECT {
    let monitor = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    GetMonitorInfoW(monitor, &mut info);
    info.rcMonitor
}

/// The panel's top-left corner: where it was dragged, unless that is on no
/// screen any more (a monitor was unplugged); then its default place.
unsafe fn origin(panel: Panel, config: PanelConfig, width: i32, height: i32) -> POINT {
    if let Some((x, y)) = config.position {
        let bounds = RECT { left: x, top: y, right: x + width, bottom: y + height };
        if !MonitorFromRect(&bounds, MONITOR_DEFAULTTONULL).is_null() {
            return POINT { x, y };
        }
    }
    let margin = ((MARGIN as f32 * width as f32 / panel.base().0).round() as i32).max(MARGIN);
    let place = config.anchor.place(primary_screen(), width, height, margin);
    POINT { x: place.x + config.dx, y: place.y + config.dy }
}

/// Keeps a dropped panel wholly on the screen it was dropped on.
unsafe fn keep_on_screen(bounds: RECT) -> (i32, i32) {
    let monitor = MonitorFromRect(&bounds, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    GetMonitorInfoW(monitor, &mut info);
    let screen = info.rcMonitor;
    let (width, height) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
    (
        bounds.left.min(screen.right - width).max(screen.left),
        bounds.top.min(screen.bottom - height).max(screen.top),
    )
}

/// Shows, hides, moves and redraws a panel to match its settings, and starts
/// or stops listening to the input it needs.
unsafe fn apply(panel: Panel) {
    let Some((window, grip, shown)) = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        Some((state.windows[panel.index()], state.grips[panel.index()], state.shown(panel)))
    }) else {
        return;
    };
    if !shown {
        ShowWindow(grip, SW_HIDE);
        ShowWindow(window, SW_HIDE);
    } else {
        redraw(panel);
        ShowWindow(window, SW_SHOWNOACTIVATE);
        ShowWindow(grip, SW_SHOWNOACTIVATE);
        SetWindowPos(window, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        SetWindowPos(grip, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
    update_listening();
}

/// Raw Input only while the key panel shows; the gamepad timer only while
/// the pad panel shows.
unsafe fn update_listening() {
    let Some((sink, keys, pad, listening)) = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        Some((state.sink, state.shown(Panel::Keys), state.shown(Panel::Pad), state.listening))
    }) else {
        return;
    };
    if keys != listening {
        let flags = if keys { RIDEV_INPUTSINK } else { RIDEV_REMOVE };
        let target = if keys { sink } else { null_mut() };
        let devices = [
            RAWINPUTDEVICE { usUsagePage: 1, usUsage: 6, dwFlags: flags, hwndTarget: target },
            RAWINPUTDEVICE { usUsagePage: 1, usUsage: 2, dwFlags: flags, hwndTarget: target },
        ];
        if RegisterRawInputDevices(devices.as_ptr(), 2, size_of::<RAWINPUTDEVICE>() as u32) != 0 {
            STATE.with_borrow_mut(|state| {
                if let Some(state) = state.as_mut() {
                    state.listening = keys;
                    if !keys {
                        state.input.keys.clear();
                        state.input.buttons = [false; 5];
                    }
                }
            });
        }
    }
    if pad {
        SetTimer(sink, PAD_TIMER, PAD_MS, None);
    } else {
        KillTimer(sink, PAD_TIMER);
    }
}

/// Draws a panel and puts it, and its grip, on screen where it belongs.
unsafe fn redraw(panel: Panel) {
    let Some((window, config, session, dragging, image, factor)) = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        let window = state.windows[panel.index()];
        let dragging = state.drag.as_ref().is_some_and(|drag| drag.panel == panel);
        let config = state.config.panels[panel.index()];
        // Inside a recording area a panel takes at most a third of its width and height.
        let session = state.in_session(panel).map(|session| (session.area, session.moved[panel.index()]));
        let fit = session.map(|(area, _)| ((area.right - area.left) / 3, (area.bottom - area.top) / 3));
        let (width, height, factor) = panel_size(window, panel, config.scale, fit);
        let image = render(panel, &state.input, width, height, factor, config.opacity)?;
        Some((window, config, session, dragging, image, factor))
    }) else {
        return;
    };
    // Keys and the gamepad redraw the panel while it is being dragged (a
    // button release can even arrive before the drop): it stays under the mouse.
    let mut bounds = RECT::default();
    let position = if dragging && GetWindowRect(window, &mut bounds) != 0 {
        POINT { x: bounds.left, y: bounds.top }
    } else {
        match session {
        Some((_, Some((x, y)))) => POINT { x, y },
        Some((area, None)) => {
            let margin = (12.0 * factor).round() as i32;
            config.anchor.place(area, image.width, image.height, margin)
        }
        None => origin(panel, config, image.width, image.height),
        }
    };
    present(window, &image, position);
    draw_grip(panel, position, image.width, image.height);
}

/// The place a panel sits at: its anchor inside the recording area during a
/// recording, unless it was dragged; on screen otherwise, unless dragged.
pub fn current_anchor(panel: Panel) -> Option<Anchor> {
    STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        let config = state.config.panels[panel.index()];
        match state.in_session(panel) {
            Some(session) => session.moved[panel.index()].is_none().then_some(config.anchor),
            None => config.anchored(),
        }
    })
}

/// The grip: a bar with three lines centered just under the panel (above it
/// when there is no room below), only for dragging. It is left out of screen
/// capture, so only the player sees it.
unsafe fn draw_grip(panel: Panel, panel_origin: POINT, width: i32, height: i32) {
    let Some((grip, dpi, active)) = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        let dpi = match GetDpiForWindow(state.windows[panel.index()]) {
            0 => 1.0,
            dpi => dpi as f32 / 96.0,
        };
        let dragging = state.drag.as_ref().is_some_and(|drag| drag.panel == panel);
        Some((state.grips[panel.index()], dpi, state.hovered == Some(panel) || dragging))
    }) else {
        return;
    };
    let Some(image) = render_grip(dpi, active) else { return };
    let gap = (4.0 * dpi).round() as i32;
    let x = panel_origin.x + (width - image.width) / 2;
    let below = panel_origin.y + height + gap;
    let monitor = MonitorFromPoint(POINT { x: x + image.width / 2, y: below }, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    GetMonitorInfoW(monitor, &mut info);
    let y = if below + image.height <= info.rcMonitor.bottom { below } else { panel_origin.y - gap - image.height };
    present(grip, &image, POINT { x, y });
}

unsafe fn render_grip(dpi: f32, active: bool) -> Option<Image> {
    let (width, height) = ((GRIP_SIZE.0 * dpi).round() as i32, (GRIP_SIZE.1 * dpi).round() as i32);
    rasterize_rect(width, height, |graphics| {
        let (w, h) = (width as f32, height as f32);
        let face = if active { argb(theme::palette().accent, 240) } else { argb(0x0010_0e0c, 210) };
        graphics.fill_round([0.0, 0.0, w, h], h / 2.0, face);
        graphics.stroke_round([0.5, 0.5, w - 0.5, h - 0.5], h / 2.0, argb(0x00ff_ffff, if active { 120 } else { 60 }), 1.0);
        // Three bars: the usual "drag me" sign.
        let (bar, thick, gap) = (22.0 * dpi, (1.6 * dpi).max(1.0), 3.8 * dpi);
        for row in -1..=1 {
            let y = h / 2.0 + row as f32 * gap;
            graphics.fill_round([(w - bar) / 2.0, y - thick / 2.0, (w + bar) / 2.0, y + thick / 2.0], thick / 2.0, argb(0x00ff_ffff, 235));
        }
    })
}

unsafe fn present(window: HWND, image: &Image, position: POINT) {
    let Ok(surface) = Surface::new(null_mut(), image.width, image.height) else { return };
    surface.write_pixels(&image.pixels);
    let screen = GetDC(null_mut());
    let size = SIZE { cx: image.width, cy: image.height };
    let origin = POINT::default();
    // Opacity is in the pixels themselves (see `colors`), not a window-wide fade.
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    UpdateLayeredWindow(window, screen, &position, &size, surface.dc, &origin, 0, &blend, ULW_ALPHA);
    ReleaseDC(null_mut(), screen);
}

/// One key of the drawn keyboard: label, virtual key, position and width in key units.
struct KeyCap {
    label: &'static str,
    vk: u16,
    x: f32,
    y: f32,
    w: f32,
}

const fn cap(label: &'static str, vk: u16, x: f32, y: f32, w: f32) -> KeyCap {
    KeyCap { label, vk, x, y, w }
}

/// The left half of a keyboard, where games put their keys.
const KEYBOARD: [KeyCap; 28] = [
    cap("Esc", 0x1b, 0.0, 0.0, 1.0),
    cap("1", 0x31, 1.0, 0.0, 1.0),
    cap("2", 0x32, 2.0, 0.0, 1.0),
    cap("3", 0x33, 3.0, 0.0, 1.0),
    cap("4", 0x34, 4.0, 0.0, 1.0),
    cap("5", 0x35, 5.0, 0.0, 1.0),
    cap("Tab", 0x09, 0.0, 1.0, 1.5),
    cap("Q", 0x51, 1.5, 1.0, 1.0),
    cap("W", 0x57, 2.5, 1.0, 1.0),
    cap("E", 0x45, 3.5, 1.0, 1.0),
    cap("R", 0x52, 4.5, 1.0, 1.0),
    cap("T", 0x54, 5.5, 1.0, 1.0),
    cap("Caps", 0x14, 0.0, 2.0, 1.75),
    cap("A", 0x41, 1.75, 2.0, 1.0),
    cap("S", 0x53, 2.75, 2.0, 1.0),
    cap("D", 0x44, 3.75, 2.0, 1.0),
    cap("F", 0x46, 4.75, 2.0, 1.0),
    cap("G", 0x47, 5.75, 2.0, 1.0),
    cap("Shift", 0x10, 0.0, 3.0, 2.25),
    cap("Z", 0x5a, 2.25, 3.0, 1.0),
    cap("X", 0x58, 3.25, 3.0, 1.0),
    cap("C", 0x43, 4.25, 3.0, 1.0),
    cap("V", 0x56, 5.25, 3.0, 1.0),
    cap("B", 0x42, 6.25, 3.0, 1.0),
    cap("Ctrl", 0x11, 0.0, 4.0, 1.5),
    cap("Win", 0x5b, 1.5, 4.0, 1.25),
    cap("Alt", 0x12, 2.75, 4.0, 1.25),
    cap("Space", 0x20, 4.0, 4.0, 3.25),
];

fn drawn(vk: u16) -> bool {
    KEYBOARD.iter().any(|key| key.vk == vk)
}

struct Colors {
    panel: u32,
    /// A key at rest.
    idle: u32,
    /// Smaller parts at rest: the wheel, side buttons, stick knobs.
    soft: u32,
    border: u32,
    text: u32,
    lit: u32,
    lit_text: u32,
    /// How strong unlit parts are, 0–1, for colors drawn elsewhere.
    strength: f32,
}

fn alpha(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The panel's colors at `opacity` percent. At 100 everything is opaque. As
/// it goes down the background fades all the way, while keys keep most of
/// their strength and pressed keys stay fully lit, so a faint panel still reads.
fn colors(opacity: i32) -> Colors {
    let o = opacity.clamp(20, 100) as f32 / 100.0;
    let strength = 0.4 + 0.6 * o;
    Colors {
        panel: argb(0x0018_1614, alpha(o)),
        idle: argb(0x0040_3c38, alpha(strength)),
        soft: argb(0x0078_726c, alpha(strength)),
        border: argb(0x00ff_ffff, alpha(0.22 * strength)),
        text: argb(0x00ff_ffff, alpha(0.75 + 0.2 * strength)),
        lit: argb(theme::palette().accent, 255),
        lit_text: argb(0x00ff_ffff, 255),
        strength,
    }
}

unsafe fn render(panel: Panel, input: &Input, width: i32, height: i32, factor: f32, opacity: i32) -> Option<Image> {
    rasterize_rect(width, height, |graphics| {
        let c = colors(opacity);
        let f = |value: f32| value * factor;
        graphics.fill_round([0.0, 0.0, width as f32, height as f32], f(14.0), c.panel);
        match panel {
            Panel::Keys => draw_keys(graphics, input, &c, &f),
            Panel::Pad => draw_pad(graphics, input, &c, &f),
        }
    })
}

unsafe fn key_face(graphics: &Graphics, c: &Colors, bounds: [f32; 4], radius: f32, lit: bool, label: &str, size: f32) {
    graphics.fill_round(bounds, radius, if lit { c.lit } else { c.idle });
    if !lit {
        graphics.stroke_round(bounds, radius, c.border, 1.0);
    }
    if !label.is_empty() {
        graphics.text(label, bounds, size, lit, if lit { c.lit_text } else { c.text });
    }
}

unsafe fn draw_keys(graphics: &Graphics, input: &Input, c: &Colors, f: &dyn Fn(f32) -> f32) {
    let (unit, gap, left, top) = (f(30.0), f(4.0), f(12.0), f(12.0));
    for key in &KEYBOARD {
        let x = left + key.x * (unit + gap);
        let y = top + key.y * (unit + gap);
        let width = key.w * unit + (key.w - 1.0).max(0.0) * gap;
        let size = if key.label.len() > 1 { f(10.5) } else { f(13.0) };
        key_face(graphics, c, [x, y, x + width, y + unit], f(6.0), input.keys.contains(&key.vk), key.label, size);
    }
    // The mouse: two buttons, the wheel and the two side buttons.
    let (mx, my, mw, mh) = (f(262.0), f(12.0), f(84.0), f(130.0));
    let half = mw / 2.0;
    graphics.fill_round([mx, my, mx + mw, my + mh], f(36.0), c.idle);
    graphics.stroke_round([mx, my, mx + mw, my + mh], f(36.0), c.border, 1.0);
    let button = |index: usize, bounds: [f32; 4]| {
        if input.buttons[index] {
            graphics.fill_round(bounds, f(12.0), c.lit);
        }
    };
    button(0, [mx + f(4.0), my + f(4.0), mx + half - f(2.0), my + f(56.0)]);
    button(1, [mx + half + f(2.0), my + f(4.0), mx + mw - f(4.0), my + f(56.0)]);
    graphics.line((mx + f(6.0), my + f(58.0)), (mx + mw - f(6.0), my + f(58.0)), c.border, 1.0);
    graphics.line((mx + half, my + f(4.0)), (mx + half, my + f(58.0)), c.border, 1.0);
    let wheel = [mx + half - f(7.0), my + f(18.0), mx + half + f(7.0), my + f(44.0)];
    graphics.fill_round(wheel, f(7.0), if input.buttons[2] { c.lit } else { c.soft });
    if input.wheel != 0 {
        let (cx, tip) = (mx + half, if input.wheel > 0 { my + f(8.0) } else { my + f(54.0) });
        let base = if input.wheel > 0 { tip + f(8.0) } else { tip - f(8.0) };
        graphics.fill_segments(
            &[
                crate::icons::Seg::M(cx, tip),
                crate::icons::Seg::L(cx - f(6.0), base),
                crate::icons::Seg::L(cx + f(6.0), base),
                crate::icons::Seg::Z,
            ],
            1.0,
            c.lit,
        );
    }
    // Side buttons on the left flank: forward above back.
    for (index, top) in [(4, my + f(66.0)), (3, my + f(92.0))] {
        let bounds = [mx - f(2.0), top, mx + f(8.0), top + f(20.0)];
        graphics.fill_round(bounds, f(4.0), if input.buttons[index] { c.lit } else { c.soft });
    }
    // A key outside the drawn layout shows by name under the mouse, only while it is fresh.
    if let Some(name) = &input.other {
        key_face(graphics, c, [mx, f(148.0), mx + mw, f(176.0)], f(6.0), true, name, f(12.0));
    }
}

unsafe fn draw_pad(graphics: &Graphics, input: &Input, c: &Colors, f: &dyn Fn(f32) -> f32) {
    let Some(pad) = input.pad else {
        graphics.text("未连接手柄", [0.0, 0.0, f(320.0), f(186.0)], f(16.0), false, c.text);
        graphics.text("插入 Xbox 手柄后自动显示", [0.0, f(110.0), f(320.0), f(140.0)], f(11.0), false, argb(0x00ff_ffff, alpha(0.6 * c.strength)));
        return;
    };
    let pressed = |bit: u16| pad.wButtons & bit != 0;
    // Triggers fill with how far they are pulled; bumpers light when held.
    for (x, value, label) in [(f(16.0), pad.bLeftTrigger, "LT"), (f(256.0), pad.bRightTrigger, "RT")] {
        let bounds = [x, f(12.0), x + f(48.0), f(34.0)];
        graphics.fill_round(bounds, f(6.0), c.idle);
        let fill = f32::from(value) / 255.0;
        if fill > 0.02 {
            graphics.fill_round([bounds[0], bounds[1], bounds[0] + (bounds[2] - bounds[0]) * fill, bounds[3]], f(6.0), c.lit);
        }
        graphics.stroke_round(bounds, f(6.0), c.border, 1.0);
        graphics.text(label, bounds, f(11.0), fill > 0.02, c.text);
    }
    key_face(graphics, c, [f(72.0), f(14.0), f(124.0), f(32.0)], f(9.0), pressed(0x0100), "LB", f(10.5));
    key_face(graphics, c, [f(196.0), f(14.0), f(248.0), f(32.0)], f(9.0), pressed(0x0200), "RB", f(10.5));
    // Sticks: the ring, and the knob where the stick points; it fills when clicked.
    let stick = |cx: f32, cy: f32, x: i16, y: i16, click: bool| {
        let r = f(28.0);
        graphics.fill_ellipse([cx - r, cy - r, cx + r, cy + r], c.idle);
        graphics.stroke_ellipse([cx - r, cy - r, cx + r, cy + r], c.border, 1.0, false);
        let reach = f(14.0);
        let (kx, ky) = (cx + f32::from(x) / 32767.0 * reach, cy - f32::from(y) / 32767.0 * reach);
        let k = f(13.0);
        graphics.fill_ellipse([kx - k, ky - k, kx + k, ky + k], if click { c.lit } else { c.soft });
    };
    stick(f(70.0), f(88.0), pad.sThumbLX, pad.sThumbLY, pressed(0x0040));
    stick(f(206.0), f(150.0), pad.sThumbRX, pad.sThumbRY, pressed(0x0080));
    // D-pad.
    let (dx, dy, arm, thick) = (f(112.0), f(150.0), f(15.0), f(13.0));
    for (bit, bounds) in [
        (0x0001, [dx - thick / 2.0, dy - arm - thick / 2.0, dx + thick / 2.0, dy - thick / 2.0]),
        (0x0002, [dx - thick / 2.0, dy + thick / 2.0, dx + thick / 2.0, dy + arm + thick / 2.0]),
        (0x0004, [dx - arm - thick / 2.0, dy - thick / 2.0, dx - thick / 2.0, dy + thick / 2.0]),
        (0x0008, [dx + thick / 2.0, dy - thick / 2.0, dx + arm + thick / 2.0, dy + thick / 2.0]),
    ] {
        key_face(graphics, c, bounds, f(3.0), pressed(bit), "", 0.0);
    }
    // View and menu.
    key_face(graphics, c, [f(128.0), f(76.0), f(152.0), f(92.0)], f(8.0), pressed(0x0020), "", 0.0);
    key_face(graphics, c, [f(168.0), f(76.0), f(192.0), f(92.0)], f(8.0), pressed(0x0010), "", 0.0);
    // Face buttons in their Xbox colors, bright when pressed.
    for (bit, label, color, cx, cy) in [
        (0x1000, "A", 0x0040_b04c, f(250.0), f(108.0)),
        (0x2000, "B", 0x003c_3ce0, f(274.0), f(84.0)),
        (0x4000, "X", 0x00e0_8a2a, f(226.0), f(84.0)),
        (0x8000, "Y", 0x0020_c8e8, f(250.0), f(60.0)),
    ] {
        let r = f(12.0);
        let lit = pressed(bit);
        graphics.fill_ellipse([cx - r, cy - r, cx + r, cy + r], argb(color, if lit { 255 } else { alpha(0.45 * c.strength) }));
        graphics.text(label, [cx - r, cy - r, cx + r, cy + r], f(12.0), true, argb(0x00ff_ffff, if lit { 255 } else { alpha(0.8 * c.strength) }));
    }
}

/// A readable name for a key the layout does not draw, like "Enter" or "F5".
unsafe fn key_name(make: u16, extended: bool) -> Option<String> {
    let mut buffer = [0u16; 32];
    let code = (i32::from(make) << 16) | if extended { 1 << 24 } else { 0 };
    let length = GetKeyNameTextW(code, buffer.as_mut_ptr(), buffer.len() as i32);
    (length > 0).then(|| String::from_utf16_lossy(&buffer[..length as usize]))
}

/// Applies one raw input event. Returns which panels need a redraw.
unsafe fn on_raw_input(handle: HRAWINPUT) -> bool {
    let mut size = 0u32;
    let header = size_of::<RAWINPUTHEADER>() as u32;
    GetRawInputData(handle, RID_INPUT, null_mut(), &mut size, header);
    if size == 0 || size as usize > 256 {
        return false;
    }
    let mut buffer = [0u64; 32];
    if GetRawInputData(handle, RID_INPUT, buffer.as_mut_ptr().cast(), &mut size, header) != size {
        return false;
    }
    let raw = &*(buffer.as_ptr() as *const RAWINPUT);
    STATE.with_borrow_mut(|state| {
        let Some(state) = state.as_mut() else { return false };
        let input = &mut state.input;
        match raw.header.dwType {
            RIM_TYPEKEYBOARD => {
                let keyboard = raw.data.keyboard;
                let vk = keyboard.VKey;
                if vk == 0xff || vk == 0 {
                    return false;
                }
                let up = keyboard.Flags & 1 != 0;
                let changed = if up { input.keys.remove(&vk) } else { input.keys.insert(vk) };
                if !up && !drawn(vk) {
                    let name = key_name(keyboard.MakeCode, keyboard.Flags & 2 != 0);
                    if name != input.other {
                        input.other = name;
                        SetTimer(state.sink, FLASH_TIMER, OTHER_MS, None);
                        return true;
                    }
                }
                if changed && !input.keys.is_empty() {
                    SetTimer(state.sink, STALE_TIMER, 500, None);
                }
                changed && drawn(vk)
            }
            RIM_TYPEMOUSE => {
                let flags = raw.data.mouse.Anonymous.Anonymous.usButtonFlags;
                let data = raw.data.mouse.Anonymous.Anonymous.usButtonData as i16;
                let before = (input.buttons, input.wheel);
                for (index, down, up) in [(0, 0x1, 0x2), (1, 0x4, 0x8), (2, 0x10, 0x20), (3, 0x40, 0x80), (4, 0x100, 0x200)] {
                    if flags & down != 0 {
                        input.buttons[index] = true;
                    }
                    if flags & up != 0 {
                        input.buttons[index] = false;
                    }
                }
                if flags & 0x400 != 0 && data != 0 {
                    input.wheel = if data > 0 { 1 } else { -1 };
                    SetTimer(state.sink, FLASH_TIMER, FLASH_MS, None);
                }
                (input.buttons, input.wheel) != before
            }
            _ => false,
        }
    })
}

/// Reads the first connected Xbox controller; redraws when it changes.
unsafe fn poll_pad() {
    let mut found = None;
    for user in 0..4 {
        let mut state = XINPUT_STATE::default();
        if XInputGetState(user, &mut state) == 0 {
            found = Some(state);
            break;
        }
    }
    let changed = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        let input = &mut state.input;
        let connected_before = input.pad.is_some();
        let changed = match found {
            Some(pad) => {
                let changed = !connected_before || pad.dwPacketNumber != input.pad_packet;
                input.pad = Some(pad.Gamepad);
                input.pad_packet = pad.dwPacketNumber;
                changed
            }
            None => {
                input.pad = None;
                connected_before
            }
        };
        // Look for a controller once a second while none is plugged in.
        SetTimer(state.sink, PAD_TIMER, if found.is_some() { PAD_MS } else { PAD_IDLE_MS }, None);
        Some(changed)
    });
    if changed == Some(true) {
        redraw(Panel::Pad);
    }
}

unsafe extern "system" fn sink_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_INPUT => {
            if on_raw_input(lparam as HRAWINPUT) {
                redraw(Panel::Keys);
            }
            return DefWindowProcW(window, message, wparam, lparam);
        }
        WM_TIMER if wparam == PAD_TIMER => poll_pad(),
        WM_TIMER if wparam == FLASH_TIMER => {
            KillTimer(window, FLASH_TIMER);
            STATE.with_borrow_mut(|state| {
                if let Some(state) = state.as_mut() {
                    state.input.wheel = 0;
                    state.input.other = None;
                }
            });
            redraw(Panel::Keys);
        }
        WM_TIMER if wparam == STALE_TIMER => {
            // A release can be missed (a secure desktop, an elevated window): check the keys still held.
            let changed = STATE.with_borrow_mut(|state| {
                let state = state.as_mut()?;
                let before = state.input.keys.len();
                state.input.keys.retain(|&vk| GetAsyncKeyState(i32::from(vk)) < 0);
                if state.input.keys.is_empty() {
                    KillTimer(window, STALE_TIMER);
                }
                Some(before != state.input.keys.len())
            });
            if changed == Some(true) {
                redraw(Panel::Keys);
            }
        }
        _ => return DefWindowProcW(window, message, wparam, lparam),
    }
    0
}

fn grip_of(window: HWND) -> Option<Panel> {
    STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        PANELS.into_iter().find(|panel| state.grips[panel.index()] == window)
    })
}

/// Ends a drag: the panel stays exactly where it was dropped (kept on that
/// screen). For a recording it is for that recording; otherwise it is saved.
unsafe fn finish_drag() {
    let Some((panel, window)) = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        let drag = state.drag.take()?;
        Some((drag.panel, state.windows[drag.panel.index()]))
    }) else {
        return;
    };
    let mut bounds = RECT::default();
    GetWindowRect(window, &mut bounds);
    let position = keep_on_screen(bounds);
    let temporary = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        state.session.as_mut().filter(|session| session.shown[panel.index()]).map(|session| session.moved[panel.index()] = Some(position))
    });
    if temporary.is_some() {
        redraw(panel);
        return;
    }
    let mut config = config().panels[panel.index()];
    config.position = Some(position);
    set(panel, config);
    redraw(panel);
    // The settings' place grid no longer shows an anchor.
    crate::settings::refresh();
}

/// A place picked for a panel: it goes there (inside the recording area
/// during a recording) and stops being free.
pub unsafe fn pick_anchor(panel: Panel, anchor: Anchor) {
    let in_session = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        state.session.as_mut().filter(|session| session.shown[panel.index()]).map(|session| session.moved[panel.index()] = None)
    });
    let current = config().panels[panel.index()];
    let config = if in_session.is_some() {
        // The recording uses the panel's anchor inside its area; where it was dragged on screen stays.
        PanelConfig { anchor, ..current }
    } else {
        current.at_anchor(anchor)
    };
    set(panel, config);
    redraw(panel);
    crate::settings::refresh();
}

/// Changes a panel's opacity by `step` percent.
pub unsafe fn step_opacity(panel: Panel, step: i32) {
    let mut config = config().panels[panel.index()];
    config.opacity += step;
    set(panel, config);
    redraw(panel);
    crate::settings::refresh();
}

/// The grip takes the mouse for its panel: drag it to move the panel
/// anywhere; the wheel resizes the panel, Ctrl+wheel changes its opacity.
unsafe extern "system" fn grip_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_LBUTTONDOWN => {
            if let Some(panel) = grip_of(window) {
                let mut cursor = POINT::default();
                GetCursorPos(&mut cursor);
                STATE.with_borrow_mut(|state| {
                    if let Some(state) = state.as_mut() {
                        let mut bounds = RECT::default();
                        GetWindowRect(state.windows[panel.index()], &mut bounds);
                        state.drag = Some(Drag { panel, grab: POINT { x: cursor.x - bounds.left, y: cursor.y - bounds.top } });
                    }
                });
                SetCapture(window);
                redraw(panel);
            }
        }
        WM_MOUSEMOVE => {
            let Some(panel) = grip_of(window) else { return 0 };
            let (drag, entered, panel_window) = STATE.with_borrow_mut(|state| {
                let Some(state) = state.as_mut() else { return (None, false, null_mut()) };
                let entered = state.hovered != Some(panel);
                state.hovered = Some(panel);
                (state.drag.as_ref().map(|drag| drag.grab), entered, state.windows[panel.index()])
            });
            if entered {
                let mut track = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: window, dwHoverTime: 0 };
                TrackMouseEvent(&mut track);
            }
            if let Some(grab) = drag {
                // Panel and grip move together, the grip keeping its place under the panel.
                let (mut cursor, mut panel_bounds, mut grip_bounds) = (POINT::default(), RECT::default(), RECT::default());
                GetCursorPos(&mut cursor);
                GetWindowRect(panel_window, &mut panel_bounds);
                GetWindowRect(window, &mut grip_bounds);
                let (x, y) = (cursor.x - grab.x, cursor.y - grab.y);
                let flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
                SetWindowPos(panel_window, null_mut(), x, y, 0, 0, flags);
                SetWindowPos(window, null_mut(), x + grip_bounds.left - panel_bounds.left, y + grip_bounds.top - panel_bounds.top, 0, 0, flags);
            } else if entered {
                redraw(panel);
            }
        }
        windows_sys::Win32::UI::Controls::WM_MOUSELEAVE => {
            let panel = STATE.with_borrow_mut(|state| state.as_mut()?.hovered.take());
            if let Some(panel) = panel {
                redraw(panel);
            }
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            finish_drag();
        }
        // Capture taken away mid-drag (Alt+Tab, a dialog): drop the panel where it is.
        WM_CAPTURECHANGED => finish_drag(),
        WM_MOUSEWHEEL => {
            if let Some(panel) = grip_of(window) {
                let notches = i32::from((wparam >> 16) as i16) / 120;
                let mut config = config().panels[panel.index()];
                if wparam & 0x0008 != 0 {
                    config.opacity += notches * 5;
                } else {
                    config.scale += notches * 10;
                }
                set(panel, config);
                crate::settings::refresh();
            }
        }
        _ => return DefWindowProcW(window, message, wparam, lparam),
    }
    0
}

/// The panels never take the mouse (they are click-through); only the grip does.
unsafe extern "system" fn panel_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_stay_in_range() {
        let mut config = OverlayConfig::default();
        assert_eq!(config.panels[0].anchor, Anchor::TopLeft);
        assert_eq!(config.panels[1].anchor, Anchor::TopRight);
        assert_eq!(config.panels[0].position, None, "not dragged yet");
        config.panels[1] = PanelConfig { visible: true, position: Some((-1500, 30)), anchor: Anchor::BottomRight, dx: -12, dy: 30, scale: 150, opacity: 60 };
        assert_eq!(OverlayConfig::parse(&config.text()), config);
        let clamped = OverlayConfig::parse("keys.size=999\r\nkeys.opacity=1\r\npad.anchor=nowhere\r\npad.x=5\r\n");
        assert_eq!((clamped.panels[0].scale, clamped.panels[0].opacity), (200, 20));
        assert_eq!(clamped.panels[1].anchor, Anchor::TopRight);
        assert_eq!(clamped.panels[1].position, None, "half a position is no position");
        let dragged = PanelConfig { position: Some((40, 50)), ..PanelConfig::default_for(Panel::Keys) };
        assert_eq!(dragged.anchored(), None, "a dragged panel sits at no anchor");
        let placed = dragged.at_anchor(Anchor::Bottom);
        assert_eq!((placed.position, placed.anchored()), (None, Some(Anchor::Bottom)));
    }

    #[test]
    fn anchors_place_panels_inside_the_screen() {
        let screen = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        let corner = Anchor::BottomRight.place(screen, 300, 200, 16);
        assert_eq!((corner.x, corner.y), (1920 - 16 - 300, 1080 - 16 - 200));
        let center = Anchor::Center.place(screen, 300, 200, 16);
        assert_eq!((center.x, center.y), (810, 440));
    }

    #[test]
    fn a_redraw_in_the_middle_of_a_drag_does_not_pull_the_panel_back() {
        unsafe {
            let folder = env::temp_dir().join(format!("snapshot-overlay-drag-{}", std::process::id()));
            fs::create_dir_all(&folder).unwrap();
            TEST_FOLDER.set(Some(folder.clone()));
            let instance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null());
            let _ = register(instance);
            start(instance).unwrap();
            let keys = PanelConfig { visible: true, position: Some((100, 100)), ..PanelConfig::default_for(Panel::Keys) };
            set(Panel::Keys, keys);
            let window = STATE.with_borrow(|state| state.as_ref().unwrap().windows[0]);
            // Grab the grip and move the panel, as the mouse does.
            STATE.with_borrow_mut(|state| state.as_mut().unwrap().drag = Some(Drag { panel: Panel::Keys, grab: POINT { x: 5, y: 5 } }));
            SetWindowPos(window, null_mut(), 300, 240, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            // A mouse button or gamepad change redraws the panel mid-drag (the button release can even come first).
            redraw(Panel::Keys);
            let mut bounds = RECT::default();
            GetWindowRect(window, &mut bounds);
            assert_eq!((bounds.left, bounds.top), (300, 240), "the panel stays under the mouse");
            finish_drag();
            assert_eq!(config().panels[0].position, Some((300, 240)), "and is dropped where it was let go");
            let state = STATE.take().unwrap();
            for window in state.grips.into_iter().chain(state.windows).chain([state.sink]) {
                DestroyWindow(window);
            }
            TEST_FOLDER.set(None);
            let _ = fs::remove_dir_all(&folder);
        }
    }

    #[test]
    fn full_opacity_is_opaque_and_low_opacity_keeps_keys_readable() {
        let full = colors(100);
        assert_eq!(full.panel >> 24, 255, "100 % background is solid");
        assert_eq!(full.idle >> 24, 255);
        let half = colors(50);
        assert_eq!(half.panel >> 24, 128);
        assert!(half.idle >> 24 >= 170, "keys stay visible at 50 %: {}", half.idle >> 24);
        let faint = colors(20);
        assert!(faint.panel >> 24 <= 52);
        assert!(faint.idle >> 24 >= 120 && faint.lit >> 24 == 255, "pressed keys always fully lit");
    }

    /// Both panels with some input, at 100 % and 40 % opacity. Run with
    /// `<test binary> input_overlay --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        unsafe {
            let mut input = Input::default();
            input.keys.extend([0x57u16, 0x10, 0x20]);
            input.buttons = [true, false, false, false, true];
            input.wheel = 1;
            input.other = Some("Enter".into());
            let pad = XINPUT_GAMEPAD { wButtons: 0x1000 | 0x0100 | 0x0001, bRightTrigger: 180, sThumbLX: 20000, sThumbLY: 15000, ..Default::default() };
            input.pad = Some(pad);
            let folder = env::temp_dir().join("snapshot-preview");
            fs::create_dir_all(&folder).unwrap();
            for (name, panel, opacity) in [("keys", Panel::Keys, 100), ("keys-40", Panel::Keys, 40), ("pad", Panel::Pad, 100), ("pad-40", Panel::Pad, 40)] {
                let (width, height) = panel.base();
                let factor = 1.5;
                let image = render(panel, &input, (width * factor) as i32, (height * factor) as i32, factor, opacity).unwrap();
                // Over a light "game" picture so the transparency shows, with the grip where it sits on screen.
                let mut flat = image.clone();
                for (index, pixel) in flat.pixels.chunks_exact_mut(4).enumerate() {
                    let checker = if (index % image.width as usize / 24 + index / image.width as usize / 24) % 2 == 0 { 200 } else { 150 };
                    let alpha = u32::from(pixel[3]);
                    for channel in &mut pixel[..3] {
                        *channel = (u32::from(*channel) + checker * (255 - alpha) / 255) as u8;
                    }
                    pixel[3] = 255;
                }
                flat.save_png(&folder.join(format!("overlay-{name}.png"))).unwrap();
            }
            // The grip, at rest and under the mouse, at 150 %.
            for (name, active) in [("grip", false), ("grip-hover", true)] {
                let mut image = render_grip(1.5, active).unwrap();
                for pixel in image.pixels.chunks_exact_mut(4) {
                    let alpha = u32::from(pixel[3]);
                    for channel in &mut pixel[..3] {
                        *channel = (u32::from(*channel) + 170 * (255 - alpha) / 255) as u8;
                    }
                    pixel[3] = 255;
                }
                image.save_png(&folder.join(format!("overlay-{name}.png"))).unwrap();
            }
            input.pad = None;
            let image = render(Panel::Pad, &input, 320, 186, 1.0, 90).unwrap();
            assert_eq!((image.width, image.height), (320, 186));
        }
    }
}
