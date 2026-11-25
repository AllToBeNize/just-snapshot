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
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::*,
        UI::{
            Controls::WM_MOUSELEAVE,
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{
                RegisterHotKey, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent,
                UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
                TME_LEAVE, TRACKMOUSEEVENT, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RETURN,
                VK_RWIN, VK_SHIFT,
            },
            WindowsAndMessaging::*,
        },
    },
};

use crate::{
    capture::Image,
    apply_preferences, global_hotkey, hotkey,
    hotkey::{Binding, Bindings, Preferences, Shortcut, BINDINGS, DEFAULT_OPACITY_STEP},
    icons::{self, Icon},
    overlay_ui::monitor_info,
    pinned, retry_hotkey,
    selection_geometry::{contains, mouse_point},
    startup,
    translate_settings,
    gdiplus::Graphics,
    theme::{self, argb, mix, Palette, Theme, THEMES},
    ui::{self, Badge, Fonts},
};

const CLASS: *const u16 = w!("Snapshot.Settings");
const PROBE_HOTKEY: i32 = 0x7ff0;
const ANIMATION_TIMER: usize = 1;
const FRAME_MS: u32 = 15;
/// Share of the remaining distance covered per frame: a quick ease-out.
const EASE: f32 = 0.32;
const PULSE_MS: f32 = 1100.0;

// Layout in 96-DPI pixels.
/// The settings themselves; the mascot stands in a column to their right.
const WIDTH: i32 = 820;
const MASCOT: i32 = 236;
const TOTAL: i32 = WIDTH + MASCOT;
const MASCOT_ART: &[u8] = include_bytes!("../assets/character/settings.png");
const PAD: i32 = 24;
const HEADER: i32 = 52;
const LABEL: i32 = 22;
const SECTION_GAP: i32 = 8;
const HERO: i32 = 124;
const ROW: i32 = 35;
const ROWS: i32 = 7;
const COLUMNS: i32 = 3;
const CARD_INSET: i32 = 6;
const GENERAL_ROW: i32 = 44;
const GENERAL_ROWS: i32 = 3;
const FOOTER: i32 = 60;
const HERO_KEYS: i32 = 220;
const ROW_KEYS: i32 = 136;
const CHIP: i32 = 78;
const CHIP_GAP: i32 = 6;
/// Field index of the optional second capture shortcut, after every binding.
const SECONDARY: usize = BINDINGS.len();
/// Bindings from here on belong to the recording tab.
const RECORD_START: usize = Binding::Record as usize;
/// The two tabs: screenshots, and recording with the input display.
const TABS: [&str; 2] = ["截图", "录像与按键显示"];
const ANCHOR_CELL: (i32, i32) = (30, 20);
const FIELDS: usize = BINDINGS.len() + 1;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Target {
    Key(usize),
    Toggle,
    /// "自动检查更新".
    AutoUpdate,
    Minus,
    Plus,
    Theme(usize),
    ClearSecondary,
    /// "服务设置" beside the translate shortcut.
    Service,
    /// The engine settings beside the image search shortcut.
    SearchService,
    /// The recognition engine (快速 / 准确) beside the extract text shortcut.
    OcrService,
    /// Opens the save settings: folder, file names, auto-save, shadow, border.
    Output,
    Tab(usize),
    /// Recording frame rate: 30 or 60.
    Fps(usize),
    Quality(usize),
    /// Choose the recording folder.
    Folder,
    Cursor,
    /// Count 3, 2, 1 before an area recording starts.
    Countdown,
    SystemAudio,
    Microphone,
    /// Input panel switches and settings, by panel index.
    PanelVisible(usize),
    /// Puts a panel at one of nine places; dragging it still moves it freely.
    PanelAnchor(usize, usize),
    PanelSize(usize, i32),
    PanelOpacity(usize, i32),
    Close,
    Reset,
    Cancel,
    Save,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CaptureStatus {
    Unset,
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
    auto_update: bool,
    theme: Theme,
    secondary: Option<Shortcut>,
    record: crate::recorder::Options,
}

impl Draft {
    fn defaults() -> Self {
        Self {
            bindings: hotkey::DEFAULT,
            opacity_step: DEFAULT_OPACITY_STEP,
            start_at_login: true,
            auto_update: true,
            theme: Theme::default(),
            secondary: None,
            record: crate::recorder::Options::default(),
        }
    }

    /// The shortcut in field `index`: a binding, or the optional second capture key.
    fn slot(&self, index: usize) -> Option<Shortcut> {
        if index == SECONDARY {
            self.secondary
        } else {
            Some(self.bindings.get(BINDINGS[index]))
        }
    }

    fn set_slot(&mut self, index: usize, shortcut: Shortcut) {
        if index == SECONDARY {
            self.secondary = Some(shortcut);
        } else {
            self.bindings.set(BINDINGS[index], shortcut);
        }
    }

    fn duplicate_of(&self, index: usize) -> Option<usize> {
        let shortcut = self.slot(index)?;
        (0..FIELDS).find(|&other| other != index && self.slot(other) == Some(shortcut))
    }

    fn first_duplicate(&self) -> Option<(usize, usize)> {
        (0..FIELDS).find_map(|index| {
            self.duplicate_of(index)
                .filter(|&other| other > index)
                .map(|other| (index, other))
        })
    }

    fn step_opacity(&mut self, delta: i32) {
        self.opacity_step = (i32::from(self.opacity_step) + delta).clamp(1, 100) as u8;
    }

    fn preferences(&self) -> Result<Preferences, String> {
        let preferences = Preferences {
            bindings: self.bindings.validate()?,
            opacity_step: self.opacity_step,
            theme: self.theme,
            capture_secondary: self.secondary,
            auto_update: self.auto_update,
            // Switched from the capture with its key, not in this window.
            show_hints: crate::show_hints(),
        };
        preferences.validate()?;
        Ok(preferences)
    }
}

struct State {
    owner: HWND,
    draft: Draft,
    /// Status of the main and the second capture shortcut.
    capture_status: [CaptureStatus; 2],
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
    /// The mascot art, decoded once per window.
    mascot: Option<Image>,
    tab: usize,
    /// The recording folder as chosen in this window; saved with the rest.
    record_folder: String,
}

thread_local! {
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Every interactive rect, computed from the DPI scale alone so painting and
/// hit testing always agree.
struct Layout {
    scale: f32,
    tab: usize,
    tabs: [RECT; 2],
    record_label: RECT,
    record_keys: RECT,
    record_options: RECT,
    fps: [RECT; 2],
    quality: [RECT; 3],
    cursor: RECT,
    countdown: RECT,
    folder: RECT,
    folder_button: RECT,
    system_audio: RECT,
    microphone: RECT,
    panels_label: RECT,
    panels: RECT,
    panel_visible: [RECT; 2],
    anchors: [[RECT; 9]; 2],
    panel_size: [[RECT; 2]; 2],
    panel_opacity: [[RECT; 2]; 2],
    width: i32,
    height: i32,
    header: RECT,
    close: RECT,
    capture_label: RECT,
    hero: RECT,
    toolbar_label: RECT,
    toolbar: RECT,
    general_label: RECT,
    general: RECT,
    footer: i32,
    keys: [RECT; FIELDS],
    rows: [RECT; BINDINGS.len()],
    clear: RECT,
    /// The column the mascot stands in.
    mascot: RECT,
    service: RECT,
    search_service: RECT,
    ocr_service: RECT,
    toggle: RECT,
    auto_update: RECT,
    output: RECT,
    stepper: RECT,
    minus: RECT,
    plus: RECT,
    themes: [RECT; THEMES.len()],
    reset: RECT,
    cancel: RECT,
    save: RECT,
}

impl Layout {
    fn new(scale: f32) -> Self {
        Self::for_tab(scale, 0)
    }

    fn for_tab(scale: f32, tab: usize) -> Self {
        let s = |value: i32| (value as f32 * scale).round() as i32;
        let rect = |left: i32, top: i32, right: i32, bottom: i32| RECT {
            left: s(left),
            top: s(top),
            right: s(right),
            bottom: s(bottom),
        };
        let header = rect(0, 0, TOTAL, HEADER);
        let close = rect(TOTAL - 12 - 36, 10, TOTAL - 12, 46);
        let mut y = HEADER;
        let capture_label = rect(PAD, y, WIDTH - PAD, y + LABEL);
        y += LABEL;
        let hero_top = y;
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
        let general = rect(PAD, y, WIDTH - PAD, y + GENERAL_ROW * GENERAL_ROWS);
        y += GENERAL_ROW * GENERAL_ROWS + SECTION_GAP + 4;
        let footer = s(y);
        let height = s(y + FOOTER);

        let mut keys = [RECT::default(); FIELDS];
        let mut rows = [RECT::default(); BINDINGS.len()];
        rows[0] = hero;
        keys[0] = rect(
            WIDTH - PAD - 16 - HERO_KEYS,
            hero_top + 12,
            WIDTH - PAD - 16,
            hero_top + 12 + 34,
        );
        // The second capture key sits on its own row below the main one.
        keys[SECONDARY] = rect(
            WIDTH - PAD - 16 - HERO_KEYS,
            hero_top + 78,
            WIDTH - PAD - 16,
            hero_top + 78 + 34,
        );
        let clear_left = WIDTH - PAD - 16 - HERO_KEYS - 6 - 28;
        let clear = rect(clear_left, hero_top + 81, clear_left + 28, hero_top + 81 + 28);
        let column = (WIDTH - PAD * 2 - CARD_INSET * 2) / COLUMNS;
        for index in 1..RECORD_START {
            let slot = (index - 1) as i32;
            let left = PAD + CARD_INSET + (slot / ROWS) * column;
            let top = toolbar_top + CARD_INSET + (slot % ROWS) * ROW;
            rows[index] = rect(left, top, left + column, top + ROW);
            let right = left + column - 8;
            keys[index] = rect(right - ROW_KEYS, top + 6, right, top + ROW - 6);
        }

        // A gear after a label of `characters` 14 px characters: icon 12 + 20,
        // gap 10, the label, gap 4.
        let gear = |row: RECT, characters: i32| {
            let left = row.left + s(12 + 20 + 10 + 14 * characters + 4);
            let top = (row.top + row.bottom) / 2 - s(12);
            RECT { left, top, right: left + s(24), bottom: top + s(24) }
        };
        let service = gear(rows[Binding::Translate as usize], 2);
        let search_service = gear(rows[Binding::SearchImage as usize], 2);
        let ocr_service = gear(rows[Binding::ExtractText as usize], 4);
        // The shortcut field starts after the gear in rows that have one.
        for (binding, gear) in [(Binding::Translate, service), (Binding::SearchImage, search_service), (Binding::ExtractText, ocr_service)] {
            let key = &mut keys[binding as usize];
            key.left = key.left.max(gear.right + s(4));
        }

        let right = WIDTH - PAD - 18;
        let row_top = |row: i32, height: i32| general_top + GENERAL_ROW * row + (GENERAL_ROW - height) / 2;
        // The first row holds two settings side by side.
        let half = WIDTH / 2;
        let toggle_top = row_top(0, 24);
        let toggle = rect(half - 18 - 44, toggle_top, half - 18, toggle_top + 24);
        let output_top = row_top(0, 30);
        let output = rect(right - 76, output_top, right, output_top + 30);
        let stepper_top = row_top(1, 30);
        // The second row is split too: the pin opacity step on the left,
        // the automatic update check on the right.
        let stepper_right = half - 18;
        let stepper = rect(stepper_right - 112, stepper_top, stepper_right, stepper_top + 30);
        let minus = rect(stepper_right - 112, stepper_top, stepper_right - 80, stepper_top + 30);
        let plus = rect(stepper_right - 32, stepper_top, stepper_right, stepper_top + 30);
        let auto_update_top = row_top(1, 24);
        let auto_update = rect(right - 44, auto_update_top, right, auto_update_top + 24);
        let chip_top = row_top(2, 32);
        let mut themes = [RECT::default(); THEMES.len()];
        let count = THEMES.len() as i32;
        for (index, chip) in themes.iter_mut().enumerate() {
            let left = right - count * CHIP - (count - 1) * CHIP_GAP + index as i32 * (CHIP + CHIP_GAP);
            *chip = rect(left, chip_top, left + CHIP, chip_top + 32);
        }

        // The recording tab, laid out in the same content area.
        let tab_left = 200;
        let tabs = [
            rect(tab_left, 12, tab_left + 72, 42),
            rect(tab_left + 80, 12, tab_left + 80 + 136, 42),
        ];
        let mut ty = HEADER;
        let record_label = rect(PAD, ty, WIDTH - PAD, ty + LABEL);
        ty += LABEL;
        let record_rows = 2;
        let record_keys = rect(PAD, ty, WIDTH - PAD, ty + ROW * record_rows + CARD_INSET * 2);
        for index in RECORD_START..BINDINGS.len() {
            let slot = (index - RECORD_START) as i32;
            let left = PAD + CARD_INSET + (slot / record_rows) * column;
            let top = ty + CARD_INSET + (slot % record_rows) * ROW;
            rows[index] = rect(left, top, left + column, top + ROW);
            let right = left + column - 8;
            keys[index] = rect(right - ROW_KEYS, top + 6, right, top + ROW - 6);
        }
        ty += ROW * record_rows + CARD_INSET * 2 + SECTION_GAP;
        let record_options = rect(PAD, ty, WIDTH - PAD, ty + GENERAL_ROW * 4);
        let option_top = |row: i32, height: i32| ty + GENERAL_ROW * row + (GENERAL_ROW - height) / 2;
        let half_right = WIDTH / 2 - 18;
        let chip = |right_edge: i32, count: i32, index: i32, width: i32| {
            let left = right_edge - count * width - (count - 1) * 4 + index * (width + 4);
            rect(left, option_top(0, 30), left + width, option_top(0, 30) + 30)
        };
        let fps = [chip(half_right, 2, 0, 56), chip(half_right, 2, 1, 56)];
        let quality = [chip(right, 3, 0, 64), chip(right, 3, 1, 64), chip(right, 3, 2, 64)];
        let system_audio = rect(half_right - 44, option_top(1, 24), half_right, option_top(1, 24) + 24);
        let microphone = rect(right - 44, option_top(1, 24), right, option_top(1, 24) + 24);
        let cursor = rect(half_right - 44, option_top(2, 24), half_right, option_top(2, 24) + 24);
        let countdown = rect(half_right - 44, option_top(3, 24), half_right, option_top(3, 24) + 24);
        let folder_button = rect(right - 68, option_top(2, 30), right, option_top(2, 30) + 30);
        let folder = rect(WIDTH / 2 + 18 + 70, option_top(2, 30), right - 76, option_top(2, 30) + 30);
        ty += GENERAL_ROW * 4 + SECTION_GAP;
        let panels_label = rect(PAD, ty, WIDTH - PAD, ty + LABEL);
        ty += LABEL;
        let panels = rect(PAD, ty, WIDTH - PAD, ty + 150);
        let half_width = (WIDTH - PAD * 2) / 2;
        let mut panel_visible = [RECT::default(); 2];
        let mut anchors = [[RECT::default(); 9]; 2];
        let mut panel_size = [[RECT::default(); 2]; 2];
        let mut panel_opacity = [[RECT::default(); 2]; 2];
        for panel in 0..2 {
            let left = PAD + half_width * panel as i32;
            let right = left + half_width - 18;
            panel_visible[panel] = rect(right - 44, ty + 12, right, ty + 36);
            let stepper = |top: i32| [rect(right - 112, top, right - 80, top + 30), rect(right - 32, top, right, top + 30)];
            let grid_left = left + 18 + 48;
            let grid_top = ty + 52;
            for (index, cell) in anchors[panel].iter_mut().enumerate() {
                let (column, row) = ((index % 3) as i32, (index / 3) as i32);
                let x = grid_left + column * (ANCHOR_CELL.0 + 4);
                let y = grid_top + row * (ANCHOR_CELL.1 + 4);
                *cell = rect(x, y, x + ANCHOR_CELL.0, y + ANCHOR_CELL.1);
            }
            panel_size[panel] = stepper(grid_top);
            panel_opacity[panel] = stepper(grid_top + 42);
        }

        let button_top = y + (FOOTER - 34) / 2;
        let mascot = rect(WIDTH - 4, HEADER + 4, TOTAL - PAD / 2, y);
        let save = rect(TOTAL - PAD - 84, button_top, TOTAL - PAD, button_top + 34);
        let cancel = rect(
            TOTAL - PAD - 84 * 2 - 10,
            button_top,
            TOTAL - PAD - 84 - 10,
            button_top + 34,
        );
        let reset = rect(PAD - 8, button_top, PAD - 8 + 92, button_top + 34);
        Self {
            scale,
            tab,
            tabs,
            record_label,
            record_keys,
            record_options,
            fps,
            quality,
            cursor,
            countdown,
            folder,
            folder_button,
            system_audio,
            microphone,
            panels_label,
            panels,
            panel_visible,
            anchors,
            panel_size,
            panel_opacity,
            width: s(TOTAL),
            mascot,
            height,
            header,
            close,
            capture_label,
            hero,
            toolbar_label,
            toolbar,
            general_label,
            general,
            footer,
            keys,
            rows,
            clear,
            service,
            search_service,
            ocr_service,
            toggle,
            auto_update,
            output,
            stepper,
            minus,
            plus,
            themes,
            reset,
            cancel,
            save,
        }
    }

    fn s(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    fn targets(&self) -> Box<dyn Iterator<Item = (Target, RECT)> + '_> {
        let common = (0..2)
            .map(|index| (Target::Tab(index), self.tabs[index]))
            .chain([
                (Target::Close, self.close),
                (Target::Reset, self.reset),
                (Target::Cancel, self.cancel),
                (Target::Save, self.save),
            ]);
        if self.tab == 0 {
            Box::new(
                (0..RECORD_START)
                    .chain([SECONDARY])
                    .map(|index| (Target::Key(index), self.keys[index]))
                    .chain([
                        (Target::ClearSecondary, self.clear),
                        (Target::Service, self.service),
                        (Target::SearchService, self.search_service),
                        (Target::OcrService, self.ocr_service),
                        (Target::Output, self.output),
                    ])
                    .chain((0..THEMES.len()).map(|index| (Target::Theme(index), self.themes[index])))
                    .chain([
                        (Target::Toggle, self.toggle),
                        (Target::AutoUpdate, self.auto_update),
                        (Target::Minus, self.minus),
                        (Target::Plus, self.plus),
                    ])
                    .chain(common),
            )
        } else {
            Box::new(
                (RECORD_START..BINDINGS.len())
                    .map(|index| (Target::Key(index), self.keys[index]))
                    .chain((0..2).map(|index| (Target::Fps(index), self.fps[index])))
                    .chain((0..3).map(|index| (Target::Quality(index), self.quality[index])))
                    .chain([
                        (Target::Folder, self.folder_button),
                        (Target::Cursor, self.cursor),
                        (Target::Countdown, self.countdown),
                        (Target::SystemAudio, self.system_audio),
                        (Target::Microphone, self.microphone),
                    ])
                    .chain((0..2).flat_map(move |panel| {
                        std::iter::once((Target::PanelVisible(panel), self.panel_visible[panel]))
                            .chain((0..9).map(move |anchor| (Target::PanelAnchor(panel, anchor), self.anchors[panel][anchor])))
                            .chain([
                                (Target::PanelSize(panel, -1), self.panel_size[panel][0]),
                                (Target::PanelSize(panel, 1), self.panel_size[panel][1]),
                                (Target::PanelOpacity(panel, -1), self.panel_opacity[panel][0]),
                                (Target::PanelOpacity(panel, 1), self.panel_opacity[panel][1]),
                            ])
                    }))
                    .chain(common),
            )
        }
    }

    fn hit(&self, point: POINT) -> Option<Target> {
        self.targets()
            .find(|(_, rect)| contains(*rect, point))
            .map(|(target, _)| target)
    }
}

pub unsafe fn register(instance: HINSTANCE) -> io::Result<()> {
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
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

/// A shortcut field is waiting for keys; a global shortcut must not start a capture then.
/// Repaints the settings window, if it is open, after a setting changed elsewhere.
pub unsafe fn refresh() {
    let window = WINDOW.get();
    if !window.is_null() {
        InvalidateRect(window, null(), 0);
    }
}

pub fn is_recording() -> bool {
    STATE.with_borrow(|state| state.as_ref().is_some_and(|state| state.recording.is_some()))
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
    // A frameless window: the header doubles as the title bar.
    let window = CreateWindowExW(
        0,
        CLASS,
        w!("just-snapshot 设置"),
        WS_POPUP | WS_SYSMENU,
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
        auto_update: crate::auto_update_enabled(),
        theme: theme::current(),
        secondary: global_hotkey::shortcuts().get(1).copied(),
        record: crate::recording_ui::load_options(),
    };
    install(window, owner, draft);
    WINDOW.set(window);
    place(window, None);
    ui::round_corners(window);
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    SetFocus(window);
    Ok(())
}

unsafe fn install(window: HWND, owner: HWND, draft: Draft) {
    let scale = match GetDpiForWindow(window) {
        0 => 1.0,
        dpi => dpi as f32 / 96.0,
    };
    STATE.set(Some(State {
        owner,
        draft,
        capture_status: [CaptureStatus::Active, CaptureStatus::Unset],
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
        mascot: crate::app_icon::decode(MASCOT_ART),
        tab: 0,
        record_folder: crate::recording_ui::load_folder(),
    }));
    refresh_capture_status(window);
}

/// Sizes the window to the layout; centers it unless a DPI change suggested a spot.
unsafe fn place(window: HWND, suggested: Option<RECT>) {
    let Some(scale) = STATE.with_borrow(|state| state.as_ref().map(|state| state.scale)) else {
        return;
    };
    let layout = Layout::new(scale);
    let (width, height) = (layout.width, layout.height);
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

/// Whether each edited capture shortcut works: Snapshot already holds it as
/// a capture key, or it is free to register right now.
unsafe fn refresh_capture_status(window: HWND) {
    let draft = STATE.with_borrow(|state| state.as_ref().map(|state| state.draft));
    let Some(draft) = draft else { return };
    let status = [0, SECONDARY].map(|index| {
        let Some(shortcut) = draft.slot(index) else {
            return CaptureStatus::Unset;
        };
        if let Some(other) = draft.duplicate_of(index) {
            CaptureStatus::Duplicate(other)
        } else if global_hotkey::holds(shortcut) {
            CaptureStatus::Active
        } else if RegisterHotKey(
            window,
            PROBE_HOTKEY,
            shortcut.modifiers | MOD_NOREPEAT,
            shortcut.key,
        ) != 0
        {
            UnregisterHotKey(window, PROBE_HOTKEY);
            CaptureStatus::Available
        } else {
            CaptureStatus::Occupied
        }
    });
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
        }
        Target::AutoUpdate => {
            with_state(|state| state.draft.auto_update = !state.draft.auto_update);
            animate(window);
        }
        Target::Minus | Target::Plus => {
            let magnitude = if hotkey::modifiers() & MOD_SHIFT != 0 { 10 } else { 1 };
            let delta = if target == Target::Minus { -magnitude } else { magnitude };
            with_state(|state| state.draft.step_opacity(delta));
            redraw(window);
        }
        Target::ClearSecondary => {
            with_state(|state| {
                state.draft.secondary = None;
                if state.recording == Some(SECONDARY) {
                    state.recording = None;
                }
            });
            refresh_capture_status(window);
            redraw(window);
        }
        Target::Theme(index) => {
            // Previewed live in this window; applied everywhere on save.
            with_state(|state| state.draft.theme = THEMES[index]);
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
        Target::Service => {
            if let Err(error) = translate_settings::open(window) {
                crate::show_error(window, &format!("无法打开翻译设置。\n{error}"));
            }
        }
        Target::OcrService => {
            if let Err(error) = crate::ocr_settings::open(window) {
                crate::show_error(window, &format!("无法打开文字识别设置。\n{error}"));
            }
        }
        Target::SearchService => {
            if let Err(error) = crate::image_search_settings::open(window) {
                crate::show_error(window, &format!("无法打开识图设置。\n{error}"));
            }
        }
        Target::Tab(tab) => {
            stop_recording(window);
            with_state(|state| state.tab = tab);
            redraw(window);
        }
        Target::Fps(index) => {
            with_state(|state| state.draft.record.fps = [30, 60][index]);
            redraw(window);
        }
        Target::Quality(index) => {
            with_state(|state| state.draft.record.quality = crate::recorder::Quality::ALL[index]);
            redraw(window);
        }
        Target::Cursor | Target::Countdown | Target::SystemAudio | Target::Microphone => {
            with_state(|state| {
                let record = &mut state.draft.record;
                match target {
                    Target::Cursor => record.cursor = !record.cursor,
                    Target::Countdown => record.countdown = !record.countdown,
                    Target::SystemAudio => record.system_audio = !record.system_audio,
                    _ => record.microphone = !record.microphone,
                }
            });
            redraw(window);
        }
        Target::Folder => {
            let current = crate::recording_ui::folder();
            if let Some(chosen) = crate::output_settings::pick_folder(window, &current.to_string_lossy()) {
                with_state(|state| state.record_folder = chosen);
                redraw(window);
            }
        }
        Target::PanelVisible(panel) => {
            crate::input_overlay::toggle(crate::input_overlay::PANELS[panel]);
            redraw(window);
        }
        Target::PanelAnchor(panel, anchor) => {
            let which = crate::input_overlay::PANELS[panel];
            let mut config = crate::input_overlay::config().panels[panel].at_anchor(crate::input_overlay::ANCHORS[anchor]);
            config.visible = true;
            crate::input_overlay::set(which, config);
            redraw(window);
        }
        Target::PanelSize(panel, step) | Target::PanelOpacity(panel, step) => {
            let which = crate::input_overlay::PANELS[panel];
            let mut config = crate::input_overlay::config().panels[panel];
            if matches!(target, Target::PanelSize(..)) {
                config.scale += step * 10;
            } else {
                config.opacity += step * 5;
            }
            crate::input_overlay::set(which, config);
            redraw(window);
        }
        Target::Output => {
            if let Err(error) = crate::output_settings::open(window) {
                crate::show_error(window, &format!("无法打开保存设置。\n{error}"));
            }
        }
        Target::Close | Target::Cancel => {
            DestroyWindow(window);
        }
        Target::Save => save(window),
    }
}

unsafe fn save(window: HWND) {
    let Some((owner, draft, folder)) = STATE.with_borrow(|state| {
        state
            .as_ref()
            .filter(|state| state.draft.first_duplicate().is_none())
            .map(|state| (state.owner, state.draft, state.record_folder.clone()))
    }) else {
        return;
    };
    match draft
        .preferences()
        .and_then(|preferences| apply_preferences(owner, preferences, draft.start_at_login))
        .and_then(|()| crate::recording_ui::save_options(draft.record))
        .and_then(|()| crate::recording_ui::save_folder(&folder))
    {
        Ok(()) => {
            DestroyWindow(window);
        }
        Err(error) => {
            // The footer has room for one line; the full error goes to the dialog.
            with_state(|state| state.notice = Notice::Error("保存失败，详细原因见弹出的提示".into()));
            redraw(window);
            crate::show_error(window, &error);
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
    } else if let Some(shortcut) = Shortcut::from_key(modifiers, key).or_else(|| {
        // A lone letter or digit, for the bindings that allow one (the pointer key).
        let letter = char::from_u32(key).filter(char::is_ascii_alphanumeric)?;
        let binding = *BINDINGS.get(index)?;
        (modifiers == 0).then(|| Shortcut::single(binding, &letter.to_string())).flatten()
    }) {
        with_state(|state| {
            state.draft.set_slot(index, shortcut);
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

fn layout() -> Option<Layout> {
    STATE.with_borrow(|state| state.as_ref().map(|state| Layout::for_tab(state.scale, state.tab)))
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
            // The header drags the window, except where its buttons are.
            let mut client = mouse_point(lparam);
            ScreenToClient(window, &mut client);
            if let Some(layout) = layout() {
                if contains(layout.header, client) && layout.hit(client).is_none() {
                    return HTCAPTION as LRESULT;
                }
            }
            return HTCLIENT as LRESULT;
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
            let hit = layout().and_then(|layout| layout.hit(mouse_point(lparam)));
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
            SetCursor(LoadCursorW(
                null_mut(),
                if clickable { IDC_HAND } else { IDC_ARROW },
            ));
            return 1;
        }
        WM_LBUTTONDOWN => {
            SetFocus(window);
            let hit = layout().and_then(|layout| layout.hit(mouse_point(lparam)));
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
            let hit = layout().and_then(|layout| layout.hit(mouse_point(lparam)));
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
        WM_SYSCHAR
            if STATE.with_borrow(|state| {
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
            Painter::new(surface.dc, state).paint(client);
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
    /// The draft theme, so picking one previews it immediately.
    palette: &'static Palette,
}

impl<'a> Painter<'a> {
    fn new(dc: HDC, state: &'a State) -> Self {
        Self {
            dc,
            state,
            layout: Layout::for_tab(state.scale, state.tab),
            palette: state.draft.theme.palette(),
        }
    }

    fn s(&self, value: i32) -> i32 {
        self.layout.s(value)
    }

    fn f(&self, value: f32) -> f32 {
        value * self.layout.scale
    }

    fn hover(&self, target: Target) -> f32 {
        self.state.hover.get(&target).copied().unwrap_or(0.0)
    }

    fn face(&self, target: Target, base: u32) -> u32 {
        if self.state.pressed == Some(target) {
            self.palette.pressed
        } else {
            mix(base, self.palette.hover, self.hover(target))
        }
    }

    unsafe fn text(&self, rect: RECT, value: &str, font: HFONT, color: u32, format: u32) {
        ui::text(self.dc, rect, value, font, color, format | DT_SINGLELINE);
    }

    unsafe fn paint(&self, client: RECT) {
        let palette = self.palette;
        let fonts = &self.state.fonts;
        ui::fill(self.dc, client, palette.background);
        if let Some(graphics) = Graphics::new(self.dc) {
            graphics.stroke_round(
                ui::inset(ui::to_f(client), 0.5),
                0.0,
                argb(palette.card_border, 255),
                1.0,
            );
        }
        let layout = &self.layout;
        let header = RECT {
            left: self.s(PAD),
            right: layout.close.left,
            ..layout.header
        };
        self.text(header, "设置", fonts.title, palette.text, DT_LEFT | DT_VCENTER);
        let title_width = ui::measure(self.dc, "设置", fonts.title);
        let version = RECT {
            left: header.left + title_width + self.s(10),
            top: header.top + self.s(4),
            ..header
        };
        self.text(
            version,
            &format!("just-snapshot v{}", env!("CARGO_PKG_VERSION")),
            fonts.small,
            palette.tertiary,
            DT_LEFT | DT_VCENTER,
        );
        self.close_button();
        self.tabs();
        self.mascot();
        if self.state.tab == 1 {
            self.record_tab();
            self.footer();
            return;
        }
        for (rect, label) in [
            (layout.capture_label, "截图"),
            (layout.toolbar_label, "截图工具栏　·　「截取窗口」和「上次截图」在任何程序里都能用"),
            (layout.general_label, "常规"),
        ] {
            let rect = RECT {
                left: rect.left + self.s(4),
                bottom: rect.bottom - self.s(6),
                ..rect
            };
            self.text(rect, label, fonts.small, palette.secondary, DT_LEFT | DT_BOTTOM);
        }
        for card in [layout.hero, layout.toolbar, layout.general] {
            self.card(card);
        }
        self.hero();
        self.toolbar_rows();
        self.general();
        self.footer();
    }

    /// Section titles in the header: 截图 and 录像与按键显示.
    unsafe fn tabs(&self) {
        let palette = self.palette;
        for (index, rect) in self.layout.tabs.iter().enumerate() {
            let selected = self.state.tab == index;
            let target = Target::Tab(index);
            if let Some(graphics) = Graphics::new(self.dc) {
                let face = if selected { palette.accent_soft } else { self.face(target, palette.background) };
                graphics.fill_round(ui::to_f(*rect), self.f(8.0), argb(face, 255));
            }
            let color = if selected { palette.accent } else { palette.secondary };
            let font = if selected { self.state.fonts.heading } else { self.state.fonts.body };
            self.text(*rect, TABS[index], font, color, DT_CENTER | DT_VCENTER);
        }
    }

    unsafe fn section_label(&self, rect: RECT, label: &str) {
        let rect = RECT { left: rect.left + self.s(4), bottom: rect.bottom - self.s(6), ..rect };
        self.text(rect, label, self.state.fonts.small, self.palette.secondary, DT_LEFT | DT_BOTTOM);
    }

    /// One shortcut row: icon, name, keycaps; a divider above all but the first in a column.
    unsafe fn binding_row(&self, index: usize, divider: bool) {
        let palette = self.palette;
        let row = self.layout.rows[index];
        if divider {
            ui::fill(
                self.dc,
                RECT { left: row.left + self.s(12), top: row.top, right: row.right - self.s(12), bottom: row.top + 1 },
                palette.divider,
            );
        }
        let size = self.s(20);
        let icon = RECT {
            left: row.left + self.s(12),
            top: (row.top + row.bottom - size) / 2,
            right: row.left + self.s(12) + size,
            bottom: (row.top + row.bottom - size) / 2 + size,
        };
        icons::paint(self.dc, icon, binding_icon(BINDINGS[index]), palette.icon);
        let label = RECT { left: icon.right + self.s(10), right: self.layout.keys[index].left, ..row };
        self.text(label, BINDINGS[index].label(), self.state.fonts.body, palette.text, DT_LEFT | DT_VCENTER);
        self.key_field(index, self.s(24));
    }

    /// A settings row's title and description, between `left` and `right`.
    unsafe fn row_text(&self, left: i32, right: i32, middle: i32, title: &str, description: &str) {
        self.text(
            RECT { left, top: middle - self.s(21), right, bottom: middle },
            title,
            self.state.fonts.body,
            self.palette.text,
            DT_LEFT | DT_BOTTOM | DT_END_ELLIPSIS,
        );
        if !description.is_empty() {
            self.text(
                RECT { left, top: middle + self.s(2), right, bottom: middle + self.s(21) },
                description,
                self.state.fonts.small,
                self.palette.secondary,
                DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
            );
        }
    }

    unsafe fn switch(&self, rect: RECT, on: bool, target: Target) {
        let palette = self.palette;
        let Some(graphics) = Graphics::new(self.dc) else { return };
        let hover = self.hover(target);
        let track = if on { mix(palette.accent, palette.accent_hover, hover) } else { mix(palette.track_off, palette.secondary, hover * 0.3) };
        let bounds = ui::to_f(rect);
        let height = bounds[3] - bounds[1];
        graphics.fill_round(bounds, height / 2.0, argb(track, 255));
        let margin = self.f(3.0);
        let knob = height - margin * 2.0;
        let left = if on { bounds[2] - margin - knob } else { bounds[0] + margin };
        graphics.fill_ellipse([left, bounds[1] + margin, left + knob, bounds[1] + margin + knob], argb(0x00ff_ffff, 255));
    }

    unsafe fn chip(&self, rect: RECT, label: &str, selected: bool, target: Target) {
        let palette = self.palette;
        if let Some(graphics) = Graphics::new(self.dc) {
            let bounds = ui::to_f(rect);
            let face = if selected { palette.accent_soft } else { self.face(target, palette.key_face) };
            graphics.fill_round(bounds, self.f(8.0), argb(face, 255));
            let (border, width) = if selected { (palette.accent, 1.5) } else { (palette.key_border, 1.0) };
            graphics.stroke_round(ui::inset(bounds, width / 2.0), self.f(8.0), argb(border, 255), width);
        }
        let color = if selected { palette.accent } else { palette.text };
        self.text(rect, label, self.state.fonts.small, color, DT_CENTER | DT_VCENTER);
    }

    /// − value + with the value between the two buttons.
    unsafe fn small_stepper(&self, minus: RECT, plus: RECT, value: &str, minus_target: Target, plus_target: Target) {
        let palette = self.palette;
        let whole = RECT { left: minus.left, right: plus.right, ..minus };
        if let Some(graphics) = Graphics::new(self.dc) {
            graphics.fill_round(ui::to_f(whole), self.f(8.0), argb(palette.key_face, 255));
            for (target, rect) in [(minus_target, minus), (plus_target, plus)] {
                let face = self.face(target, palette.key_face);
                graphics.fill_round(ui::to_f(ui::inflate(rect, -self.s(2))), self.f(6.0), argb(face, 255));
            }
            graphics.stroke_round(ui::inset(ui::to_f(whole), 0.5), self.f(8.0), argb(palette.key_border, 255), 1.0);
        }
        self.text(minus, "−", self.state.fonts.body, palette.icon, DT_CENTER | DT_VCENTER);
        self.text(plus, "+", self.state.fonts.body, palette.icon, DT_CENTER | DT_VCENTER);
        self.text(RECT { left: minus.right, right: plus.left, ..minus }, value, self.state.fonts.key, palette.text, DT_CENTER | DT_VCENTER);
    }

    /// The 录像与按键显示 tab.
    unsafe fn record_tab(&self) {
        let palette = self.palette;
        let layout = &self.layout;
        let draft = self.state.draft.record;
        self.section_label(layout.record_label, "录像快捷键　·　在任何程序里都能用");
        self.section_label(layout.panels_label, "按键显示　·　改动立即生效；拖面板下方的 ≡ 可以随意摆");
        for card in [layout.record_keys, layout.record_options, layout.panels] {
            self.card(card);
        }
        // Shortcuts, two to a column.
        for index in RECORD_START..BINDINGS.len() {
            self.binding_row(index, (index - RECORD_START) % 2 == 1);
        }
        // Options: three rows, two settings each.
        let card = layout.record_options;
        let row = self.s(GENERAL_ROW);
        let half = card.left + (card.right - card.left) / 2;
        for line in 0..4 {
            let top = card.top + row * line;
            if line > 0 {
                ui::fill(self.dc, RECT { left: card.left + self.s(18), top, right: card.right - self.s(18), bottom: top + 1 }, palette.divider);
            }
            ui::fill(self.dc, RECT { left: half, top: top + self.s(10), right: half + 1, bottom: top + row - self.s(10) }, palette.divider);
        }
        let middle = |line: i32| card.top + row * line + row / 2;
        let left_text = card.left + self.s(18);
        let right_text = half + self.s(18);
        self.row_text(left_text, layout.fps[0].left - self.s(12), middle(0), "帧率", "60 帧更流畅，30 帧文件更小");
        for (index, fps) in [30, 60].into_iter().enumerate() {
            self.chip(layout.fps[index], &format!("{fps} 帧"), draft.fps == fps, Target::Fps(index));
        }
        self.row_text(right_text, layout.quality[0].left - self.s(12), middle(0), "画质", "原画按屏幕实际像素录");
        for (index, quality) in crate::recorder::Quality::ALL.into_iter().enumerate() {
            self.chip(layout.quality[index], quality.name(), draft.quality == quality, Target::Quality(index));
        }
        self.row_text(left_text, layout.system_audio.left - self.s(12), middle(1), "录制系统声音", "电脑里播放的声音");
        self.switch(layout.system_audio, draft.system_audio, Target::SystemAudio);
        self.row_text(right_text, layout.microphone.left - self.s(12), middle(1), "录制麦克风", "和系统声音混在一起");
        self.switch(layout.microphone, draft.microphone, Target::Microphone);
        self.row_text(left_text, layout.cursor.left - self.s(12), middle(2), "录制鼠标指针", "");
        self.switch(layout.cursor, draft.cursor, Target::Cursor);
        self.row_text(left_text, layout.countdown.left - self.s(12), middle(3), "开始前倒数 3 秒", "框选录像时，给你时间切回要录的窗口");
        self.switch(layout.countdown, draft.countdown, Target::Countdown);
        self.row_text(right_text, layout.folder.left - self.s(8), middle(2), "保存位置", "");
        let folder = if self.state.record_folder.trim().is_empty() {
            crate::recording_ui::folder().to_string_lossy().into_owned()
        } else {
            self.state.record_folder.clone()
        };
        self.text(layout.folder, &folder, self.state.fonts.small, palette.secondary, DT_LEFT | DT_VCENTER | DT_PATH_ELLIPSIS);
        ui::button(
            self.dc,
            layout.folder_button,
            "更改…",
            self.state.fonts.body,
            palette,
            false,
            self.face(Target::Folder, palette.key_face),
            self.f(8.0),
        );
        // Input panels: one column each.
        let config = crate::input_overlay::config();
        let panels = layout.panels;
        let half_width = (panels.right - panels.left) / 2;
        ui::fill(
            self.dc,
            RECT { left: panels.left + half_width, top: panels.top + self.s(12), right: panels.left + half_width + 1, bottom: panels.bottom - self.s(12) },
            palette.divider,
        );
        for (panel, (name, description, icon)) in [("键鼠", "按下的键和鼠标按键会亮起", Icon::Keyboard), ("手柄", "Xbox 手柄的按键、扳机和摇杆", Icon::Gamepad)]
            .into_iter()
            .enumerate()
        {
            let settings = config.panels[panel];
            let left = panels.left + half_width * panel as i32 + self.s(18);
            let toggle = layout.panel_visible[panel];
            let size = self.s(20);
            let icon_rect = RECT { left, top: (toggle.top + toggle.bottom - size) / 2, right: left + size, bottom: (toggle.top + toggle.bottom - size) / 2 + size };
            icons::paint(self.dc, icon_rect, icon, palette.icon);
            self.row_text(icon_rect.right + self.s(10), toggle.left - self.s(12), (toggle.top + toggle.bottom) / 2, name, description);
            self.switch(toggle, settings.visible, Target::PanelVisible(panel));
            // Nine places; none is lit once the panel was dragged somewhere else.
            let grid = layout.anchors[panel];
            self.text(
                RECT { left, top: grid[0].top, right: grid[0].left - self.s(6), bottom: grid[0].bottom },
                "位置",
                self.state.fonts.small,
                palette.secondary,
                DT_LEFT | DT_VCENTER,
            );
            let anchored = settings.anchored();
            for (index, cell) in grid.iter().enumerate() {
                let selected = anchored == Some(crate::input_overlay::ANCHORS[index]);
                let target = Target::PanelAnchor(panel, index);
                if let Some(graphics) = Graphics::new(self.dc) {
                    let face = if selected { palette.accent } else { self.face(target, palette.key_face) };
                    graphics.fill_round(ui::to_f(*cell), self.f(5.0), argb(face, 255));
                    if !selected {
                        graphics.stroke_round(ui::inset(ui::to_f(*cell), 0.5), self.f(5.0), argb(palette.key_border, 255), 1.0);
                    }
                }
            }
            if anchored.is_none() {
                self.text(
                    RECT { left, top: grid[6].bottom + self.s(4), right: grid[2].right + self.s(40), bottom: grid[6].bottom + self.s(20) },
                    "已拖到自定义位置",
                    self.state.fonts.small,
                    palette.secondary,
                    DT_LEFT | DT_VCENTER,
                );
            }
            let [minus, plus] = layout.panel_size[panel];
            self.text(RECT { left: grid[2].right + self.s(16), right: minus.left - self.s(8), ..minus }, "大小", self.state.fonts.small, palette.secondary, DT_RIGHT | DT_VCENTER);
            self.small_stepper(minus, plus, &format!("{}%", settings.scale), Target::PanelSize(panel, -1), Target::PanelSize(panel, 1));
            let [minus, plus] = layout.panel_opacity[panel];
            self.text(RECT { left: grid[2].right + self.s(16), right: minus.left - self.s(8), ..minus }, "不透明度", self.state.fonts.small, palette.secondary, DT_RIGHT | DT_VCENTER);
            self.small_stepper(minus, plus, &format!("{}%", settings.opacity), Target::PanelOpacity(panel, -1), Target::PanelOpacity(panel, 1));
        }
    }

    /// The mascot, as tall as her column allows, standing on the footer line.
    unsafe fn mascot(&self) {
        let Some(art) = &self.state.mascot else { return };
        let column = self.layout.mascot;
        let Some(graphics) = Graphics::new(self.dc) else { return };
        // A soft shadow on the floor under her feet.
        let (cx, floor) = ((column.left + column.right) as f32 / 2.0, column.bottom as f32 - self.f(3.0));
        for step in 0..4 {
            let (rx, ry) = (self.f(58.0 - step as f32 * 10.0), self.f(7.0 - step as f32 * 1.2));
            graphics.fill_ellipse([cx - rx, floor - ry, cx + rx, floor + ry], argb(0, 18));
        }
        let height = (column.bottom - column.top) as f32;
        let width = height * art.width as f32 / art.height as f32;
        let (width, height) = if width > (column.right - column.left) as f32 {
            let width = (column.right - column.left) as f32;
            (width, width * art.height as f32 / art.width as f32)
        } else {
            (width, height)
        };
        let left = (column.left + column.right) as f32 / 2.0 - width / 2.0;
        let top = column.bottom as f32 - height;
        let point = |x: f32, y: f32| windows_sys::Win32::Graphics::GdiPlus::PointF { X: x, Y: y };
        graphics.image(art, [point(left, top), point(left + width, top), point(left, top + height)], true);
    }

    unsafe fn close_button(&self) {
        let rect = self.layout.close;
        let hover = self.hover(Target::Close);
        if hover > 0.0 || self.state.pressed == Some(Target::Close) {
            if let Some(graphics) = Graphics::new(self.dc) {
                let face = self.face(Target::Close, self.palette.background);
                graphics.fill_round(ui::to_f(rect), self.f(8.0), argb(face, 255));
            }
        }
        let size = self.s(18);
        let icon = RECT {
            left: (rect.left + rect.right - size) / 2,
            top: (rect.top + rect.bottom - size) / 2,
            right: (rect.left + rect.right - size) / 2 + size,
            bottom: (rect.top + rect.bottom - size) / 2 + size,
        };
        let color = mix(self.palette.secondary, self.palette.text, hover);
        icons::paint(self.dc, icon, Icon::Close, color);
    }

    /// A small gear after a label that opens a service window: translation
    /// after "翻译", the image search engines after "识图".
    unsafe fn service_link(&self, target: Target, rect: RECT) {
        let hover = self.hover(target);
        if hover > 0.0 || self.state.pressed == Some(target) {
            if let Some(graphics) = Graphics::new(self.dc) {
                let face = self.face(target, self.palette.card);
                graphics.fill_round(ui::to_f(rect), self.f(6.0), argb(face, 255));
            }
        }
        let color = mix(self.palette.secondary, self.palette.accent, hover);
        icons::paint(self.dc, ui::inflate(rect, -self.s(4)), Icon::Settings, color);
    }

    unsafe fn card(&self, rect: RECT) {
        let Some(graphics) = Graphics::new(self.dc) else {
            ui::fill(self.dc, rect, self.palette.card);
            return;
        };
        let bounds = ui::to_f(rect);
        let radius = self.f(10.0);
        graphics.fill_round(bounds, radius, argb(self.palette.card, 255));
        graphics.stroke_round(
            ui::inset(bounds, 0.5),
            radius,
            argb(self.palette.card_border, 255),
            1.0,
        );
    }

    unsafe fn hero(&self) {
        let palette = self.palette;
        let fonts = &self.state.fonts;
        let card = self.layout.hero;
        let main = self.layout.keys[0];
        let second = self.layout.keys[SECONDARY];
        // Row 1: icon, title and the main shortcut with its status below it.
        let tile_size = self.s(40);
        let row_middle = main.top + self.s(29);
        let tile = RECT {
            left: card.left + self.s(18),
            top: row_middle - tile_size / 2,
            right: card.left + self.s(18) + tile_size,
            bottom: row_middle + tile_size / 2,
        };
        if let Some(graphics) = Graphics::new(self.dc) {
            graphics.fill_round(ui::to_f(tile), self.f(10.0), argb(palette.accent_soft, 255));
        }
        icons::paint(self.dc, ui::inflate(tile, -self.s(8)), Icon::Rectangle, palette.accent);
        let left = tile.right + self.s(14);
        let title = RECT {
            left,
            top: row_middle - self.s(22),
            right: main.left,
            bottom: row_middle,
        };
        self.text(title, "开始截图", fonts.heading, palette.text, DT_LEFT | DT_BOTTOM);
        self.text(
            RECT {
                top: row_middle + self.s(3),
                bottom: row_middle + self.s(23),
                ..title
            },
            "全局快捷键，在任何程序里按下都能截图",
            fonts.small,
            palette.secondary,
            DT_LEFT | DT_TOP,
        );
        self.key_field(0, self.s(28));
        let status = RECT {
            left: main.left - self.s(80),
            top: main.bottom + self.s(4),
            right: main.right - self.s(6),
            bottom: main.bottom + self.s(24),
        };
        self.status_line(status, 0, DT_RIGHT);

        // Row 2: the optional second shortcut; its description is its status.
        let divider = card.top + self.s(70);
        ui::fill(
            self.dc,
            RECT {
                left: card.left + self.s(18),
                top: divider,
                right: card.right - self.s(18),
                bottom: divider + 1,
            },
            palette.divider,
        );
        let middle = (second.top + second.bottom) / 2;
        let label = RECT {
            left,
            top: middle - self.s(21),
            right: self.layout.clear.left - self.s(8),
            bottom: middle,
        };
        self.text(label, "副键位", fonts.body, palette.text, DT_LEFT | DT_BOTTOM);
        let status = RECT {
            top: middle + self.s(2),
            bottom: middle + self.s(21),
            ..label
        };
        self.status_line(status, SECONDARY, DT_LEFT);
        if self.state.draft.secondary.is_some() {
            self.clear_button();
        }
        self.key_field(SECONDARY, self.s(28));
    }

    /// The live status of capture field `index` (0 or SECONDARY), drawn as a
    /// badge plus text aligned to the `align` edge of `rect`.
    unsafe fn status_line(&self, rect: RECT, index: usize, align: u32) {
        let palette = self.palette;
        let fonts = &self.state.fonts;
        let which = usize::from(index == SECONDARY);
        let (color, label, badge) = if self.state.recording == Some(index) {
            (palette.tertiary, "按下新的组合键，Esc 取消".to_string(), None)
        } else {
            match self.state.capture_status[which] {
                CaptureStatus::Unset => (
                    palette.secondary,
                    "可选，和主键位同时生效".to_string(),
                    None,
                ),
                CaptureStatus::Active => (palette.success, "已生效".into(), Some(Badge::Check)),
                CaptureStatus::Available => {
                    (palette.success, "可以使用，保存后生效".into(), Some(Badge::Check))
                }
                CaptureStatus::Occupied => (
                    palette.warning,
                    "已被其他程序占用，换一个组合键试试".into(),
                    Some(Badge::Exclamation),
                ),
                CaptureStatus::Duplicate(other) => (
                    palette.warning,
                    format!("和「{}」重复", field_label(other)),
                    Some(Badge::Exclamation),
                ),
            }
        };
        let size = self.s(14);
        let gap = self.s(5);
        let width = ui::measure(self.dc, &label, fonts.small);
        let badge_width = if badge.is_some() { size + gap } else { 0 };
        let left = if align == DT_RIGHT {
            rect.right - width - badge_width
        } else {
            rect.left
        };
        let center_y = (rect.top + rect.bottom) / 2;
        if let Some(badge) = badge {
            let bounds = RECT {
                left,
                top: center_y - size / 2,
                right: left + size,
                bottom: center_y - size / 2 + size,
            };
            ui::badge(self.dc, bounds, color, badge, palette.card);
        }
        let text = RECT {
            left: left + badge_width,
            right: rect.right.max(left + badge_width + width),
            ..rect
        };
        self.text(text, &label, fonts.small, color, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS);
    }

    unsafe fn clear_button(&self) {
        let rect = self.layout.clear;
        let hover = self.hover(Target::ClearSecondary);
        if hover > 0.0 || self.state.pressed == Some(Target::ClearSecondary) {
            if let Some(graphics) = Graphics::new(self.dc) {
                let face = self.face(Target::ClearSecondary, self.palette.card);
                graphics.fill_round(ui::to_f(rect), self.f(6.0), argb(face, 255));
            }
        }
        let icon = ui::inflate(rect, -self.s(6));
        let color = mix(self.palette.tertiary, self.palette.text, hover);
        icons::paint(self.dc, icon, Icon::Close, color);
    }

    unsafe fn toolbar_rows(&self) {
        let palette = self.palette;
        let layout = &self.layout;
        let card = layout.toolbar;
        for column in 1..COLUMNS as usize {
            let x = layout.rows[1 + column * ROWS as usize].left;
            ui::fill(
                self.dc,
                RECT {
                    left: x,
                    top: card.top + self.s(14),
                    right: x + 1,
                    bottom: card.bottom - self.s(14),
                },
                palette.divider,
            );
        }
        for index in 1..RECORD_START {
            let row = layout.rows[index];
            if (index - 1) as i32 % ROWS != 0 {
                ui::fill(
                    self.dc,
                    RECT {
                        left: row.left + self.s(12),
                        top: row.top,
                        right: row.right - self.s(12),
                        bottom: row.top + 1,
                    },
                    palette.divider,
                );
            }
            let size = self.s(20);
            let icon = RECT {
                left: row.left + self.s(12),
                top: (row.top + row.bottom - size) / 2,
                right: row.left + self.s(12) + size,
                bottom: (row.top + row.bottom - size) / 2 + size,
            };
            icons::paint(self.dc, icon, binding_icon(BINDINGS[index]), palette.icon);
            let label = RECT {
                left: icon.right + self.s(10),
                right: layout.keys[index].left,
                ..row
            };
            self.text(
                label,
                BINDINGS[index].label(),
                self.state.fonts.body,
                palette.text,
                DT_LEFT | DT_VCENTER,
            );
            self.key_field(index, self.s(24));
        }
        self.service_link(Target::Service, self.layout.service);
        self.service_link(Target::SearchService, self.layout.search_service);
        self.service_link(Target::OcrService, self.layout.ocr_service);
    }

    /// A shortcut drawn as keycaps, right-aligned in its field.
    unsafe fn key_field(&self, index: usize, cap_height: i32) {
        let palette = self.palette;
        let field = self.layout.keys[index];
        let target = Target::Key(index);
        let recording = self.state.recording == Some(index);
        let duplicate = self.state.draft.duplicate_of(index).is_some();
        let Some(graphics) = Graphics::new(self.dc) else {
            return;
        };
        let bounds = ui::to_f(field);
        let radius = self.f(8.0);
        if recording {
            // Breathing outline while waiting for keys.
            let wave = 0.5 - 0.5 * (self.state.pulse * std::f32::consts::TAU).cos();
            graphics.fill_round(bounds, radius, argb(palette.accent_soft, 255));
            let alpha = (110.0 + 145.0 * wave) as u8;
            graphics.stroke_round(ui::inset(bounds, 0.75), radius, argb(palette.accent, alpha), 1.5);
        } else {
            let base = if duplicate { palette.warning_soft } else { palette.card };
            let face = self.face(target, base);
            if face != palette.card {
                graphics.fill_round(bounds, radius, argb(face, 255));
            }
            if duplicate {
                graphics.stroke_round(ui::inset(bounds, 0.75), radius, argb(palette.warning, 200), 1.5);
            }
        }
        drop(graphics);

        let parts: Vec<String> = if recording {
            if self.state.held == 0 {
                let rect = RECT {
                    right: field.right - self.s(12),
                    ..field
                };
                self.text(
                    rect,
                    "按下组合键…",
                    self.state.fonts.small,
                    palette.accent,
                    DT_RIGHT | DT_VCENTER,
                );
                return;
            }
            let mut parts: Vec<String> = modifier_names(self.state.held)
                .into_iter()
                .map(String::from)
                .collect();
            parts.push("…".into());
            parts
        } else if let Some(shortcut) = self.state.draft.slot(index) {
            shortcut.label().split('+').map(String::from).collect()
        } else {
            let rect = RECT {
                right: field.right - self.s(12),
                ..field
            };
            self.text(
                rect,
                "点击设置",
                self.state.fonts.small,
                palette.tertiary,
                DT_RIGHT | DT_VCENTER,
            );
            return;
        };
        let font = self.state.fonts.key;
        let gap = self.s(4);
        let padding = self.s(9);
        let widths: Vec<i32> = parts
            .iter()
            .map(|part| (ui::measure(self.dc, part, font) + padding * 2).max(cap_height))
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
            let bounds = ui::to_f(cap);
            let radius = self.f(5.0);
            let depth = self.f(2.0);
            graphics.fill_round(
                [bounds[0], bounds[1] + depth, bounds[2], bounds[3] + depth],
                radius,
                argb(palette.key_depth, 255),
            );
            graphics.fill_round(bounds, radius, argb(palette.key_face, 255));
            graphics.stroke_round(ui::inset(bounds, 0.5), radius, argb(palette.key_border, 255), 1.0);
            caps.push(cap);
            right -= width + gap;
        }
        drop(graphics);
        let color = if duplicate && !recording { palette.warning } else { palette.text };
        for (cap, part) in caps.into_iter().rev().zip(&parts) {
            self.text(cap, part, font, color, DT_CENTER | DT_VCENTER);
        }
    }

    unsafe fn general(&self) {
        let palette = self.palette;
        let card = self.layout.general;
        let row = self.s(GENERAL_ROW);
        let half = card.left + (card.right - card.left) / 2;
        let rows = [
            ("开机自动启动", "登录 Windows 后在托盘待命", self.layout.toggle.left),
            ("贴图透明度步长", "贴图上 Ctrl+滚轮每格的变化", self.layout.stepper.left),
            ("外观", "截图工具栏、设置和提示窗口的配色", self.layout.themes[0].left),
        ];
        for (index, (title, description, control)) in rows.into_iter().enumerate() {
            let top = card.top + row * index as i32;
            if index > 0 {
                ui::fill(
                    self.dc,
                    RECT {
                        left: card.left + self.s(18),
                        top,
                        right: card.right - self.s(18),
                        bottom: top + 1,
                    },
                    palette.divider,
                );
            }
            let middle = top + row / 2;
            let left = card.left + self.s(18);
            let right = control - self.s(12);
            self.text(
                RECT {
                    left,
                    top: middle - self.s(21),
                    right,
                    bottom: middle,
                },
                title,
                self.state.fonts.body,
                palette.text,
                DT_LEFT | DT_BOTTOM | DT_END_ELLIPSIS,
            );
            self.text(
                RECT {
                    left,
                    top: middle + self.s(2),
                    right,
                    bottom: middle + self.s(21),
                },
                description,
                self.state.fonts.small,
                palette.secondary,
                DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
            );
        }
        // The right halves of the first two rows: save settings, updates.
        let halves = [
            ("保存截图", "位置、文件名、自动保存、阴影和边框", self.layout.output.left),
            ("自动检查更新", "启动后查看新版本，有就在右下角提示", self.layout.auto_update.left),
        ];
        for (index, (title, description, control)) in halves.into_iter().enumerate() {
            let top = card.top + row * index as i32;
            let middle = top + row / 2;
            ui::fill(
                self.dc,
                RECT { left: half, top: top + self.s(10), right: half + 1, bottom: top + row - self.s(10) },
                palette.divider,
            );
            let text = RECT { left: half + self.s(18), right: control - self.s(12), ..card };
            self.text(
                RECT { top: middle - self.s(21), bottom: middle, ..text },
                title,
                self.state.fonts.body,
                palette.text,
                DT_LEFT | DT_BOTTOM | DT_END_ELLIPSIS,
            );
            self.text(
                RECT { top: middle + self.s(2), bottom: middle + self.s(21), ..text },
                description,
                self.state.fonts.small,
                palette.secondary,
                DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
            );
        }
        let radius = self.f(8.0);
        ui::button(
            self.dc,
            self.layout.output,
            "设置…",
            self.state.fonts.body,
            palette,
            false,
            self.face(Target::Output, palette.key_face),
            radius,
        );
        self.toggle();
        self.switch(self.layout.auto_update, self.state.draft.auto_update, Target::AutoUpdate);
        self.stepper();
        self.theme_chips();
    }

    unsafe fn toggle(&self) {
        let palette = self.palette;
        let rect = self.layout.toggle;
        let Some(graphics) = Graphics::new(self.dc) else {
            return;
        };
        let t = self.state.toggle;
        let hover = self.hover(Target::Toggle);
        let off = mix(palette.track_off, palette.secondary, hover * 0.3);
        let on = mix(palette.accent, palette.accent_hover, hover);
        let bounds = ui::to_f(rect);
        let height = bounds[3] - bounds[1];
        graphics.fill_round(bounds, height / 2.0, argb(mix(off, on, t), 255));
        let margin = self.f(3.0);
        let knob = height - margin * 2.0;
        let travel = bounds[2] - bounds[0] - margin * 2.0 - knob;
        let left = bounds[0] + margin + travel * t;
        let top = bounds[1] + margin;
        graphics.fill_ellipse(
            [left, top + self.f(0.8), left + knob, top + knob + self.f(0.8)],
            argb(0, 50),
        );
        graphics.fill_ellipse([left, top, left + knob, top + knob], argb(0x00ff_ffff, 255));
    }

    unsafe fn stepper(&self) {
        let palette = self.palette;
        let layout = &self.layout;
        if let Some(graphics) = Graphics::new(self.dc) {
            let radius = self.f(8.0);
            graphics.fill_round(ui::to_f(layout.stepper), radius, argb(palette.key_face, 255));
            for (target, rect) in [(Target::Minus, layout.minus), (Target::Plus, layout.plus)] {
                let face = self.face(target, palette.key_face);
                graphics.fill_round(
                    ui::to_f(ui::inflate(rect, -self.s(2))),
                    self.f(6.0),
                    argb(face, 255),
                );
            }
            graphics.stroke_round(
                ui::inset(ui::to_f(layout.stepper), 0.5),
                radius,
                argb(palette.key_border, 255),
                1.0,
            );
            // Minus and plus glyphs as crisp bars.
            let bar = self.f(10.0);
            let thick = self.f(1.6).max(1.2);
            for (rect, vertical) in [(layout.minus, false), (layout.plus, true)] {
                let (cx, cy) = (
                    (rect.left + rect.right) as f32 / 2.0,
                    (rect.top + rect.bottom) as f32 / 2.0,
                );
                graphics.fill_round(
                    [cx - bar / 2.0, cy - thick / 2.0, cx + bar / 2.0, cy + thick / 2.0],
                    thick / 2.0,
                    argb(palette.icon, 255),
                );
                if vertical {
                    graphics.fill_round(
                        [cx - thick / 2.0, cy - bar / 2.0, cx + thick / 2.0, cy + bar / 2.0],
                        thick / 2.0,
                        argb(palette.icon, 255),
                    );
                }
            }
        }
        let value = RECT {
            left: layout.minus.right,
            right: layout.plus.left,
            ..layout.stepper
        };
        self.text(
            value,
            &format!("{}%", self.state.draft.opacity_step),
            self.state.fonts.key,
            palette.text,
            DT_CENTER | DT_VCENTER,
        );
    }

    /// One chip per theme: a miniature of its card and accent, plus its name.
    unsafe fn theme_chips(&self) {
        let palette = self.palette;
        for (index, theme) in THEMES.into_iter().enumerate() {
            let rect = self.layout.themes[index];
            let selected = self.state.draft.theme == theme;
            let target = Target::Theme(index);
            let Some(graphics) = Graphics::new(self.dc) else {
                return;
            };
            let bounds = ui::to_f(rect);
            let radius = self.f(8.0);
            let face = if selected {
                palette.accent_soft
            } else {
                self.face(target, palette.key_face)
            };
            graphics.fill_round(bounds, radius, argb(face, 255));
            let (border, width) = if selected {
                (palette.accent, 1.5)
            } else {
                (palette.key_border, 1.0)
            };
            graphics.stroke_round(ui::inset(bounds, width / 2.0), radius, argb(border, 255), width);
            // Swatch: the theme's background with its accent dot.
            let swatch = theme.palette();
            let size = self.f(16.0);
            let x = bounds[0] + self.f(9.0);
            let y = (bounds[1] + bounds[3] - size) / 2.0;
            graphics.fill_ellipse([x, y, x + size, y + size], argb(swatch.card, 255));
            graphics.stroke_round([x, y, x + size, y + size], size / 2.0, argb(palette.key_border, 255), 1.0);
            let dot = self.f(7.0);
            let (dx, dy) = (x + (size - dot) / 2.0, y + (size - dot) / 2.0);
            graphics.fill_ellipse([dx, dy, dx + dot, dy + dot], argb(swatch.accent, 255));
            drop(graphics);
            let label = RECT {
                left: rect.left + self.s(30),
                right: rect.right - self.s(4),
                ..rect
            };
            let color = if selected { palette.accent } else { palette.text };
            self.text(label, theme.name(), self.state.fonts.small, color, DT_LEFT | DT_VCENTER);
        }
    }

    unsafe fn footer(&self) {
        let palette = self.palette;
        let fonts = &self.state.fonts;
        let layout = &self.layout;
        ui::fill(
            self.dc,
            RECT {
                left: 1,
                top: layout.footer,
                right: layout.width - 1,
                bottom: layout.footer + 1,
            },
            palette.card_border,
        );
        let blocked = self.state.draft.first_duplicate();
        let radius = self.f(8.0);
        let hover = self.hover(Target::Reset);
        if hover > 0.0 || self.state.pressed == Some(Target::Reset) {
            if let Some(graphics) = Graphics::new(self.dc) {
                let face = self.face(Target::Reset, palette.background);
                graphics.fill_round(ui::to_f(layout.reset), radius, argb(face, 255));
            }
        }
        self.text(layout.reset, "恢复默认", fonts.body, palette.secondary, DT_CENTER | DT_VCENTER);
        ui::button(
            self.dc,
            layout.cancel,
            "取消",
            fonts.body,
            palette,
            false,
            self.face(Target::Cancel, palette.card),
            radius,
        );
        let save_face = if blocked.is_some() {
            palette.accent_muted
        } else if self.state.pressed == Some(Target::Save) {
            mix(palette.accent_hover, 0, 0.12)
        } else {
            mix(palette.accent, palette.accent_hover, self.hover(Target::Save))
        };
        ui::button(self.dc, layout.save, "保存", fonts.heading, palette, true, save_face, radius);

        let (message, color) = match (&self.state.notice, blocked) {
            (Notice::Error(error), _) => (error.clone(), palette.error),
            (Notice::Hint(hint), _) => (hint.clone(), palette.warning),
            (Notice::None, Some((first, second))) => (
                format!(
                    "「{}」和「{}」都是 {}",
                    field_label(first),
                    field_label(second),
                    self.state.draft.slot(first).map(Shortcut::label).unwrap_or_default()
                ),
                palette.warning,
            ),
            (Notice::None, None) => (
                "点击按键可以修改 · Enter 保存 · Esc 关闭".into(),
                palette.tertiary,
            ),
        };
        let rect = RECT {
            left: layout.reset.right + self.s(10),
            top: layout.reset.top,
            right: layout.cancel.left - self.s(12),
            bottom: layout.reset.bottom,
        };
        // Wrap rather than cut off, centered on the buttons.
        let wrap = DT_WORDBREAK | DT_EDITCONTROL;
        let height = ui::wrapped_height(self.dc, &message, fonts.small, rect.right - rect.left, wrap);
        let top = (rect.top + rect.bottom - height) / 2;
        self.text(
            RECT {
                top,
                bottom: top + height,
                ..rect
            },
            &message,
            fonts.small,
            color,
            DT_LEFT | DT_TOP | wrap,
        );
    }
}

fn field_label(index: usize) -> &'static str {
    if index == SECONDARY {
        "副截图键"
    } else {
        BINDINGS[index].label()
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
        Binding::ExtractText => Icon::ExtractText,
        Binding::Translate => Icon::Translate,
        Binding::SearchImage => Icon::SearchImage,
        Binding::Arrow => Icon::Arrow,
        Binding::Number => Icon::Number,
        Binding::Magnify => Icon::Magnify,
        Binding::Select => Icon::Select,
        Binding::CaptureWindow => Icon::Window,
        Binding::LastCapture => Icon::History,
        Binding::ShowPointer => Icon::MouseLeft,
        // The same keycap look as the panel it switches; 键鼠显示 is on the other tab.
        Binding::ShowHints => Icon::Keyboard,
        Binding::RecordScreen => Icon::Record,
        Binding::Record => Icon::Video,
        Binding::RecordPause => Icon::Pause,
        Binding::ShowKeys => Icon::Keyboard,
        Binding::ShowPad => Icon::Gamepad,
    }
}

fn modifier_names(modifiers: u32) -> Vec<&'static str> {
    [
        (MOD_CONTROL, "Ctrl"),
        (MOD_ALT, "Alt"),
        (MOD_SHIFT, "Shift"),
        (MOD_WIN, "Win"),
    ]
    .into_iter()
    .filter(|(mask, _)| modifiers & mask != 0)
    .map(|(_, name)| name)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

    /// Renders the window in its main states, every theme, and the dialogs to
    /// `%TEMP%\snapshot-preview`. Run with `<test binary> preview --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        use crate::capture::{Desktop, Surface};
        let folder = std::env::temp_dir().join("snapshot-preview");
        std::fs::create_dir_all(&folder).unwrap();
        let mut duplicate = Draft::defaults();
        duplicate
            .bindings
            .set(Binding::Copy, Shortcut::parse("Ctrl+S").unwrap());
        let mut cases: Vec<(String, f32, Draft, CaptureStatus, Option<usize>, u32)> = THEMES
            .into_iter()
            .map(|theme| {
                (
                    format!("theme-{}", theme.key()),
                    1.0,
                    Draft {
                        theme,
                        ..Draft::defaults()
                    },
                    CaptureStatus::Active,
                    None,
                    0,
                )
            })
            .collect();
        cases.extend([
            ("occupied".into(), 1.0, Draft::defaults(), CaptureStatus::Occupied, None, 0),
            ("record-tab".into(), 1.0, Draft::defaults(), CaptureStatus::Active, None, 0),
            (
                "recording".into(),
                1.0,
                Draft::defaults(),
                CaptureStatus::Active,
                Some(0),
                MOD_CONTROL | MOD_ALT,
            ),
            ("duplicate".into(), 1.0, duplicate, CaptureStatus::Active, None, 0),
            (
                "secondary".into(),
                1.0,
                Draft {
                    secondary: Shortcut::parse("Ctrl+Shift+A"),
                    ..Draft::defaults()
                },
                CaptureStatus::Active,
                None,
                0,
            ),
            (
                "scale150".into(),
                1.5,
                Draft {
                    start_at_login: false,
                    ..Draft::defaults()
                },
                CaptureStatus::Available,
                None,
                0,
            ),
        ]);
        unsafe {
            for (name, scale, draft, status, recording, held) in cases {
                let state = State {
                    owner: null_mut(),
                    draft,
                    capture_status: [
                        status,
                        if draft.secondary.is_some() {
                            CaptureStatus::Occupied
                        } else {
                            CaptureStatus::Unset
                        },
                    ],
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
                    mascot: crate::app_icon::decode(MASCOT_ART),
                    tab: usize::from(name == "record-tab"),
                    record_folder: String::new(),
                };
                let layout = Layout::new(scale);
                let bounds = RECT {
                    right: layout.width,
                    bottom: layout.height,
                    ..Default::default()
                };
                let surface = Surface::new(null_mut(), bounds.right, bounds.bottom).unwrap();
                Painter::new(surface.dc, &state).paint(bounds);
                GdiFlush();
                let mut image = Desktop { bounds, surface }.crop(bounds).unwrap();
                for pixel in image.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                image
                    .save_png(&folder.join(format!("settings-{name}.png")))
                    .unwrap();
            }
            for theme in [Theme::Graphite, Theme::Light] {
                theme::set(theme);
                crate::dialog::preview(
                    crate::dialog::Kind::Error,
                    "无法注册截图快捷键 Ctrl+Shift+A。\n它可能已被其他程序占用。可以双击托盘图标截图，或者在设置里换一个快捷键。",
                    1.0,
                    i32::MAX,
                )
                .save_png(&folder.join(format!("dialog-{}.png", theme.key())))
                .unwrap();
            }
            let long = format!(
                "长截图拼接失败，因为连续多帧找不到重叠区域，可能是页面里有固定悬浮的元素。\n{}C:\\Users\\ERIC\\AppData\\Local\\Temp\\snapshot-ai\\long-capture\\frame-0042-overlap-search-failed.png",
                "错误详情会完整显示，太长时可以滚动查看。\n".repeat(30)
            );
            for (name, max_height) in [("long", i32::MAX), ("scroll", 420)] {
                crate::dialog::preview(crate::dialog::Kind::Error, &long, 1.0, max_height)
                    .save_png(&folder.join(format!("dialog-{name}.png")))
                    .unwrap();
            }
            theme::set(Theme::default());
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
        assert_eq!(draft.theme, Theme::Graphite, "dark by default");
        assert_eq!(draft.preferences().unwrap(), Preferences::default());
        draft.step_opacity(-10);
        assert_eq!(draft.opacity_step, 1);
        draft.step_opacity(500);
        assert_eq!(draft.opacity_step, 100);
        let copy = Binding::Copy as usize;
        let save = Binding::Save as usize;
        draft
            .bindings
            .set(Binding::Copy, Shortcut::parse("Ctrl+S").unwrap());
        assert_eq!(draft.duplicate_of(copy), Some(save));
        assert_eq!(draft.first_duplicate(), Some((save.min(copy), save.max(copy))));
        assert!(draft.preferences().is_err());

        let mut draft = Draft::defaults();
        assert_eq!(draft.slot(SECONDARY), None, "no second capture key by default");
        draft.set_slot(SECONDARY, Shortcut::parse("Ctrl+C").unwrap());
        assert_eq!(draft.duplicate_of(SECONDARY), Some(copy));
        assert!(draft.preferences().is_err(), "second key may not reuse a binding");
        draft.set_slot(SECONDARY, Shortcut::parse("F1").unwrap());
        assert!(draft.first_duplicate().is_none());
        let preferences = draft.preferences().unwrap();
        assert_eq!(preferences.globals(false).len(), 8, "main, second, window, last capture, record, full screen and the two panels");
    }

    #[test]
    fn every_target_is_hit_at_its_center_on_any_scale() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            for tab in 0..2 {
            let layout = Layout::for_tab(scale, tab);
            for (target, rect) in layout.targets() {
                let center = POINT {
                    x: (rect.left + rect.right) / 2,
                    y: (rect.top + rect.bottom) / 2,
                };
                assert_eq!(layout.hit(center), Some(target), "{target:?} at {scale}");
                assert!(rect.right <= layout.width && rect.bottom <= layout.height);
            }
            assert!(
                layout.height < (720.0 * scale) as i32,
                "fits a 1080p screen: {} at {scale}",
                layout.height
            );
            }
        }
    }

    #[test]
    fn clicking_records_keys_picks_themes_and_escape_closes() {
        unsafe {
            let instance = GetModuleHandleW(null());
            let _ = register(instance);
            let window = CreateWindowExW(
                0,
                CLASS,
                null(),
                WS_POPUP,
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
            let click = |rect: RECT| {
                let lparam = (((rect.top + rect.bottom) / 2) << 16 | (rect.left + rect.right) / 2)
                    as LPARAM;
                SendMessageW(window, WM_LBUTTONDOWN, 0, lparam);
                SendMessageW(window, WM_LBUTTONUP, 0, lparam);
            };
            let state = |read: fn(&State) -> bool| STATE.with_borrow(|s| s.as_ref().is_some_and(read));
            click(layout.keys[Binding::Copy as usize]);
            assert!(state(|s| s.recording == Some(Binding::Copy as usize)));
            // Painting while recording must not panic.
            paint_window(window, GetDC(window));
            SendMessageW(window, WM_KEYDOWN, VK_ESCAPE as usize, 0);
            assert!(state(|s| s.recording.is_none()));
            click(layout.toggle);
            assert!(state(|s| !s.draft.start_at_login));
            click(layout.themes[3]);
            assert!(state(|s| s.draft.theme == Theme::Light));
            assert_eq!(theme::current(), Theme::Graphite, "applied only on save");
            SendMessageW(window, WM_KEYDOWN, VK_ESCAPE as usize, 0);
            assert!(STATE.with_borrow(|s| s.is_none()), "Esc closes when idle");
        }
    }
}
