use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io,
    mem::size_of,
    ptr::{null, null_mut},
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
        Graphics::{Gdi::*, GdiPlus::PointF},
        UI::{
            Controls::WM_MOUSELEAVE,
            HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow},
            Input::KeyboardAndMouse::{
                RegisterHotKey, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent,
                UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, TME_LEAVE, TRACKMOUSEEVENT, VK_CONTROL,
                VK_ESCAPE, VK_LWIN, VK_MENU, VK_RETURN, VK_RWIN, VK_SHIFT,
            },
            WindowsAndMessaging::*,
        },
    },
};

use crate::{
    apply_preferences, hotkey,
    hotkey::{Binding, Bindings, Preferences, Shortcut, BINDINGS, DEFAULT_OPACITY_STEP},
    icons::{self, Icon},
    overlay_ui::monitor_info,
    pinned, retry_hotkey, startup,
    text_render::Graphics,
    wide,
};

const CLASS: *const u16 = w!("Snapshot.Settings");
const PROBE_HOTKEY: i32 = 0x7ff0;
const ANIMATION_TIMER: usize = 1;
const FRAME_MS: u32 = 15;
/// Share of the remaining distance covered per frame: a quick ease-out.
const EASE: f32 = 0.32;
const PULSE_MS: f32 = 1100.0;

// Layout in 96-DPI pixels.
const WIDTH: i32 = 620;
const PAD: i32 = 24;
const HEADER: i32 = 52;
const LABEL: i32 = 22;
const SECTION_GAP: i32 = 14;
const HERO: i32 = 82;
const ROW: i32 = 42;
const ROWS: i32 = 5;
const CARD_INSET: i32 = 6;
const GENERAL_ROW: i32 = 56;
const FOOTER: i32 = 60;
const HERO_KEYS: i32 = 220;
const ROW_KEYS: i32 = 136;

// COLORREF values (0x00BBGGRR).
const BACKGROUND: u32 = rgb(0xf3, 0xf4, 0xf6);
const CARD: u32 = rgb(0xff, 0xff, 0xff);
const CARD_BORDER: u32 = rgb(0xe3, 0xe5, 0xe9);
const DIVIDER: u32 = rgb(0xee, 0xef, 0xf2);
const TEXT: u32 = rgb(0x1f, 0x23, 0x29);
const SECONDARY: u32 = rgb(0x6b, 0x72, 0x80);
const TERTIARY: u32 = rgb(0x9a, 0xa0, 0xa8);
const ICON: u32 = rgb(0x4b, 0x52, 0x5c);
const ACCENT: u32 = rgb(0x3d, 0x6f, 0xd9);
const ACCENT_DARK: u32 = rgb(0x2f, 0x5b, 0xbd);
const ACCENT_SOFT: u32 = rgb(0xea, 0xf0, 0xfc);
const ACCENT_MUTED: u32 = rgb(0xbf, 0xcf, 0xf0);
const SUCCESS: u32 = rgb(0x3a, 0x93, 0x5e);
const WARNING: u32 = rgb(0xc4, 0x7d, 0x1a);
const WARNING_SOFT: u32 = rgb(0xfd, 0xf4, 0xe4);
const ERROR: u32 = rgb(0xc9, 0x45, 0x45);
const HOVER: u32 = rgb(0xf0, 0xf2, 0xf5);
const PRESSED: u32 = rgb(0xe4, 0xe7, 0xeb);
const KEY_FACE: u32 = rgb(0xf8, 0xf9, 0xfb);
const KEY_BORDER: u32 = rgb(0xd5, 0xd9, 0xdf);
const KEY_DEPTH: u32 = rgb(0xc9, 0xce, 0xd5);
const TRACK_OFF: u32 = rgb(0xc9, 0xcd, 0xd4);

const fn rgb(red: u32, green: u32, blue: u32) -> u32 {
    red | (green << 8) | (blue << 16)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Target {
    Key(usize),
    Toggle,
    Minus,
    Plus,
    Reset,
    Cancel,
    Save,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CaptureStatus {
    Active,
    Available,
    Occupied,
    Duplicate(usize),
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Notice {
    None,
    Hint(String),
    Error(String),
}

/// The values being edited; nothing is applied until the user saves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Draft {
    bindings: Bindings,
    opacity_step: u8,
    start_at_login: bool,
}

impl Draft {
    fn defaults() -> Self {
        Self {
            bindings: hotkey::DEFAULT,
            opacity_step: DEFAULT_OPACITY_STEP,
            start_at_login: true,
        }
    }

    fn shortcut(&self, index: usize) -> Shortcut {
        self.bindings.get(BINDINGS[index])
    }

    fn duplicate_of(&self, index: usize) -> Option<usize> {
        let shortcut = self.shortcut(index);
        (0..BINDINGS.len()).find(|&other| other != index && self.shortcut(other) == shortcut)
    }

    fn first_duplicate(&self) -> Option<(usize, usize)> {
        (0..BINDINGS.len()).find_map(|index| {
            self.duplicate_of(index)
                .filter(|&other| other > index)
                .map(|other| (index, other))
        })
    }

    fn step_opacity(&mut self, delta: i32) {
        self.opacity_step = (i32::from(self.opacity_step) + delta).clamp(1, 100) as u8;
    }

    fn preferences(&self) -> Result<Preferences, String> {
        Ok(Preferences {
            bindings: self.bindings.validate()?,
            opacity_step: self.opacity_step,
        })
    }
}

struct Fonts {
    title: HFONT,
    heading: HFONT,
    body: HFONT,
    small: HFONT,
    key: HFONT,
}

impl Fonts {
    unsafe fn new(scale: f32) -> Self {
        let font = |size: f32, weight: i32, face: *const u16| {
            CreateFontW(
                -(size * scale).round() as i32,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                face,
            )
        };
        let ui = w!("Microsoft YaHei UI");
        Self {
            title: font(20.0, FW_SEMIBOLD as i32, ui),
            heading: font(14.0, FW_SEMIBOLD as i32, ui),
            body: font(14.0, FW_NORMAL as i32, ui),
            small: font(12.0, FW_NORMAL as i32, ui),
            key: font(12.5, FW_SEMIBOLD as i32, w!("Segoe UI")),
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for font in [self.title, self.heading, self.body, self.small, self.key] {
            if !font.is_null() {
                unsafe { DeleteObject(font) };
            }
        }
    }
}

struct State {
    owner: HWND,
    draft: Draft,
    capture_status: CaptureStatus,
    recording: Option<usize>,
    held: u32,
    hovered: Option<Target>,
    pressed: Option<Target>,
    hover: HashMap<Target, f32>,
    toggle: f32,
    pulse: f32,
    notice: Notice,
    scale: f32,
    fonts: Fonts,
    tracking: bool,
    animating: bool,
}

thread_local! {
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Every interactive rect, computed from the DPI scale alone so painting and
/// hit testing always agree.
struct Layout {
    scale: f32,
    width: i32,
    height: i32,
    capture_label: RECT,
    hero: RECT,
    toolbar_label: RECT,
    toolbar: RECT,
    general_label: RECT,
    general: RECT,
    footer: i32,
    keys: [RECT; BINDINGS.len()],
    rows: [RECT; BINDINGS.len()],
    toggle: RECT,
    stepper: RECT,
    minus: RECT,
    plus: RECT,
    reset: RECT,
    cancel: RECT,
    save: RECT,
}

impl Layout {
    fn new(scale: f32) -> Self {
        let s = |value: i32| (value as f32 * scale).round() as i32;
        let rect = |left: i32, top: i32, right: i32, bottom: i32| RECT {
            left: s(left),
            top: s(top),
            right: s(right),
            bottom: s(bottom),
        };
        let mut y = HEADER;
        let capture_label = rect(PAD, y, WIDTH - PAD, y + LABEL);
        y += LABEL;
        let hero = rect(PAD, y, WIDTH - PAD, y + HERO);
        y += HERO + SECTION_GAP;
        let toolbar_label = rect(PAD, y, WIDTH - PAD, y + LABEL);
        y += LABEL;
        let toolbar_top = y;
        let toolbar_height = ROW * ROWS + CARD_INSET * 2;
        let toolbar = rect(PAD, y, WIDTH - PAD, y + toolbar_height);
        y += toolbar_height + SECTION_GAP;
        let general_label = rect(PAD, y, WIDTH - PAD, y + LABEL);
        y += LABEL;
        let general_top = y;
        let general = rect(PAD, y, WIDTH - PAD, y + GENERAL_ROW * 2);
        y += GENERAL_ROW * 2 + SECTION_GAP + 4;
        let footer = s(y);
        let height = s(y + FOOTER);

        let mut keys = [RECT::default(); BINDINGS.len()];
        let mut rows = [RECT::default(); BINDINGS.len()];
        let hero_top = HEADER + LABEL;
        rows[0] = hero;
        keys[0] = rect(
            WIDTH - PAD - 16 - HERO_KEYS,
            hero_top + 12,
            WIDTH - PAD - 16,
            hero_top + 12 + 34,
        );
        let column = (WIDTH - PAD * 2 - CARD_INSET * 2) / 2;
        for index in 1..BINDINGS.len() {
            let slot = (index - 1) as i32;
            let left = PAD + CARD_INSET + (slot / ROWS) * column;
            let top = toolbar_top + CARD_INSET + (slot % ROWS) * ROW;
            rows[index] = rect(left, top, left + column, top + ROW);
            let right = left + column - 8;
            keys[index] = rect(right - ROW_KEYS, top + 6, right, top + ROW - 6);
        }

        let right = WIDTH - PAD - 18;
        let toggle_top = general_top + (GENERAL_ROW - 24) / 2;
        let toggle = rect(right - 44, toggle_top, right, toggle_top + 24);
        let stepper_top = general_top + GENERAL_ROW + (GENERAL_ROW - 30) / 2;
        let stepper = rect(right - 112, stepper_top, right, stepper_top + 30);
        let minus = rect(right - 112, stepper_top, right - 80, stepper_top + 30);
        let plus = rect(right - 32, stepper_top, right, stepper_top + 30);

        let button_top = y + (FOOTER - 34) / 2;
        let save = rect(WIDTH - PAD - 84, button_top, WIDTH - PAD, button_top + 34);
        let cancel = rect(
            WIDTH - PAD - 84 * 2 - 10,
            button_top,
            WIDTH - PAD - 84 - 10,
            button_top + 34,
        );
        let reset = rect(PAD - 8, button_top, PAD - 8 + 92, button_top + 34);
        Self {
            scale,
            width: s(WIDTH),
            height,
            capture_label,
            hero,
            toolbar_label,
            toolbar,
            general_label,
            general,
            footer,
            keys,
            rows,
            toggle,
            stepper,
            minus,
            plus,
            reset,
            cancel,
            save,
        }
    }

    fn s(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    fn targets(&self) -> impl Iterator<Item = (Target, RECT)> + '_ {
        (0..BINDINGS.len())
            .map(|index| (Target::Key(index), self.keys[index]))
            .chain([
                (Target::Toggle, self.toggle),
                (Target::Minus, self.minus),
                (Target::Plus, self.plus),
                (Target::Reset, self.reset),
                (Target::Cancel, self.cancel),
                (Target::Save, self.save),
            ])
    }

    fn hit(&self, point: POINT) -> Option<Target> {
        self.targets()
            .find(|(_, rect)| contains(*rect, point))
            .map(|(target, _)| target)
    }
}

fn contains(rect: RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

pub unsafe fn register(instance: HINSTANCE) -> io::Result<()> {
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        lpszClassName: CLASS,
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn is_open() -> bool {
    !WINDOW.get().is_null()
}

pub unsafe fn open(owner: HWND, instance: HINSTANCE) -> io::Result<()> {
    let existing = WINDOW.get();
    if !existing.is_null() {
        SetForegroundWindow(existing);
        return Ok(());
    }
    // The other program may have released the shortcut since startup.
    retry_hotkey(owner);
    let start_at_login = startup::is_enabled().map_err(io::Error::other)?;
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    let window = CreateWindowExW(
        0,
        CLASS,
        w!("Snapshot 设置"),
        style,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        WIDTH,
        WIDTH,
        owner,
        null_mut(),
        instance,
        null(),
    );
    if window.is_null() {
        return Err(io::Error::last_os_error());
    }
    let draft = Draft {
        bindings: hotkey::current(),
        opacity_step: pinned::opacity_step(),
        start_at_login,
    };
    install(window, owner, draft);
    WINDOW.set(window);
    place(window, None);
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    SetFocus(window);
    Ok(())
}

unsafe fn install(window: HWND, owner: HWND, draft: Draft) {
    let scale = window_scale(window);
    STATE.set(Some(State {
        owner,
        draft,
        capture_status: CaptureStatus::Active,
        recording: None,
        held: 0,
        hovered: None,
        pressed: None,
        hover: HashMap::new(),
        toggle: if draft.start_at_login { 1.0 } else { 0.0 },
        pulse: 0.0,
        notice: Notice::None,
        scale,
        fonts: Fonts::new(scale),
        tracking: false,
        animating: false,
    }));
    refresh_capture_status(window);
}

unsafe fn window_scale(window: HWND) -> f32 {
    match GetDpiForWindow(window) {
        0 => 1.0,
        dpi => dpi as f32 / 96.0,
    }
}

/// Sizes the window to the layout; centers it unless a DPI change suggested a spot.
unsafe fn place(window: HWND, suggested: Option<RECT>) {
    let Some(scale) = STATE.with_borrow(|state| state.as_ref().map(|state| state.scale)) else {
        return;
    };
    let layout = Layout::new(scale);
    let style = GetWindowLongW(window, GWL_STYLE) as u32;
    let mut frame = RECT {
        right: layout.width,
        bottom: layout.height,
        ..Default::default()
    };
    AdjustWindowRectExForDpi(&mut frame, style, 0, 0, (scale * 96.0) as u32);
    let width = frame.right - frame.left;
    let height = frame.bottom - frame.top;
    let (x, y) = match suggested {
        Some(rect) => (rect.left, rect.top),
        None => {
            let mut point = POINT::default();
            GetCursorPos(&mut point);
            monitor_info(point).map_or((CW_USEDEFAULT, CW_USEDEFAULT), |info| {
                let area = info.rcWork;
                (
                    (area.left + (area.right - area.left - width) / 2).max(area.left),
                    (area.top + (area.bottom - area.top - height) / 2).max(area.top),
                )
            })
        }
    };
    SetWindowPos(window, null_mut(), x, y, width, height, SWP_NOZORDER | SWP_NOACTIVATE);
}

/// Whether the edited capture shortcut works: it is either Snapshot's live
/// hotkey, or free to register right now.
unsafe fn refresh_capture_status(window: HWND) {
    let draft = STATE.with_borrow(|state| state.as_ref().map(|state| state.draft));
    let Some(draft) = draft else { return };
    let status = if let Some(other) = draft.duplicate_of(0) {
        CaptureStatus::Duplicate(other)
    } else {
        let capture = draft.shortcut(0);
        if crate::active_hotkey() == Some(capture) {
            CaptureStatus::Active
        } else if RegisterHotKey(
            window,
            PROBE_HOTKEY,
            capture.modifiers | MOD_NOREPEAT,
            capture.key,
        ) != 0
        {
            UnregisterHotKey(window, PROBE_HOTKEY);
            CaptureStatus::Available
        } else {
            CaptureStatus::Occupied
        }
    };
    with_state(|state| state.capture_status = status);
}

fn with_state<R>(action: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with_borrow_mut(|state| state.as_mut().map(action))
}

unsafe fn redraw(window: HWND) {
    InvalidateRect(window, null(), 0);
}

unsafe fn animate(window: HWND) {
    let start = with_state(|state| !std::mem::replace(&mut state.animating, true)).unwrap_or(false);
    if start {
        SetTimer(window, ANIMATION_TIMER, FRAME_MS, None);
    }
    redraw(window);
}

/// Advances every animation one frame; stops the timer once all have settled.
unsafe fn tick(window: HWND) {
    let settled = with_state(|state| {
        let mut settled = true;
        let mut step = |value: &mut f32, target: f32| {
            let next = *value + (target - *value) * EASE;
            *value = if (target - next).abs() < 0.01 { target } else { next };
            settled &= *value == target;
        };
        for (target, value) in &mut state.hover {
            let goal = if Some(*target) == state.hovered || Some(*target) == state.pressed {
                1.0
            } else {
                0.0
            };
            step(value, goal);
        }
        state.hover.retain(|_, value| *value > 0.0);
        let goal = if state.draft.start_at_login { 1.0 } else { 0.0 };
        step(&mut state.toggle, goal);
        if state.recording.is_some() {
            state.pulse = (state.pulse + FRAME_MS as f32 / PULSE_MS).fract();
            settled = false;
        }
        if settled {
            state.animating = false;
        }
        settled
    })
    .unwrap_or(true);
    if settled {
        KillTimer(window, ANIMATION_TIMER);
    }
    redraw(window);
}

unsafe fn set_hovered(window: HWND, hovered: Option<Target>) {
    let changed = with_state(|state| {
        if state.hovered == hovered {
            return false;
        }
        state.hovered = hovered;
        if let Some(target) = hovered {
            state.hover.entry(target).or_insert(0.0);
        }
        true
    })
    .unwrap_or(false);
    if changed {
        animate(window);
    }
}

unsafe fn activate(window: HWND, target: Target) {
    match target {
        Target::Key(index) => {
            with_state(|state| {
                state.recording = (state.recording != Some(index)).then_some(index);
                state.held = 0;
                state.pulse = 0.0;
                state.notice = Notice::None;
            });
            animate(window);
        }
        Target::Toggle => {
            with_state(|state| state.draft.start_at_login = !state.draft.start_at_login);
            animate(window);
        }
        Target::Minus | Target::Plus => {
            let magnitude = if hotkey::modifiers() & MOD_SHIFT != 0 { 10 } else { 1 };
            let delta = if target == Target::Minus { -magnitude } else { magnitude };
            with_state(|state| state.draft.step_opacity(delta));
            redraw(window);
        }
        Target::Reset => {
            with_state(|state| {
                state.draft = Draft::defaults();
                state.recording = None;
                state.notice = Notice::Hint("已恢复默认，保存后生效".into());
            });
            refresh_capture_status(window);
            animate(window);
        }
        Target::Cancel => {
            DestroyWindow(window);
        }
        Target::Save => save(window),
    }
}

unsafe fn save(window: HWND) {
    let Some((owner, draft)) = STATE.with_borrow(|state| {
        state
            .as_ref()
            .filter(|state| state.draft.first_duplicate().is_none())
            .map(|state| (state.owner, state.draft))
    }) else {
        return;
    };
    match draft
        .preferences()
        .and_then(|preferences| apply_preferences(owner, preferences, draft.start_at_login))
    {
        Ok(()) => {
            DestroyWindow(window);
        }
        Err(error) => {
            with_state(|state| state.notice = Notice::Error(error.replace('\n', " ")));
            redraw(window);
        }
    }
}

/// Handles a key while a shortcut field is recording. Returns false when idle.
unsafe fn record_key(window: HWND, key: u32) -> bool {
    let Some(index) = STATE.with_borrow(|state| state.as_ref()?.recording) else {
        return false;
    };
    let modifiers = hotkey::modifiers();
    let modifier_key = [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
        .into_iter()
        .any(|vk| u32::from(vk) == key);
    if modifier_key {
        with_state(|state| state.held = modifiers);
    } else if key == u32::from(VK_ESCAPE) && modifiers == 0 {
        with_state(|state| {
            state.recording = None;
            state.notice = Notice::None;
        });
    } else if let Some(shortcut) = Shortcut::from_key(modifiers, key) {
        with_state(|state| {
            state.draft.bindings.set(BINDINGS[index], shortcut);
            state.recording = None;
            state.notice = Notice::None;
        });
        refresh_capture_status(window);
    } else {
        with_state(|state| {
            state.notice = Notice::Hint(
                "需要搭配 Ctrl、Alt、Shift 或 Win，或者直接用 F1–F24（F12 除外）".into(),
            )
        });
    }
    redraw(window);
    true
}

unsafe fn stop_recording(window: HWND) {
    if with_state(|state| state.recording.take().is_some()).unwrap_or(false) {
        redraw(window);
    }
}

fn point(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam & 0xffff) as i16 as i32,
        y: ((lparam >> 16) & 0xffff) as i16 as i32,
    }
}

fn layout() -> Option<Layout> {
    STATE.with_borrow(|state| state.as_ref().map(|state| Layout::new(state.scale)))
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
        WM_TIMER if wparam == ANIMATION_TIMER => tick(window),
        WM_MOUSEMOVE => {
            let start = with_state(|state| !std::mem::replace(&mut state.tracking, true));
            if start == Some(true) {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: window,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut track);
            }
            let hit = layout().and_then(|layout| layout.hit(point(lparam)));
            set_hovered(window, hit);
        }
        WM_MOUSELEAVE => {
            with_state(|state| state.tracking = false);
            set_hovered(window, None);
        }
        WM_SETCURSOR if (lparam & 0xffff) as u32 == HTCLIENT => {
            let clickable = STATE.with_borrow(|state| {
                state.as_ref().is_some_and(|state| {
                    state.hovered.is_some_and(|target| {
                        target != Target::Save || state.draft.first_duplicate().is_none()
                    })
                })
            });
            SetCursor(LoadCursorW(null_mut(), if clickable { IDC_HAND } else { IDC_ARROW }));
            return 1;
        }
        WM_LBUTTONDOWN => {
            SetFocus(window);
            let hit = layout().and_then(|layout| layout.hit(point(lparam)));
            let recording = STATE.with_borrow(|state| state.as_ref()?.recording);
            if recording.is_some() && hit != recording.map(Target::Key) {
                stop_recording(window);
            }
            with_state(|state| {
                state.pressed = hit;
                if let Some(target) = hit {
                    state.hover.entry(target).or_insert(0.0);
                }
            });
            SetCapture(window);
            animate(window);
        }
        WM_LBUTTONUP => {
            let pressed = with_state(|state| state.pressed.take()).flatten();
            ReleaseCapture();
            let hit = layout().and_then(|layout| layout.hit(point(lparam)));
            animate(window);
            if let Some(target) = pressed.filter(|&target| Some(target) == hit) {
                activate(window, target);
            }
        }
        WM_CAPTURECHANGED => {
            with_state(|state| state.pressed = None);
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if record_key(window, wparam as u32) {
                return 0;
            }
            match wparam as u16 {
                VK_ESCAPE => {
                    DestroyWindow(window);
                }
                VK_RETURN => save(window),
                _ => return DefWindowProcW(window, message, wparam, lparam),
            }
        }
        WM_KEYUP | WM_SYSKEYUP => {
            let recording = with_state(|state| {
                state.recording.is_some() && {
                    state.held = hotkey::modifiers();
                    true
                }
            });
            if recording == Some(true) {
                redraw(window);
                return 0;
            }
            return DefWindowProcW(window, message, wparam, lparam);
        }
        WM_SYSCHAR if STATE.with_borrow(|state| {
            state.as_ref().is_some_and(|state| state.recording.is_some())
        }) => {}
        WM_KILLFOCUS => stop_recording(window),
        WM_DPICHANGED => {
            let scale = (wparam & 0xffff) as f32 / 96.0;
            with_state(|state| {
                state.scale = scale;
                state.fonts = Fonts::new(scale);
            });
            place(window, Some(*(lparam as *const RECT)));
            redraw(window);
        }
        WM_CLOSE => {
            DestroyWindow(window);
        }
        WM_DESTROY => {
            KillTimer(window, ANIMATION_TIMER);
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
    if client.right <= 0 || client.bottom <= 0 {
        return;
    }
    let Ok(surface) = crate::capture::Surface::new(target, client.right, client.bottom) else {
        return;
    };
    STATE.with_borrow(|state| {
        if let Some(state) = state {
            Painter {
                dc: surface.dc,
                state,
                layout: Layout::new(state.scale),
            }
            .paint(client);
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

struct Painter<'a> {
    dc: HDC,
    state: &'a State,
    layout: Layout,
}

impl Painter<'_> {
    fn s(&self, value: i32) -> i32 {
        self.layout.s(value)
    }

    fn f(&self, value: f32) -> f32 {
        value * self.layout.scale
    }

    fn hover(&self, target: Target) -> f32 {
        self.state.hover.get(&target).copied().unwrap_or(0.0)
    }

    unsafe fn paint(&self, client: RECT) {
        fill(self.dc, client, BACKGROUND);
        let layout = &self.layout;
        let header = RECT {
            left: self.s(PAD),
            top: 0,
            right: layout.width - self.s(PAD),
            bottom: self.s(HEADER),
        };
        text(self.dc, header, "设置", self.state.fonts.title, TEXT, DT_LEFT | DT_VCENTER);
        text(
            self.dc,
            header,
            &format!("Snapshot v{}", env!("CARGO_PKG_VERSION")),
            self.state.fonts.small,
            TERTIARY,
            DT_RIGHT | DT_VCENTER,
        );
        for (rect, label) in [
            (layout.capture_label, "截图"),
            (layout.toolbar_label, "截图工具栏"),
            (layout.general_label, "常规"),
        ] {
            let rect = RECT {
                left: rect.left + self.s(4),
                bottom: rect.bottom - self.s(6),
                ..rect
            };
            text(self.dc, rect, label, self.state.fonts.small, SECONDARY, DT_LEFT | DT_BOTTOM);
        }
        for card in [layout.hero, layout.toolbar, layout.general] {
            self.card(card);
        }
        self.hero();
        self.toolbar_rows();
        self.general();
        self.footer();
    }

    unsafe fn card(&self, rect: RECT) {
        let Some(graphics) = Graphics::new(self.dc) else {
            fill(self.dc, rect, CARD);
            return;
        };
        let bounds = to_f(rect);
        let radius = self.f(10.0);
        graphics.fill_round(bounds, radius, argb(CARD, 255));
        graphics.stroke_round(inset(bounds, 0.5), radius, argb(CARD_BORDER, 255), 1.0);
    }

    unsafe fn hero(&self) {
        let card = self.layout.hero;
        let tile_size = self.s(40);
        let tile = RECT {
            left: card.left + self.s(18),
            top: (card.top + card.bottom - tile_size) / 2,
            right: card.left + self.s(18) + tile_size,
            bottom: (card.top + card.bottom + tile_size) / 2,
        };
        if let Some(graphics) = Graphics::new(self.dc) {
            graphics.fill_round(to_f(tile), self.f(10.0), argb(ACCENT_SOFT, 255));
        }
        let icon = inflate(tile, -self.s(10));
        icons::paint(self.dc, icon, Icon::Rectangle, ACCENT);

        let left = tile.right + self.s(14);
        let middle = (card.top + card.bottom) / 2;
        let title = RECT {
            left,
            top: middle - self.s(22),
            right: self.layout.keys[0].left,
            bottom: middle,
        };
        text(self.dc, title, "开始截图", self.state.fonts.heading, TEXT, DT_LEFT | DT_BOTTOM);
        let description = RECT {
            top: middle + self.s(3),
            bottom: middle + self.s(23),
            ..title
        };
        text(
            self.dc,
            description,
            "全局快捷键，在任何程序里按下都能截图",
            self.state.fonts.small,
            SECONDARY,
            DT_LEFT | DT_TOP,
        );

        self.key_field(0, self.s(28));

        let keys = self.layout.keys[0];
        let status = RECT {
            left: keys.left - self.s(80),
            top: keys.bottom + self.s(4),
            right: keys.right - self.s(6),
            bottom: keys.bottom + self.s(24),
        };
        let (color, label, good) = if self.state.recording == Some(0) {
            (TERTIARY, "按下新的组合键，Esc 取消".to_string(), None)
        } else {
            match self.state.capture_status {
                CaptureStatus::Active => (SUCCESS, "已生效".into(), Some(true)),
                CaptureStatus::Available => (SUCCESS, "可以使用，保存后生效".into(), Some(true)),
                CaptureStatus::Occupied => {
                    (WARNING, "已被其他程序占用，换一个组合键试试".into(), Some(false))
                }
                CaptureStatus::Duplicate(other) => (
                    WARNING,
                    format!("和「{}」重复", BINDINGS[other].label()),
                    Some(false),
                ),
            }
        };
        let width = measure(self.dc, &label, self.state.fonts.small);
        let mut label_rect = status;
        label_rect.left = label_rect.right - width;
        text(self.dc, label_rect, &label, self.state.fonts.small, color, DT_LEFT | DT_VCENTER);
        if let Some(good) = good {
            let size = self.s(14);
            let center_y = (status.top + status.bottom) / 2;
            let badge = RECT {
                left: label_rect.left - self.s(5) - size,
                top: center_y - size / 2,
                right: label_rect.left - self.s(5),
                bottom: center_y - size / 2 + size,
            };
            status_badge(self.dc, badge, color, good);
        }
    }

    unsafe fn toolbar_rows(&self) {
        let layout = &self.layout;
        let card = layout.toolbar;
        let middle = (card.left + card.right) / 2;
        fill(
            self.dc,
            RECT {
                left: middle,
                top: card.top + self.s(14),
                right: middle + 1,
                bottom: card.bottom - self.s(14),
            },
            DIVIDER,
        );
        for index in 1..BINDINGS.len() {
            let row = layout.rows[index];
            if (index - 1) as i32 % ROWS != 0 {
                fill(
                    self.dc,
                    RECT {
                        left: row.left + self.s(12),
                        top: row.top,
                        right: row.right - self.s(12),
                        bottom: row.top + 1,
                    },
                    DIVIDER,
                );
            }
            let size = self.s(18);
            let icon = RECT {
                left: row.left + self.s(12),
                top: (row.top + row.bottom - size) / 2,
                right: row.left + self.s(12) + size,
                bottom: (row.top + row.bottom - size) / 2 + size,
            };
            icons::paint(self.dc, icon, binding_icon(BINDINGS[index]), ICON);
            let label = RECT {
                left: icon.right + self.s(10),
                right: layout.keys[index].left,
                ..row
            };
            text(
                self.dc,
                label,
                BINDINGS[index].label(),
                self.state.fonts.body,
                TEXT,
                DT_LEFT | DT_VCENTER,
            );
            self.key_field(index, self.s(24));
        }
    }

    /// A shortcut drawn as keycaps, right-aligned in its field.
    unsafe fn key_field(&self, index: usize, cap_height: i32) {
        let field = self.layout.keys[index];
        let target = Target::Key(index);
        let recording = self.state.recording == Some(index);
        let duplicate = self.state.draft.duplicate_of(index).is_some();
        let hover = self.hover(target);
        let pressed = self.state.pressed == Some(target);
        let Some(graphics) = Graphics::new(self.dc) else {
            return;
        };
        let bounds = to_f(field);
        let radius = self.f(8.0);
        if recording {
            // Breathing outline while waiting for keys.
            let wave = 0.5 - 0.5 * (self.state.pulse * std::f32::consts::TAU).cos();
            graphics.fill_round(bounds, radius, argb(ACCENT_SOFT, 255));
            let alpha = (110.0 + 145.0 * wave) as u8;
            graphics.stroke_round(inset(bounds, 0.75), radius, argb(ACCENT, alpha), 1.5);
        } else {
            let base = if duplicate { WARNING_SOFT } else { CARD };
            let face = if pressed { PRESSED } else { mix(base, HOVER, hover) };
            if face != CARD {
                graphics.fill_round(bounds, radius, argb(face, 255));
            }
            if duplicate {
                graphics.stroke_round(inset(bounds, 0.75), radius, argb(WARNING, 200), 1.5);
            }
        }
        drop(graphics);

        let parts: Vec<String> = if recording {
            let held = Shortcut {
                modifiers: self.state.held,
                key: 0,
            };
            if self.state.held == 0 {
                let rect = RECT {
                    right: field.right - self.s(12),
                    ..field
                };
                text(self.dc, rect, "按下组合键…", self.state.fonts.small, ACCENT, DT_RIGHT | DT_VCENTER);
                return;
            }
            let mut parts: Vec<String> =
                modifier_names(held.modifiers).into_iter().map(String::from).collect();
            parts.push("…".into());
            parts
        } else {
            self.state
                .draft
                .shortcut(index)
                .label()
                .split('+')
                .map(String::from)
                .collect()
        };
        let font = self.state.fonts.key;
        let gap = self.s(4);
        let padding = self.s(9);
        let widths: Vec<i32> = parts
            .iter()
            .map(|part| (measure(self.dc, part, font) + padding * 2).max(cap_height))
            .collect();
        let mut right = field.right - self.s(8);
        let top = (field.top + field.bottom - cap_height) / 2 - self.s(1);
        let Some(graphics) = Graphics::new(self.dc) else {
            return;
        };
        let mut caps = Vec::new();
        for width in widths.iter().rev() {
            let cap = RECT {
                left: right - width,
                top,
                right,
                bottom: top + cap_height,
            };
            let bounds = to_f(cap);
            let radius = self.f(5.0);
            let depth = self.f(2.0);
            graphics.fill_round(
                [bounds[0], bounds[1] + depth, bounds[2], bounds[3] + depth],
                radius,
                argb(KEY_DEPTH, 255),
            );
            graphics.fill_round(bounds, radius, argb(KEY_FACE, 255));
            graphics.stroke_round(inset(bounds, 0.5), radius, argb(KEY_BORDER, 255), 1.0);
            caps.push(cap);
            right -= width + gap;
        }
        drop(graphics);
        let color = if duplicate && !recording { WARNING } else { TEXT };
        for (cap, part) in caps.into_iter().rev().zip(&parts) {
            text(self.dc, cap, part, font, color, DT_CENTER | DT_VCENTER);
        }
    }

    unsafe fn general(&self) {
        let card = self.layout.general;
        let row = self.s(GENERAL_ROW);
        let rows = [
            ("开机自动启动", "登录 Windows 后在托盘待命"),
            ("贴图透明度步长", "在贴图上 Ctrl+滚轮，每格改变的不透明度"),
        ];
        for (index, (title, description)) in rows.into_iter().enumerate() {
            let top = card.top + row * index as i32;
            if index > 0 {
                fill(
                    self.dc,
                    RECT {
                        left: card.left + self.s(18),
                        top,
                        right: card.right - self.s(18),
                        bottom: top + 1,
                    },
                    DIVIDER,
                );
            }
            let middle = top + row / 2;
            let left = card.left + self.s(18);
            let right = self.layout.stepper.left - self.s(12);
            text(
                self.dc,
                RECT { left, top: middle - self.s(21), right, bottom: middle },
                title,
                self.state.fonts.body,
                TEXT,
                DT_LEFT | DT_BOTTOM,
            );
            text(
                self.dc,
                RECT { left, top: middle + self.s(2), right, bottom: middle + self.s(21) },
                description,
                self.state.fonts.small,
                SECONDARY,
                DT_LEFT | DT_TOP,
            );
        }
        self.toggle();
        self.stepper();
    }

    unsafe fn toggle(&self) {
        let rect = self.layout.toggle;
        let Some(graphics) = Graphics::new(self.dc) else {
            return;
        };
        let t = self.state.toggle;
        let hover = self.hover(Target::Toggle);
        let off = mix(TRACK_OFF, rgb(0xb5, 0xba, 0xc2), hover);
        let on = mix(ACCENT, ACCENT_DARK, hover);
        let bounds = to_f(rect);
        let height = bounds[3] - bounds[1];
        graphics.fill_round(bounds, height / 2.0, argb(mix(off, on, t), 255));
        let margin = self.f(3.0);
        let knob = height - margin * 2.0;
        let travel = bounds[2] - bounds[0] - margin * 2.0 - knob;
        let left = bounds[0] + margin + travel * t;
        let top = bounds[1] + margin;
        graphics.fill_ellipse(
            [left, top + self.f(0.8), left + knob, top + knob + self.f(0.8)],
            argb(0, 40),
        );
        graphics.fill_ellipse([left, top, left + knob, top + knob], argb(CARD, 255));
    }

    unsafe fn stepper(&self) {
        let layout = &self.layout;
        if let Some(graphics) = Graphics::new(self.dc) {
            let radius = self.f(8.0);
            graphics.fill_round(to_f(layout.stepper), radius, argb(KEY_FACE, 255));
            for (target, rect) in [(Target::Minus, layout.minus), (Target::Plus, layout.plus)] {
                let hover = self.hover(target);
                let face = if self.state.pressed == Some(target) {
                    PRESSED
                } else {
                    mix(KEY_FACE, HOVER, hover)
                };
                graphics.fill_round(to_f(inflate(rect, -self.s(2))), self.f(6.0), argb(face, 255));
            }
            graphics.stroke_round(
                inset(to_f(layout.stepper), 0.5),
                radius,
                argb(KEY_BORDER, 255),
                1.0,
            );
            // Minus and plus glyphs as crisp bars.
            let bar = self.f(10.0);
            let thick = self.f(1.6).max(1.2);
            for (rect, vertical) in [(layout.minus, false), (layout.plus, true)] {
                let center = center_f(rect);
                graphics.fill_round(
                    [center.0 - bar / 2.0, center.1 - thick / 2.0, center.0 + bar / 2.0, center.1 + thick / 2.0],
                    thick / 2.0,
                    argb(ICON, 255),
                );
                if vertical {
                    graphics.fill_round(
                        [center.0 - thick / 2.0, center.1 - bar / 2.0, center.0 + thick / 2.0, center.1 + bar / 2.0],
                        thick / 2.0,
                        argb(ICON, 255),
                    );
                }
            }
        }
        let value = RECT {
            left: layout.minus.right,
            right: layout.plus.left,
            ..layout.stepper
        };
        text(
            self.dc,
            value,
            &format!("{}%", self.state.draft.opacity_step),
            self.state.fonts.key,
            TEXT,
            DT_CENTER | DT_VCENTER,
        );
    }

    unsafe fn footer(&self) {
        let layout = &self.layout;
        fill(
            self.dc,
            RECT {
                left: 0,
                top: layout.footer,
                right: layout.width,
                bottom: layout.footer + 1,
            },
            CARD_BORDER,
        );
        fill(
            self.dc,
            RECT {
                left: 0,
                top: layout.footer + 1,
                right: layout.width,
                bottom: layout.height,
            },
            CARD,
        );
        let blocked = self.state.draft.first_duplicate();
        let Some(graphics) = Graphics::new(self.dc) else {
            return;
        };
        let radius = self.f(8.0);
        // Reset: quiet text button.
        let hover = self.hover(Target::Reset);
        if hover > 0.0 || self.state.pressed == Some(Target::Reset) {
            let face = if self.state.pressed == Some(Target::Reset) { PRESSED } else { mix(CARD, HOVER, hover) };
            graphics.fill_round(to_f(layout.reset), radius, argb(face, 255));
        }
        // Cancel: outlined.
        let hover = self.hover(Target::Cancel);
        let face = if self.state.pressed == Some(Target::Cancel) { PRESSED } else { mix(CARD, HOVER, hover) };
        graphics.fill_round(to_f(layout.cancel), radius, argb(face, 255));
        graphics.stroke_round(inset(to_f(layout.cancel), 0.5), radius, argb(KEY_BORDER, 255), 1.0);
        // Save: filled accent, muted while duplicates block it.
        let hover = self.hover(Target::Save);
        let face = if blocked.is_some() {
            ACCENT_MUTED
        } else if self.state.pressed == Some(Target::Save) {
            mix(ACCENT_DARK, 0, 0.12)
        } else {
            mix(ACCENT, ACCENT_DARK, hover)
        };
        graphics.fill_round(to_f(layout.save), radius, argb(face, 255));
        drop(graphics);
        text(self.dc, layout.reset, "恢复默认", self.state.fonts.body, SECONDARY, DT_CENTER | DT_VCENTER);
        text(self.dc, layout.cancel, "取消", self.state.fonts.body, TEXT, DT_CENTER | DT_VCENTER);
        text(self.dc, layout.save, "保存", self.state.fonts.heading, CARD, DT_CENTER | DT_VCENTER);

        let (message, color) = match (&self.state.notice, blocked) {
            (Notice::Error(error), _) => (error.clone(), ERROR),
            (Notice::Hint(hint), _) => (hint.clone(), WARNING),
            (Notice::None, Some((first, second))) => (
                format!(
                    "「{}」和「{}」都是 {}",
                    BINDINGS[first].label(),
                    BINDINGS[second].label(),
                    self.state.draft.shortcut(first).label()
                ),
                WARNING,
            ),
            (Notice::None, None) => ("点击按键可以修改 · Enter 保存 · Esc 关闭".into(), TERTIARY),
        };
        let rect = RECT {
            left: layout.reset.right + self.s(10),
            top: layout.reset.top,
            right: layout.cancel.left - self.s(12),
            bottom: layout.reset.bottom,
        };
        text(
            self.dc,
            rect,
            &message,
            self.state.fonts.small,
            color,
            DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }
}

fn binding_icon(binding: Binding) -> Icon {
    match binding {
        Binding::Capture | Binding::Rectangle => Icon::Rectangle,
        Binding::Text => Icon::Text,
        Binding::Brush => Icon::Brush,
        Binding::Mosaic => Icon::Mosaic,
        Binding::Undo => Icon::Undo,
        Binding::Redo => Icon::Redo,
        Binding::LongCapture => Icon::LongCapture,
        Binding::Copy => Icon::Copy,
        Binding::Save => Icon::Save,
        Binding::Pin => Icon::Pin,
    }
}

fn modifier_names(modifiers: u32) -> Vec<&'static str> {
    [(MOD_CONTROL, "Ctrl"), (MOD_ALT, "Alt"), (MOD_SHIFT, "Shift"), (MOD_WIN, "Win")]
        .into_iter()
        .filter(|(mask, _)| modifiers & mask != 0)
        .map(|(_, name)| name)
        .collect()
}

/// A round check or exclamation badge.
unsafe fn status_badge(dc: HDC, rect: RECT, color: u32, good: bool) {
    let Some(graphics) = Graphics::new(dc) else {
        return;
    };
    let bounds = to_f(rect);
    graphics.fill_ellipse(bounds, argb(color, 255));
    let size = bounds[2] - bounds[0];
    let (cx, cy) = ((bounds[0] + bounds[2]) / 2.0, (bounds[1] + bounds[3]) / 2.0);
    if good {
        if let Some(pen) = graphics.pen(CARD, (size * 0.13).max(1.4)) {
            graphics.lines(
                &pen,
                &[
                    PointF { X: cx - size * 0.24, Y: cy + size * 0.01 },
                    PointF { X: cx - size * 0.06, Y: cy + size * 0.19 },
                    PointF { X: cx + size * 0.25, Y: cy - size * 0.17 },
                ],
            );
        }
    } else {
        let width = (size * 0.14).max(1.4);
        graphics.fill_round(
            [cx - width / 2.0, cy - size * 0.28, cx + width / 2.0, cy + size * 0.07],
            width / 2.0,
            argb(CARD, 255),
        );
        graphics.fill_ellipse(
            [cx - width * 0.6, cy + size * 0.16, cx + width * 0.6, cy + size * 0.16 + width * 1.2],
            argb(CARD, 255),
        );
    }
}

fn to_f(rect: RECT) -> [f32; 4] {
    [rect.left as f32, rect.top as f32, rect.right as f32, rect.bottom as f32]
}

fn inset([left, top, right, bottom]: [f32; 4], amount: f32) -> [f32; 4] {
    [left + amount, top + amount, right - amount, bottom - amount]
}

fn inflate(rect: RECT, amount: i32) -> RECT {
    RECT {
        left: rect.left - amount,
        top: rect.top - amount,
        right: rect.right + amount,
        bottom: rect.bottom + amount,
    }
}

fn center_f(rect: RECT) -> (f32, f32) {
    (
        (rect.left + rect.right) as f32 / 2.0,
        (rect.top + rect.bottom) as f32 / 2.0,
    )
}

/// COLORREF plus alpha to GDI+ ARGB.
fn argb(color: u32, alpha: u8) -> u32 {
    (u32::from(alpha) << 24) | ((color & 0xff) << 16) | (color & 0xff00) | ((color >> 16) & 0xff)
}

fn mix(from: u32, to: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    [0, 8, 16].into_iter().fold(0, |color, shift| {
        let a = ((from >> shift) & 0xff) as f32;
        let b = ((to >> shift) & 0xff) as f32;
        color | (((a + (b - a) * t).round() as u32) << shift)
    })
}

unsafe fn fill(dc: HDC, rect: RECT, color: u32) {
    SetDCBrushColor(dc, color);
    FillRect(dc, &rect, GetStockObject(DC_BRUSH));
}

unsafe fn text(dc: HDC, mut rect: RECT, value: &str, font: HFONT, color: u32, format: u32) {
    let value = wide(value);
    SelectObject(dc, font);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, color);
    DrawTextW(
        dc,
        value.as_ptr(),
        -1,
        &mut rect,
        format | DT_SINGLELINE | DT_NOPREFIX,
    );
}

unsafe fn measure(dc: HDC, value: &str, font: HFONT) -> i32 {
    let units: Vec<u16> = value.encode_utf16().collect();
    let previous = SelectObject(dc, font);
    let mut size = SIZE::default();
    GetTextExtentPoint32W(dc, units.as_ptr(), units.len() as i32, &mut size);
    SelectObject(dc, previous);
    size.cx
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

    /// Renders the window in its main states to `%TEMP%\\snapshot-preview`.
    /// Run with `cargo test preview -- --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        use crate::capture::{Desktop, Surface};
        let folder = std::env::temp_dir().join("snapshot-preview");
        std::fs::create_dir_all(&folder).unwrap();
        let mut duplicate = Draft::defaults();
        duplicate.bindings.set(Binding::Copy, Shortcut::parse("Ctrl+S").unwrap());
        let cases: [(&str, f32, Draft, CaptureStatus, Option<usize>, u32); 5] = [
            ("active", 1.0, Draft::defaults(), CaptureStatus::Active, None, 0),
            ("occupied", 1.0, Draft::defaults(), CaptureStatus::Occupied, None, 0),
            ("recording", 1.0, Draft::defaults(), CaptureStatus::Active, Some(0), MOD_CONTROL | MOD_SHIFT),
            ("duplicate", 1.0, duplicate, CaptureStatus::Active, None, 0),
            ("scale150", 1.5, Draft { start_at_login: false, ..Draft::defaults() }, CaptureStatus::Available, None, 0),
        ];
        unsafe {
            for (name, scale, draft, status, recording, held) in cases {
                let state = State {
                    owner: null_mut(),
                    draft,
                    capture_status: status,
                    recording,
                    held,
                    hovered: None,
                    pressed: None,
                    hover: HashMap::from([(Target::Key(1), 1.0)]),
                    toggle: if draft.start_at_login { 1.0 } else { 0.0 },
                    pulse: 0.5,
                    notice: Notice::None,
                    scale,
                    fonts: Fonts::new(scale),
                    tracking: false,
                    animating: false,
                };
                let layout = Layout::new(scale);
                let bounds = RECT { right: layout.width, bottom: layout.height, ..Default::default() };
                let surface = Surface::new(null_mut(), bounds.right, bounds.bottom).unwrap();
                Painter { dc: surface.dc, state: &state, layout }.paint(bounds);
                GdiFlush();
                let mut image = Desktop { bounds, surface }.crop(bounds).unwrap();
                for pixel in image.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                image.save_png(&folder.join(format!("settings-{name}.png"))).unwrap();
            }
            for warning in [false, true] {
                crate::app_icon::render(64, warning)
                    .save_png(&folder.join(format!("icon-{warning}.png")))
                    .unwrap();
            }
        }
    }

    #[test]
    fn draft_resets_to_defaults_and_reports_duplicates() {
        let mut draft = Draft::defaults();
        assert!(draft.start_at_login, "start at login is on by default");
        assert_eq!(draft.preferences().unwrap(), Preferences::default());
        draft.step_opacity(-10);
        assert_eq!(draft.opacity_step, 1);
        draft.step_opacity(500);
        assert_eq!(draft.opacity_step, 100);
        let copy = Binding::Copy as usize;
        let save = Binding::Save as usize;
        draft.bindings.set(Binding::Copy, Shortcut::parse("Ctrl+S").unwrap());
        assert_eq!(draft.duplicate_of(copy), Some(save));
        assert_eq!(draft.first_duplicate(), Some((copy, save)));
        assert!(draft.preferences().is_err());
    }

    #[test]
    fn every_target_is_hit_at_its_center_on_any_scale() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let layout = Layout::new(scale);
            for (target, rect) in layout.targets() {
                let center = POINT {
                    x: (rect.left + rect.right) / 2,
                    y: (rect.top + rect.bottom) / 2,
                };
                assert_eq!(layout.hit(center), Some(target), "{target:?} at {scale}");
                assert!(rect.right <= layout.width && rect.bottom <= layout.height);
            }
            assert!(layout.height < (720.0 * scale) as i32, "fits a 1080p screen");
        }
    }

    #[test]
    fn clicking_a_key_records_and_escape_cancels_then_closes() {
        unsafe {
            let instance = GetModuleHandleW(null());
            let _ = register(instance);
            let window = CreateWindowExW(
                0,
                CLASS,
                null(),
                WS_OVERLAPPED,
                0,
                0,
                WIDTH,
                WIDTH,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(!window.is_null());
            install(window, null_mut(), Draft::defaults());
            let layout = layout().unwrap();
            let key = layout.keys[Binding::Copy as usize];
            let lparam = (((key.top + key.bottom) / 2) << 16 | (key.left + key.right) / 2) as LPARAM;
            SendMessageW(window, WM_LBUTTONDOWN, 0, lparam);
            SendMessageW(window, WM_LBUTTONUP, 0, lparam);
            assert_eq!(
                STATE.with_borrow(|state| state.as_ref().unwrap().recording),
                Some(Binding::Copy as usize)
            );
            // Painting while recording must not panic.
            paint_window(window, GetDC(window));
            SendMessageW(window, WM_KEYDOWN, VK_ESCAPE as usize, 0);
            assert!(STATE.with_borrow(|state| state.as_ref().unwrap().recording.is_none()));
            let toggle = layout.toggle;
            let lparam = (((toggle.top + toggle.bottom) / 2) << 16 | (toggle.left + toggle.right) / 2) as LPARAM;
            SendMessageW(window, WM_LBUTTONDOWN, 0, lparam);
            SendMessageW(window, WM_LBUTTONUP, 0, lparam);
            assert!(!STATE.with_borrow(|state| state.as_ref().unwrap().draft.start_at_login));
            SendMessageW(window, WM_KEYDOWN, VK_ESCAPE as usize, 0);
            assert!(STATE.with_borrow(|state| state.is_none()), "Esc closes when idle");
        }
    }
}
