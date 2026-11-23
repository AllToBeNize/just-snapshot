//! The translation service window: pick a provider, fill in its address and
//! keys, test, save. Keys go straight to the Credential Manager and are never
//! read back into the window; a saved key shows as "已保存" and an empty box
//! keeps it.

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
            WindowsAndMessaging::*,
        },
    },
};

use crate::{
    credential,
    gdiplus::Graphics,
    icons::{self, Icon},
    pinned,
    selection_geometry::{contains, mouse_point},
    theme::{self, argb, mix},
    translate::{self, Config, Field, Language, Provider, Service, PROVIDERS},
    ui::{self, Fonts},
    wide,
};

const CLASS: *const u16 = w!("Snapshot.TranslateSettings");
const WIDTH: i32 = 520;
const PAD: i32 = 24;
const HEADER: i32 = 52;
const LABEL: i32 = 22;
const CHIP_HEIGHT: i32 = 34;
const CHIP_GAP: i32 = 8;
const FIELD: i32 = 38;
const FIELD_GAP: i32 = 10;
const FIELD_LABEL: i32 = 84;
const SAVED_TAG: i32 = 60;
const FOOTER: i32 = 60;
const GAP: i32 = 16;
const WRAP: u32 = DT_WORDBREAK | DT_EDITCONTROL;
/// Posted by the test thread; `wparam` is the test id, `lparam` owns a `Box<Result<String, String>>`.
const TEST_DONE: u32 = WM_APP + 1;
const FIRST_EDIT: usize = 100;
const EM_SETSEL: u32 = 0x00b1;
const EN_SETFOCUS: u32 = 0x0100;
const EN_KILLFOCUS: u32 = 0x0200;
const EN_CHANGE: u32 = 0x0300;
const SECURITY_NOTE: &str = "密钥只保存在本机的 Windows 凭据管理器里，不写入任何文件；输入框留空表示沿用已保存的密钥。";
const TEST_TEXT: &str = "Hello, world. This is a translation test.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Provider(usize),
    Close,
    Test,
    Cancel,
    Save,
}

enum Status {
    None,
    Testing,
    Success(String),
    Error(String),
}

/// One edit box per provider field, created up front; only the chosen
/// provider's are visible.
struct Input {
    provider: Provider,
    field: Field,
    edit: HWND,
    /// A key is already in the Credential Manager.
    saved: bool,
}

struct State {
    provider: Provider,
    inputs: Vec<Input>,
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
    provider_label: RECT,
    chips: [RECT; PROVIDERS.len()],
    /// Rows for the chosen provider's fields, in order.
    fields: Vec<RECT>,
    hint: RECT,
    security: RECT,
    status: RECT,
    footer: i32,
    test: RECT,
    cancel: RECT,
    save: RECT,
}

impl Layout {
    unsafe fn new(state: &State) -> Self {
        let scale = state.scale;
        let s = |value: i32| (value as f32 * scale).round() as i32;
        let dc = GetDC(null_mut());
        let inner = s(WIDTH - PAD * 2);
        let hint_height = ui::wrapped_height(dc, state.provider.hint(), state.fonts.small, inner, WRAP);
        // Only services with keys need the note about where keys are kept.
        let has_secret = state.provider.fields().iter().any(|field| field.secret());
        let security_height = if has_secret { ui::wrapped_height(dc, SECURITY_NOTE, state.fonts.small, inner, WRAP) } else { 0 };
        let status_height = match status_text(&state.status) {
            Some((text, _)) => ui::wrapped_height(dc, &text, state.fonts.small, inner, WRAP),
            None => 0,
        };
        ReleaseDC(null_mut(), dc);

        let close = RECT { left: s(WIDTH - 12 - 36), top: s(10), right: s(WIDTH - 12), bottom: s(46) };
        let mut y = s(HEADER);
        let provider_label = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + s(LABEL) };
        y += s(LABEL);
        let count = PROVIDERS.len() as i32;
        let chip_width = (inner - s(CHIP_GAP) * (count - 1)) / count;
        let mut chips = [RECT::default(); PROVIDERS.len()];
        for (index, chip) in chips.iter_mut().enumerate() {
            let left = s(PAD) + index as i32 * (chip_width + s(CHIP_GAP));
            *chip = RECT { left, top: y, right: left + chip_width, bottom: y + s(CHIP_HEIGHT) };
        }
        y += s(CHIP_HEIGHT) + s(GAP + 4);
        let mut fields = Vec::new();
        for _ in state.provider.fields() {
            fields.push(RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + s(FIELD) });
            y += s(FIELD + FIELD_GAP);
        }
        y += s(GAP - FIELD_GAP);
        let hint = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + hint_height };
        y += hint_height + s(8);
        let security = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + security_height };
        y += security_height;
        let status = RECT {
            left: s(PAD),
            top: y + s(10),
            right: s(WIDTH - PAD),
            bottom: y + s(10) + status_height,
        };
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
        Self {
            scale,
            width: s(WIDTH),
            height: y + s(FOOTER),
            close,
            provider_label,
            chips,
            fields,
            hint,
            security,
            status,
            footer,
            test,
            cancel,
            save,
        }
    }

    fn s(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    /// The edit box inside a field row, leaving room for the "已保存" tag.
    fn edit_rect(&self, row: RECT, secret: bool, font_height: i32) -> RECT {
        let height = font_height + self.s(4);
        let top = (row.top + row.bottom - height) / 2;
        RECT {
            left: row.left + self.s(FIELD_LABEL + 12),
            top,
            right: row.right - self.s(12) - if secret { self.s(SAVED_TAG) } else { 0 },
            bottom: top + height,
        }
    }

    fn box_rect(&self, row: RECT) -> RECT {
        RECT { left: row.left + self.s(FIELD_LABEL), ..row }
    }

    fn targets(&self) -> impl Iterator<Item = (Target, RECT)> + '_ {
        self.chips
            .iter()
            .enumerate()
            .map(|(index, chip)| (Target::Provider(index), *chip))
            .chain([
                (Target::Close, self.close),
                (Target::Test, self.test),
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
        Status::Testing => Some(("正在测试…".into(), true)),
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
    let config = Config::load().map_err(io::Error::other)?;
    // Topmost so it is not hidden behind the pin that opened it.
    let window = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_CONTROLPARENT,
        CLASS,
        w!("just-snapshot 翻译服务"),
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
    let palette = theme::palette();
    let mut inputs = Vec::new();
    for provider in PROVIDERS {
        for &field in provider.fields() {
            let style = WS_CHILD | WS_TABSTOP | ES_AUTOHSCROLL as u32 | if field.secret() { ES_PASSWORD as u32 } else { 0 };
            let edit = CreateWindowExW(
                0,
                w!("EDIT"),
                null(),
                style,
                0,
                0,
                0,
                0,
                window,
                (FIRST_EDIT + inputs.len()) as _,
                GetModuleHandleW(null()),
                null(),
            );
            let value = if field.secret() {
                String::new()
            } else {
                Some(config.value(provider, field))
                    .filter(|value| !value.is_empty())
                    .unwrap_or(field.placeholder())
                    .to_string()
            };
            SetWindowTextW(edit, wide(&value).as_ptr());
            let saved = field.secret() && translate::has_secret(provider, field);
            inputs.push(Input { provider, field, edit, saved });
        }
    }
    STATE.set(Some(State {
        provider: config.provider.unwrap_or(Provider::Free),
        inputs,
        hovered: None,
        pressed: None,
        focused: None,
        status: Status::None,
        test: 0,
        scale,
        fonts: Fonts::new(scale),
        brush: CreateSolidBrush(palette.card),
        tracking: false,
    }));
    WINDOW.set(window);
    place(window, near);
    ui::round_corners(window);
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    focus_first(window);
    Ok(())
}

unsafe fn focus_first(window: HWND) {
    let first = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        state.inputs.iter().find(|input| input.provider == state.provider).map(|input| input.edit)
    });
    match first {
        Some(edit) => {
            SetFocus(edit);
            SendMessageW(edit, EM_SETSEL, 0, -1);
        }
        None => {
            SetFocus(window);
        }
    }
}

/// Sizes the window to the layout, lays out the boxes and, on first show,
/// centers it on the monitor of `near`.
unsafe fn place(window: HWND, near: HWND) {
    let Some(layout) = STATE.with_borrow(|state| state.as_ref().map(|state| Layout::new(state))) else {
        return;
    };
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
    // Moving and hiding boxes can shift focus, which notifies this window
    // synchronously, so collect positions first and apply them after.
    let boxes = STATE.with_borrow(|state| {
        let Some(state) = state.as_ref() else { return Vec::new() };
        let mut metrics = TEXTMETRICW::default();
        let dc = GetDC(window);
        SelectObject(dc, state.fonts.body);
        GetTextMetricsW(dc, &mut metrics);
        ReleaseDC(window, dc);
        let mut row = 0;
        state
            .inputs
            .iter()
            .map(|input| {
                let rect = (input.provider == state.provider).then(|| {
                    row += 1;
                    layout.edit_rect(layout.fields[row - 1], input.field.secret(), metrics.tmHeight)
                });
                (input.edit, state.fonts.body, rect)
            })
            .collect()
    });
    for (edit, font, rect) in boxes {
        SendMessageW(edit, WM_SETFONT, font as WPARAM, 0);
        match rect {
            Some(rect) => {
                SetWindowPos(
                    edit,
                    null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
            None => {
                ShowWindow(edit, SW_HIDE);
            }
        }
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
    let text = String::from_utf16_lossy(&buffer[..copied.max(0) as usize]);
    buffer.fill(0);
    text.trim().to_string()
}

/// The chosen provider's values as typed: plain fields and any new secret.
unsafe fn form() -> Option<(Provider, Vec<(Field, String, bool)>)> {
    STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        let values = state
            .inputs
            .iter()
            .filter(|input| input.provider == state.provider)
            .map(|input| (input.field, text_of(input.edit), input.saved))
            .collect();
        Some((state.provider, values))
    })
}

/// Checks required fields; `Err` names the first missing one.
fn missing(provider: Provider, values: &[(Field, String, bool)]) -> Option<&'static str> {
    values
        .iter()
        .find(|(field, value, saved)| {
            let required = match field {
                Field::Endpoint | Field::Model | Field::Email => false,
                Field::ApiKey => provider == Provider::DeepL,
                Field::AppId | Field::Secret => true,
            };
            required && value.is_empty() && !saved
        })
        .map(|(field, _, _)| field.label(provider))
}

unsafe fn service_from_form(provider: Provider, values: &[(Field, String, bool)]) -> Result<Service, String> {
    let get = |wanted: Field| values.iter().find(|(field, _, _)| *field == wanted);
    let secret_field = if matches!(provider, Provider::Baidu | Provider::Youdao) { Field::Secret } else { Field::ApiKey };
    let secret = match get(secret_field) {
        Some((_, value, _)) if !value.is_empty() => value.clone(),
        Some((_, _, true)) => credential::load(&translate::secret_name(provider, secret_field))?.unwrap_or_default(),
        _ => String::new(),
    };
    let plain = |field: Field| get(field).map(|(_, value, _)| value.clone()).unwrap_or_default();
    Ok(Service {
        provider,
        endpoint: match provider {
            Provider::OpenAi => Some(plain(Field::Endpoint))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| translate::DEFAULT_ENDPOINT.into()),
            _ => String::new(),
        },
        model: Some(plain(Field::Model))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| translate::DEFAULT_MODEL.into()),
        app_id: plain(if provider == Provider::Free { Field::Email } else { Field::AppId }),
        secret,
    })
}

unsafe fn set_status(window: HWND, status: Status) {
    with_state(|state| state.status = status);
    // The status line changes the window height.
    place(window, null_mut());
}

unsafe fn test(window: HWND) {
    let Some((provider, values)) = form() else { return };
    if let Some(field) = missing(provider, &values) {
        set_status(window, Status::Error(format!("请先填写「{field}」。")));
        return;
    }
    let service = match service_from_form(provider, &values) {
        Ok(service) => service,
        Err(error) => return set_status(window, Status::Error(error)),
    };
    let id = with_state(|state| {
        state.test = state.test.wrapping_add(1);
        state.test
    })
    .unwrap_or(0);
    set_status(window, Status::Testing);
    let target = window as isize;
    std::thread::spawn(move || {
        let result = translate::translate(&service, TEST_TEXT, Language::Chinese);
        let result = Box::into_raw(Box::new(result));
        if PostMessageW(target as HWND, TEST_DONE, id as WPARAM, result as LPARAM) == 0 {
            drop(Box::from_raw(result));
        }
    });
}

unsafe fn save(window: HWND) {
    let Some((provider, values)) = form() else { return };
    if let Some(field) = missing(provider, &values) {
        set_status(window, Status::Error(format!("请先填写「{field}」。")));
        return;
    }
    let result = (|| {
        let mut config = Config::load()?;
        config.provider = Some(provider);
        for (field, value, _) in &values {
            if field.secret() {
                // An empty box keeps the saved key.
                if !value.is_empty() {
                    credential::save(&translate::secret_name(provider, *field), value)?;
                }
            } else {
                config.set(provider, *field, value);
            }
        }
        config.save()
    })();
    match result {
        Ok(()) => {
            pinned::translation_settings_changed();
            DestroyWindow(window);
        }
        Err(error) => set_status(window, Status::Error(error)),
    }
}

unsafe fn activate(window: HWND, target: Target) {
    match target {
        Target::Provider(index) => {
            let changed = with_state(|state| {
                let provider = PROVIDERS[index];
                let changed = state.provider != provider;
                state.provider = provider;
                state.status = Status::None;
                changed
            });
            if changed == Some(true) {
                place(window, null_mut());
                focus_first(window);
            }
        }
        Target::Close | Target::Cancel => {
            DestroyWindow(window);
        }
        Target::Test => test(window),
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
                    }
                    EN_CHANGE => {
                        // Typing a new key hides the "已保存" tag.
                        InvalidateRect(window, null(), 0);
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
                set_status(
                    window,
                    match result {
                        Ok(text) => Status::Success(format!("连接成功，测试译文：{text}")),
                        Err(error) => Status::Error(error),
                    },
                );
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
        "翻译服务",
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
        RECT { left: layout.provider_label.left + s(4), bottom: layout.provider_label.bottom - s(6), ..layout.provider_label },
        "服务商",
        fonts.small,
        palette.secondary,
        DT_LEFT | DT_BOTTOM | DT_SINGLELINE,
    );
    for (index, chip) in layout.chips.iter().enumerate() {
        let selected = PROVIDERS[index] == state.provider;
        let target = Target::Provider(index);
        if let Some(graphics) = Graphics::new(dc) {
            let fill = if selected { palette.accent_soft } else { face(target, palette.card) };
            graphics.fill_round(ui::to_f(*chip), radius, argb(fill, 255));
            let border = if selected { palette.accent } else { palette.key_border };
            graphics.stroke_round(ui::inset(ui::to_f(*chip), 0.5), radius, argb(border, 255), 1.0);
        }
        let font = if selected { fonts.heading } else { fonts.body };
        let color = if selected { palette.accent } else { palette.text };
        ui::text(dc, *chip, PROVIDERS[index].name(), font, color, DT_CENTER | single);
    }

    let inputs: Vec<&Input> = state.inputs.iter().filter(|input| input.provider == state.provider).collect();
    for (row, input) in layout.fields.iter().zip(&inputs) {
        ui::text(
            dc,
            RECT { right: row.left + s(FIELD_LABEL - 8), ..*row },
            input.field.label(state.provider),
            fonts.body,
            palette.secondary,
            DT_LEFT | single,
        );
        let field_box = layout.box_rect(*row);
        let focused = state.focused == Some(input.edit);
        if let Some(graphics) = Graphics::new(dc) {
            graphics.fill_round(ui::to_f(field_box), radius, argb(palette.card, 255));
            let (border, width) = if focused { (palette.accent, 1.5) } else { (palette.key_border, 1.0) };
            graphics.stroke_round(ui::inset(ui::to_f(field_box), width / 2.0), radius, argb(border, 255), width);
        }
        if input.saved && GetWindowTextLengthW(input.edit) == 0 {
            let tag = RECT {
                left: field_box.right - s(8 + SAVED_TAG),
                top: (field_box.top + field_box.bottom) / 2 - s(11),
                right: field_box.right - s(8),
                bottom: (field_box.top + field_box.bottom) / 2 + s(11),
            };
            if let Some(graphics) = Graphics::new(dc) {
                graphics.fill_round(ui::to_f(tag), s(11) as f32, argb(mix(palette.card, palette.success, 0.18), 255));
            }
            ui::text(dc, tag, "已保存", fonts.small, palette.success, DT_CENTER | single);
        }
    }

    ui::text(dc, layout.hint, state.provider.hint(), fonts.small, palette.secondary, DT_LEFT | DT_TOP | WRAP);
    ui::text(
        dc,
        layout.security,
        if layout.security.bottom > layout.security.top { SECURITY_NOTE } else { "" },
        fonts.small,
        palette.tertiary,
        DT_LEFT | DT_TOP | WRAP,
    );
    if let Some((text, good)) = status_text(&state.status) {
        let color = match state.status {
            Status::Testing => palette.secondary,
            _ if good => palette.success,
            _ => palette.error,
        };
        ui::text(dc, layout.status, &text, fonts.small, color, DT_LEFT | DT_TOP | WRAP);
    }

    ui::fill(
        dc,
        RECT { left: 1, top: layout.footer, right: layout.width - 1, bottom: layout.footer + 1 },
        palette.card_border,
    );
    let testing = matches!(state.status, Status::Testing);
    let test_face = if testing { palette.hover } else { face(Target::Test, palette.card) };
    ui::button(dc, layout.test, if testing { "测试中…" } else { "测试连接" }, fonts.body, palette, false, test_face, radius);
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

    #[test]
    fn required_fields_depend_on_the_provider() {
        let value = |field, text: &str, saved| (field, text.to_string(), saved);
        // OpenAI-compatible endpoints may run without a key (Ollama).
        assert_eq!(missing(Provider::OpenAi, &[value(Field::ApiKey, "", false)]), None);
        assert_eq!(missing(Provider::DeepL, &[value(Field::ApiKey, "", false)]), Some("API Key"));
        assert_eq!(missing(Provider::DeepL, &[value(Field::ApiKey, "", true)]), None, "a saved key counts");
        assert_eq!(
            missing(Provider::Youdao, &[value(Field::AppId, "id", false), value(Field::Secret, "", false)]),
            Some("应用密钥")
        );
    }

    /// Opens the real window (Session 0 is fine) and saves a picture of it,
    /// child edit boxes included. Run with `<test binary> translate_settings --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        unsafe {
            let _ = register();
            let folder = std::env::temp_dir().join("snapshot-preview");
            std::fs::create_dir_all(&folder).unwrap();
            for (index, provider) in PROVIDERS.into_iter().enumerate() {
                open(null_mut()).unwrap();
                let window = WINDOW.get();
                // A process started hidden ignores its first ShowWindow.
                ShowWindow(window, SW_SHOW);
                activate(window, Target::Provider(index));
                if provider == Provider::OpenAi {
                    let edit = STATE.with_borrow(|state| {
                        let inputs = &state.as_ref().unwrap().inputs;
                        inputs.iter().find(|input| input.provider == provider && input.field == Field::Endpoint).unwrap().edit
                    });
                    assert_eq!(text_of(edit), translate::DEFAULT_ENDPOINT, "addresses start filled in");
                }
                if provider == Provider::Baidu {
                    set_status(window, Status::Error("翻译失败（百度翻译）\n签名错误，请检查 APP ID 和密钥是否填对（错误码 54001）\n服务器返回：Invalid Sign".into()));
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
                // Then each visible edit box at its own spot.
                STATE.with_borrow(|state| {
                    for input in &state.as_ref().unwrap().inputs {
                        let mut rect = RECT::default();
                        GetWindowRect(input.edit, &mut rect);
                        if IsWindowVisible(input.edit) == 0 {
                            continue;
                        }
                        let mut origin = POINT { x: rect.left, y: rect.top };
                        ScreenToClient(window, &mut origin);
                        SetViewportOrgEx(surface.dc, origin.x, origin.y, null_mut());
                        SendMessageW(input.edit, WM_PRINT, surface.dc as WPARAM, (PRF_CLIENT | PRF_ERASEBKGND) as LPARAM);
                        SetViewportOrgEx(surface.dc, 0, 0, null_mut());
                    }
                });
                GdiFlush();
                let bounds = RECT { left: 0, top: 0, right: width, bottom: height };
                let mut image = crate::capture::Desktop { bounds, surface }.crop(bounds).unwrap();
                for pixel in image.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                image.save_png(&folder.join(format!("translate-settings-{index}.png"))).unwrap();
                DestroyWindow(window);
            }
        }
    }
}
