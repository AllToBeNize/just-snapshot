//! The image search window: pick an engine, edit where the picture goes and
//! how the result page is found, add your own, test, save. One set of edit
//! boxes shows the chosen engine; switching engines keeps what was typed.

use std::{
    cell::{Cell, RefCell},
    io,
    mem::size_of,
    ptr::{null, null_mut},
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
            Input::KeyboardAndMouse::{
                ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
            },
            Shell::ShellExecuteW,
            WindowsAndMessaging::*,
        },
    },
};

use crate::{
    gdiplus::Graphics,
    icons::{self, Icon},
    image_search::{self, Engine, FIELDS},
    selection_geometry::{contains, mouse_point},
    theme::{self, argb, mix},
    ui::{self, Fonts},
    wide,
};

const CLASS: *const u16 = w!("Snapshot.ImageSearchSettings");
const WIDTH: i32 = 560;
const PAD: i32 = 24;
const HEADER: i32 = 52;
const LABEL: i32 = 22;
const CHIP_HEIGHT: i32 = 34;
const CHIP_GAP: i32 = 8;
const CHIPS_PER_ROW: i32 = 4;
const FIELD: i32 = 38;
const FIELD_GAP: i32 = 10;
const FIELD_LABEL: i32 = 84;
const FOOTER: i32 = 60;
const GAP: i32 = 16;
const WRAP: u32 = DT_WORDBREAK | DT_EDITCONTROL;
/// Posted by the test thread; `wparam` is the test id, `lparam` owns a `Box<Result<String, String>>`.
const TEST_DONE: u32 = WM_APP + 1;
const FIRST_EDIT: usize = 100;
const EM_SETSEL: u32 = 0x00b1;
const EN_SETFOCUS: u32 = 0x0100;
const EN_KILLFOCUS: u32 = 0x0200;
const HINT: &str = "截图以 multipart 表单上传到「上传地址」，地址里的 {timestamp} 会换成当前时间。「结果字段」留空时由浏览器上传并跟着跳转打开结果页（适合 Google Lens 这类结果和浏览器会话绑定的服务）；填了就由程序上传，按路径（例如 blocks.0.params.cbirId）从返回的 JSON 里取值，代入「结果链接」里的 {value}。";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Engine(usize),
    Add,
    Close,
    Test,
    /// "恢复默认" for a built-in engine, "删除" for one the user added.
    Reset,
    Cancel,
    Save,
}

enum Status {
    None,
    Testing,
    Success(String),
    Error(String),
}

struct State {
    engines: Vec<Engine>,
    selected: usize,
    /// One box per field, showing the selected engine.
    edits: [HWND; FIELDS.len()],
    hovered: Option<Target>,
    pressed: Option<Target>,
    focused: Option<HWND>,
    status: Status,
    test: u32,
    scale: f32,
    fonts: Fonts,
    brush: HBRUSH,
    tracking: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        if !self.brush.is_null() {
            unsafe { DeleteObject(self.brush) };
        }
    }
}

thread_local! {
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct Layout {
    scale: f32,
    width: i32,
    height: i32,
    close: RECT,
    engine_label: RECT,
    /// One chip per engine, then "新增".
    chips: Vec<RECT>,
    fields: [RECT; FIELDS.len()],
    hint: RECT,
    status: RECT,
    footer: i32,
    test: RECT,
    reset: RECT,
    cancel: RECT,
    save: RECT,
}

impl Layout {
    unsafe fn new(state: &State) -> Self {
        let scale = state.scale;
        let s = |value: i32| (value as f32 * scale).round() as i32;
        let dc = GetDC(null_mut());
        let inner = s(WIDTH - PAD * 2);
        let hint_height = ui::wrapped_height(dc, HINT, state.fonts.small, inner, WRAP);
        let status_height = match status_text(&state.status) {
            Some((text, _)) => ui::wrapped_height(dc, &text, state.fonts.small, inner, WRAP),
            None => 0,
        };
        ReleaseDC(null_mut(), dc);

        let close = RECT { left: s(WIDTH - 12 - 36), top: s(10), right: s(WIDTH - 12), bottom: s(46) };
        let mut y = s(HEADER);
        let engine_label = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + s(LABEL) };
        y += s(LABEL);
        let chip_width = (inner - s(CHIP_GAP) * (CHIPS_PER_ROW - 1)) / CHIPS_PER_ROW;
        let count = state.engines.len() + 1;
        let chips: Vec<RECT> = (0..count as i32)
            .map(|index| {
                let left = s(PAD) + (index % CHIPS_PER_ROW) * (chip_width + s(CHIP_GAP));
                let top = y + (index / CHIPS_PER_ROW) * (s(CHIP_HEIGHT) + s(CHIP_GAP));
                RECT { left, top, right: left + chip_width, bottom: top + s(CHIP_HEIGHT) }
            })
            .collect();
        y = chips.last().map_or(y, |chip| chip.bottom) + s(GAP + 4);
        let mut fields = [RECT::default(); FIELDS.len()];
        for field in &mut fields {
            *field = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + s(FIELD) };
            y += s(FIELD + FIELD_GAP);
        }
        y += s(GAP - FIELD_GAP);
        let hint = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + hint_height };
        y += hint_height;
        let status = RECT { left: s(PAD), top: y + s(10), right: s(WIDTH - PAD), bottom: y + s(10) + status_height };
        if status_height > 0 {
            y += s(10) + status_height;
        }
        y += s(GAP);
        let footer = y;
        let button_top = y + (s(FOOTER) - s(34)) / 2;
        let button = |left: i32, width: i32| RECT { left, top: button_top, right: left + width, bottom: button_top + s(34) };
        let save = button(s(WIDTH - PAD - 84), s(84));
        let cancel = button(save.left - s(10 + 84), s(84));
        let test = button(s(PAD), s(96));
        let reset = button(test.right + s(10), s(96));
        Self {
            scale,
            width: s(WIDTH),
            height: y + s(FOOTER),
            close,
            engine_label,
            chips,
            fields,
            hint,
            status,
            footer,
            test,
            reset,
            cancel,
            save,
        }
    }

    fn s(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    fn edit_rect(&self, row: RECT, font_height: i32) -> RECT {
        let height = font_height + self.s(4);
        let top = (row.top + row.bottom - height) / 2;
        RECT { left: row.left + self.s(FIELD_LABEL + 12), top, right: row.right - self.s(12), bottom: top + height }
    }

    fn box_rect(&self, row: RECT) -> RECT {
        RECT { left: row.left + self.s(FIELD_LABEL), ..row }
    }

    fn targets(&self) -> impl Iterator<Item = (Target, RECT)> + '_ {
        let add = self.chips.len() - 1;
        self.chips
            .iter()
            .enumerate()
            .map(move |(index, chip)| (if index == add { Target::Add } else { Target::Engine(index) }, *chip))
            .chain([
                (Target::Close, self.close),
                (Target::Test, self.test),
                (Target::Reset, self.reset),
                (Target::Cancel, self.cancel),
                (Target::Save, self.save),
            ])
    }

    fn hit(&self, point: POINT) -> Option<Target> {
        self.targets().find(|(_, rect)| contains(*rect, point)).map(|(target, _)| target)
    }
}

fn status_text(status: &Status) -> Option<(String, bool)> {
    match status {
        Status::None => None,
        Status::Testing => Some(("正在上传测试图片…".into(), true)),
        Status::Success(text) => Some((text.clone(), true)),
        Status::Error(text) => Some((text.clone(), false)),
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

/// Lets Tab move between the boxes and Enter/Esc save or close.
pub unsafe fn dialog_message(message: &MSG) -> bool {
    let window = WINDOW.get();
    !window.is_null() && IsDialogMessageW(window, message) != 0
}

/// Opens the window, or brings it forward if it is already open.
pub unsafe fn open(near: HWND) -> io::Result<()> {
    let existing = WINDOW.get();
    if !existing.is_null() {
        SetForegroundWindow(existing);
        return Ok(());
    }
    let mut engines = image_search::load().map_err(io::Error::other)?;
    if engines.is_empty() {
        engines = image_search::PRESETS.into_iter().map(image_search::Preset::engine).collect();
    }
    // Topmost so it shows above the capture or pin that opened it.
    let window = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_CONTROLPARENT,
        CLASS,
        w!("just-snapshot 识图接口"),
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
    let mut edits = [null_mut(); FIELDS.len()];
    for (index, edit) in edits.iter_mut().enumerate() {
        *edit = CreateWindowExW(
            0,
            w!("EDIT"),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            0,
            0,
            0,
            0,
            window,
            (FIRST_EDIT + index) as _,
            GetModuleHandleW(null()),
            null(),
        );
    }
    STATE.set(Some(State {
        engines,
        selected: 0,
        edits,
        hovered: None,
        pressed: None,
        focused: None,
        status: Status::None,
        test: 0,
        scale,
        fonts: Fonts::new(scale),
        brush: CreateSolidBrush(theme::palette().card),
        tracking: false,
    }));
    WINDOW.set(window);
    show_engine();
    place(window, near);
    ui::round_corners(window);
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    focus_first();
    Ok(())
}

/// Fills the boxes from the selected engine and sets their cue banners.
unsafe fn show_engine() {
    let boxes = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        let engine = state.engines.get(state.selected)?;
        Some(FIELDS.map(|field| (wide(engine.get(field)), wide(field.placeholder()))))
    });
    let Some(boxes) = boxes else { return };
    let edits = STATE.with_borrow(|state| state.as_ref().map(|state| state.edits)).unwrap_or_default();
    for (edit, (value, cue)) in edits.into_iter().zip(&boxes) {
        SetWindowTextW(edit, value.as_ptr());
        // EM_SETCUEBANNER, shown while the box is empty.
        SendMessageW(edit, 0x1501, 1, cue.as_ptr() as LPARAM);
    }
}

/// Copies the boxes back into the selected engine.
unsafe fn commit() {
    let edits = STATE.with_borrow(|state| state.as_ref().map(|state| state.edits));
    let Some(edits) = edits else { return };
    let values = edits.map(|edit| text_of(edit));
    with_state(|state| {
        if let Some(engine) = state.engines.get_mut(state.selected) {
            for (field, value) in FIELDS.into_iter().zip(&values) {
                engine.set(field, value);
            }
        }
    });
}

unsafe fn focus_first() {
    if let Some(edit) = STATE.with_borrow(|state| state.as_ref().map(|state| state.edits[0])) {
        SetFocus(edit);
        SendMessageW(edit, EM_SETSEL, 0, -1);
    }
}

/// Sizes the window to the layout, lays out the boxes and, on first show,
/// centers it on the monitor of `near`.
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
    let boxes = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        let mut metrics = TEXTMETRICW::default();
        let dc = GetDC(window);
        SelectObject(dc, state.fonts.body);
        GetTextMetricsW(dc, &mut metrics);
        ReleaseDC(window, dc);
        Some((state.edits, state.fonts.body, layout.fields.map(|row| layout.edit_rect(row, metrics.tmHeight))))
    });
    let Some((edits, font, rects)) = boxes else { return };
    for (edit, rect) in edits.into_iter().zip(rects) {
        SendMessageW(edit, WM_SETFONT, font as WPARAM, 0);
        SetWindowPos(
            edit,
            null_mut(),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    InvalidateRect(window, null(), 0);
}

fn with_state<R>(action: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with_borrow_mut(|state| state.as_mut().map(action))
}

unsafe fn text_of(edit: HWND) -> String {
    let length = GetWindowTextLengthW(edit);
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(edit, buffer.as_mut_ptr(), buffer.len() as i32);
    String::from_utf16_lossy(&buffer[..copied.max(0) as usize]).trim().to_string()
}

unsafe fn set_status(window: HWND, status: Status) {
    with_state(|state| state.status = status);
    // The status line changes the window height.
    place(window, null_mut());
}

/// Switches to engine `index` after keeping what was typed for the current one.
unsafe fn select(window: HWND, index: usize) {
    commit();
    with_state(|state| {
        state.selected = index.min(state.engines.len().saturating_sub(1));
        state.status = Status::None;
    });
    show_engine();
    place(window, null_mut());
    focus_first();
}

unsafe fn selected_engine() -> Option<Engine> {
    STATE.with_borrow(|state| state.as_ref().and_then(|state| state.engines.get(state.selected).cloned()))
}

unsafe fn test(window: HWND) {
    commit();
    let Some(engine) = selected_engine() else { return };
    if let Some(problem) = engine.problem() {
        set_status(window, Status::Error(problem));
        return;
    }
    let id = with_state(|state| {
        state.test = state.test.wrapping_add(1);
        state.test
    })
    .unwrap_or(0);
    set_status(window, Status::Testing);
    let target = window as isize;
    std::thread::spawn(move || {
        // The app icon stands in for a capture.
        let mut png = Vec::new();
        let result = crate::app_icon::render(256, false)
            .write_png_fast(&mut png)
            .map_err(|error| format!("无法生成测试图片：{error}"))
            .and_then(|()| engine.search(&png));
        let result = Box::into_raw(Box::new(result));
        if PostMessageW(target as HWND, TEST_DONE, id as WPARAM, result as LPARAM) == 0 {
            drop(Box::from_raw(result));
        }
    });
}

/// Resets a built-in engine to how it ships, or deletes one the user added.
unsafe fn reset(window: HWND) {
    commit();
    let deleted = with_state(|state| {
        let engine = state.engines.get_mut(state.selected)?;
        match engine.preset {
            Some(preset) => {
                *engine = preset.engine();
                Some(false)
            }
            None => {
                state.engines.remove(state.selected);
                state.selected = state.selected.saturating_sub(1);
                Some(true)
            }
        }
    })
    .flatten();
    if deleted.is_some() {
        show_engine();
        set_status(window, Status::None);
        focus_first();
    }
}

unsafe fn add(window: HWND) {
    commit();
    let index = with_state(|state| {
        state.engines.push(Engine { name: "新引擎".into(), field: "file".into(), ..Engine::default() });
        state.engines.len() - 1
    });
    if let Some(index) = index {
        with_state(|state| {
            state.selected = index;
            state.status = Status::None;
        });
        show_engine();
        place(window, null_mut());
        focus_first();
    }
}

unsafe fn save(window: HWND) {
    commit();
    let Some(engines) = STATE.with_borrow(|state| state.as_ref().map(|state| state.engines.clone())) else { return };
    if let Some((index, problem)) = engines.iter().enumerate().find_map(|(index, engine)| Some((index, engine.problem()?))) {
        select(window, index);
        set_status(window, Status::Error(problem));
        return;
    }
    match image_search::save(&engines) {
        Ok(()) => {
            DestroyWindow(window);
        }
        Err(error) => set_status(window, Status::Error(error)),
    }
}

unsafe fn activate(window: HWND, target: Target) {
    match target {
        Target::Engine(index) => select(window, index),
        Target::Add => add(window),
        Target::Close | Target::Cancel => {
            DestroyWindow(window);
        }
        Target::Test => test(window),
        Target::Reset => reset(window),
        Target::Save => save(window),
    }
}

unsafe fn layout() -> Option<Layout> {
    STATE.with_borrow(|state| state.as_ref().map(|state| Layout::new(state)))
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
            if let Some(layout) = layout() {
                if point.y < layout.s(HEADER) && layout.hit(point).is_none() {
                    return HTCAPTION as LRESULT;
                }
            }
            return HTCLIENT as LRESULT;
        }
        WM_MOUSEMOVE => {
            if with_state(|state| !std::mem::replace(&mut state.tracking, true)) == Some(true) {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: window,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut track);
            }
            let hit = layout().and_then(|layout| layout.hit(mouse_point(lparam)));
            if with_state(|state| std::mem::replace(&mut state.hovered, hit) != hit) == Some(true) {
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
            let hit = layout().and_then(|layout| layout.hit(mouse_point(lparam)));
            with_state(|state| state.pressed = hit);
            SetCapture(window);
            InvalidateRect(window, null(), 0);
        }
        WM_LBUTTONUP => {
            let pressed = with_state(|state| state.pressed.take()).flatten();
            ReleaseCapture();
            InvalidateRect(window, null(), 0);
            let hit = layout().and_then(|layout| layout.hit(mouse_point(lparam)));
            if let Some(target) = pressed.filter(|&target| Some(target) == hit) {
                activate(window, target);
            }
        }
        WM_CAPTURECHANGED => {
            with_state(|state| state.pressed = None);
        }
        WM_COMMAND => {
            let id = wparam & 0xffff;
            let code = (wparam >> 16) as u32 & 0xffff;
            match id {
                // From IsDialogMessage: Enter and Esc.
                1 => save(window),
                2 => {
                    DestroyWindow(window);
                }
                _ if id >= FIRST_EDIT => match code {
                    EN_SETFOCUS => {
                        with_state(|state| state.focused = Some(lparam as HWND));
                        InvalidateRect(window, null(), 0);
                    }
                    EN_KILLFOCUS => {
                        with_state(|state| state.focused = None);
                        InvalidateRect(window, null(), 0);
                        // The chip shows the name as it is typed.
                        if id == FIRST_EDIT {
                            commit();
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        WM_CTLCOLOREDIT => {
            let palette = theme::palette();
            let dc = wparam as HDC;
            SetTextColor(dc, palette.text);
            SetBkColor(dc, palette.card);
            return STATE.with_borrow(|state| state.as_ref().map_or(0, |state| state.brush as LRESULT));
        }
        TEST_DONE => {
            let result = *Box::from_raw(lparam as *mut Result<String, String>);
            let current = STATE.with_borrow(|state| state.as_ref().is_some_and(|state| state.test == wparam as u32));
            if current {
                let status = match result {
                    Ok(page) => {
                        let address = wide(&page);
                        ShellExecuteW(null_mut(), w!("open"), address.as_ptr(), null(), null(), SW_SHOWNORMAL);
                        Status::Success("已在浏览器打开，请在浏览器里确认结果页（测试图片是本程序的图标）。".into())
                    }
                    Err(error) => Status::Error(error),
                };
                set_status(window, status);
            }
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
    ui::text(
        dc,
        RECT { left: s(PAD), top: 0, right: layout.close.left, bottom: s(HEADER) },
        "识图接口",
        fonts.title,
        palette.text,
        DT_LEFT | single,
    );
    if state.hovered == Some(Target::Close) || state.pressed == Some(Target::Close) {
        if let Some(graphics) = Graphics::new(dc) {
            graphics.fill_round(ui::to_f(layout.close), radius, argb(face(Target::Close, palette.background), 255));
        }
    }
    icons::paint(dc, ui::inflate(layout.close, -s(9)), Icon::Close, palette.secondary);

    ui::text(
        dc,
        RECT { left: layout.engine_label.left + s(4), bottom: layout.engine_label.bottom - s(6), ..layout.engine_label },
        "引擎（截图工具栏、贴图和托盘的识图菜单按这个顺序列出）",
        fonts.small,
        palette.secondary,
        DT_LEFT | DT_BOTTOM | DT_SINGLELINE,
    );
    for (index, chip) in layout.chips.iter().enumerate() {
        let engine = state.engines.get(index);
        let selected = engine.is_some() && index == state.selected;
        let target = if engine.is_some() { Target::Engine(index) } else { Target::Add };
        if let Some(graphics) = Graphics::new(dc) {
            let fill = if selected { palette.accent_soft } else { face(target, palette.card) };
            graphics.fill_round(ui::to_f(*chip), radius, argb(fill, 255));
            let border = if selected { palette.accent } else { palette.key_border };
            graphics.stroke_round(ui::inset(ui::to_f(*chip), 0.5), radius, argb(border, 255), 1.0);
        }
        let font = if selected { fonts.heading } else { fonts.body };
        let (label, color) = match engine {
            Some(engine) if engine.name.is_empty() => ("未命名", palette.tertiary),
            Some(engine) => (engine.name.as_str(), if selected { palette.accent } else { palette.text }),
            None => ("＋ 新增", palette.secondary),
        };
        ui::text(dc, ui::inflate(*chip, -s(6)), label, font, color, DT_CENTER | single | DT_END_ELLIPSIS);
    }

    for (row, (field, edit)) in layout.fields.iter().zip(FIELDS.into_iter().zip(state.edits)) {
        ui::text(dc, RECT { right: row.left + s(FIELD_LABEL - 8), ..*row }, field.label(), fonts.body, palette.secondary, DT_LEFT | single);
        let field_box = layout.box_rect(*row);
        let focused = state.focused == Some(edit);
        if let Some(graphics) = Graphics::new(dc) {
            graphics.fill_round(ui::to_f(field_box), radius, argb(palette.card, 255));
            let (border, width) = if focused { (palette.accent, 1.5) } else { (palette.key_border, 1.0) };
            graphics.stroke_round(ui::inset(ui::to_f(field_box), width / 2.0), radius, argb(border, 255), width);
        }
    }

    ui::text(dc, layout.hint, HINT, fonts.small, palette.secondary, DT_LEFT | DT_TOP | WRAP);
    if let Some((text, good)) = status_text(&state.status) {
        let color = match state.status {
            Status::Testing => palette.secondary,
            _ if good => palette.success,
            _ => palette.error,
        };
        ui::text(dc, layout.status, &text, fonts.small, color, DT_LEFT | DT_TOP | WRAP);
    }

    ui::fill(dc, RECT { left: 1, top: layout.footer, right: layout.width - 1, bottom: layout.footer + 1 }, palette.card_border);
    let testing = matches!(state.status, Status::Testing);
    let test_face = if testing { palette.hover } else { face(Target::Test, palette.card) };
    ui::button(dc, layout.test, if testing { "测试中…" } else { "测试" }, fonts.body, palette, false, test_face, radius);
    let built_in = state.engines.get(state.selected).is_some_and(|engine| engine.preset.is_some());
    let reset_label = if built_in { "恢复默认" } else { "删除" };
    ui::button(dc, layout.reset, reset_label, fonts.body, palette, false, face(Target::Reset, palette.card), radius);
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

    /// Opens the real window (Session 0 is fine) and saves a picture of it,
    /// edit boxes included. Run with `<test binary> image_search_settings --ignored`.
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
                // A process started hidden ignores its first ShowWindow.
                ShowWindow(window, SW_SHOW);
                match step {
                    1 => activate(window, Target::Engine(1)),
                    2 => {
                        activate(window, Target::Add);
                        save(window);
                    }
                    _ => {}
                }
                let mut message = MSG::default();
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    DispatchMessageW(&message);
                }
                let mut bounds = RECT::default();
                GetWindowRect(window, &mut bounds);
                let (width, height) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
                let surface = crate::capture::Surface::new(null_mut(), width, height).unwrap();
                SendMessageW(window, WM_PRINTCLIENT, surface.dc as WPARAM, PRF_CLIENT as LPARAM);
                let edits = STATE.with_borrow(|state| state.as_ref().unwrap().edits);
                let expected = ["Google", "Yandex", "新引擎"][step];
                assert_eq!(text_of(edits[0]), expected, "the boxes show the selected engine");
                for edit in edits {
                    let mut rect = RECT::default();
                    GetWindowRect(edit, &mut rect);
                    let mut origin = POINT { x: rect.left, y: rect.top };
                    ScreenToClient(window, &mut origin);
                    SetViewportOrgEx(surface.dc, origin.x, origin.y, null_mut());
                    SendMessageW(edit, WM_PRINT, surface.dc as WPARAM, (PRF_CLIENT | PRF_ERASEBKGND) as LPARAM);
                    SetViewportOrgEx(surface.dc, 0, 0, null_mut());
                }
                GdiFlush();
                let bounds = RECT { left: 0, top: 0, right: width, bottom: height };
                let mut image = crate::capture::Desktop { bounds, surface }.crop(bounds).unwrap();
                for pixel in image.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                image.save_png(&folder.join(format!("image-search-settings-{step}.png"))).unwrap();
                DestroyWindow(window);
            }
        }
    }
}
