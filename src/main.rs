#![windows_subsystem = "windows"]

mod annotation;
mod app_icon;
mod annotation_options;
mod capture;
mod dialog;
mod hotkey;
mod icons;
mod mosaic;
mod overlay_ui;
mod panel_edges;
mod pinned;
mod scroll_capture;
mod scroll_input;
mod scroll_stitch;
mod selection;
mod selection_geometry;
mod settings;
mod startup;
mod targets;
mod text_mark;
mod text_render;
mod theme;
mod toolbar;
mod typography;
mod ui;

use std::{
    cell::Cell,
    io,
    mem::size_of,
    ptr::{null, null_mut},
    sync::OnceLock,
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
            Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT},
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_NOSOUND,
                NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_BALLOONUSERCLICK,
                NOTIFYICONDATAW,
            },
            WindowsAndMessaging::*,
        },
    },
};

const TRAY_MESSAGE: u32 = WM_APP + 1;
const MENU_CAPTURE: usize = 1;
const MENU_CONFIG: usize = 2;
const MENU_EXIT: usize = 3;
const MENU_CONFLICT: usize = 4;
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
thread_local! {
    static HOTKEY_ID: Cell<i32> = const { Cell::new(1) };
    static HOTKEY_REGISTERED: Cell<bool> = const { Cell::new(false) };
    static TRAY_ICONS: Cell<[HICON; 2]> = const { Cell::new([null_mut(); 2]) };
}

fn main() {
    if let Err(error) = unsafe { run() } {
        unsafe { show_error(null_mut(), &error) };
        std::process::exit(1);
    }
}

unsafe fn run() -> Result<(), String> {
    if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
        return Err(format!(
            "无法启用高 DPI 支持。\n{}",
            io::Error::last_os_error()
        ));
    }
    let first_run = hotkey::first_run();
    let preferences = hotkey::load()?;
    if first_run {
        // Start at login by default; the settings file marks the choice as made.
        if let Err(error) = startup::save_with(true, || hotkey::save(preferences)) {
            show_error(
                null_mut(),
                &format!("无法设置开机自动启动，可在托盘「设置」中重试。\n{error}"),
            );
        }
    }
    let bindings = preferences.bindings;
    hotkey::activate(bindings);
    pinned::set_opacity_step(preferences.opacity_step);
    theme::set(preferences.theme);
    let instance = GetModuleHandleW(null());
    selection::register(instance).map_err(|error| error.to_string())?;
    scroll_capture::register(instance).map_err(|error| error.to_string())?;
    pinned::register(instance).map_err(|error| error.to_string())?;
    settings::register(instance).map_err(|error| error.to_string())?;
    TASKBAR_CREATED.get_or_init(|| RegisterWindowMessageW(w!("TaskbarCreated")));
    let class = WNDCLASSW {
        lpfnWndProc: Some(tray_proc),
        hInstance: instance,
        lpszClassName: w!("Snapshot.Tray"),
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let window = CreateWindowExW(
        0,
        class.lpszClassName,
        w!("Snapshot"),
        WS_OVERLAPPED,
        0,
        0,
        0,
        0,
        null_mut(),
        null_mut(),
        instance,
        null(),
    );
    if window.is_null() {
        return Err(io::Error::last_os_error().to_string());
    }

    let capture = bindings.get(hotkey::Binding::Capture);
    let registered = RegisterHotKey(
        window,
        HOTKEY_ID.get(),
        capture.modifiers | MOD_NOREPEAT,
        capture.key,
    ) != 0;
    HOTKEY_REGISTERED.set(registered);
    if !tray_icon(window, NIM_ADD) {
        DestroyWindow(window);
        return Err("无法创建 Snapshot 托盘图标。".into());
    }
    // A conflict is not fatal: the tray shows a warning badge and stays usable.
    if !registered {
        notify_conflict(window);
    }

    let mut message = MSG::default();
    loop {
        match GetMessageW(&mut message, null_mut(), 0, 0) {
            -1 => {
                let error = io::Error::last_os_error();
                DestroyWindow(window);
                return Err(error.to_string());
            }
            0 => return Ok(()),
            _ => {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

unsafe extern "system" fn tray_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_HOTKEY if wparam == HOTKEY_ID.get() as usize => begin_capture(window),
        TRAY_MESSAGE => match lparam as u32 {
            WM_RBUTTONUP => tray_menu(window),
            WM_LBUTTONDBLCLK => begin_capture(window),
            NIN_BALLOONUSERCLICK => open_settings(window),
            _ => {}
        },
        WM_CLOSE => {
            DestroyWindow(window);
        }
        WM_DESTROY => {
            tray_icon(window, NIM_DELETE);
            if HOTKEY_REGISTERED.get() {
                UnregisterHotKey(window, HOTKEY_ID.get());
            }
            PostQuitMessage(0);
        }
        taskbar_created if TASKBAR_CREATED.get() == Some(&taskbar_created) => {
            tray_icon(window, NIM_ADD);
        }
        _ => return DefWindowProcW(window, message, wparam, lparam),
    }
    0
}

unsafe fn tray_icon(window: HWND, message: u32) -> bool {
    let registered = HOTKEY_REGISTERED.get();
    let mut icon = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: TRAY_MESSAGE,
        hIcon: tray_icon_handle(!registered),
        ..Default::default()
    };
    let capture = hotkey::current().get(hotkey::Binding::Capture).label();
    let label = if registered {
        format!("Snapshot · {capture}")
    } else {
        format!("Snapshot · {capture} 被其他程序占用\n双击截图，右键换快捷键")
    };
    copy_text(&mut icon.szTip, &label);
    Shell_NotifyIconW(message, &icon) != 0
}

/// The app icon, or the same icon with an amber warning badge.
unsafe fn tray_icon_handle(warning: bool) -> HICON {
    let mut icons = TRAY_ICONS.get();
    let index = usize::from(warning);
    if icons[index].is_null() {
        icons[index] = app_icon::create(GetSystemMetrics(SM_CXSMICON).max(16), warning);
        TRAY_ICONS.set(icons);
    }
    if icons[index].is_null() {
        LoadIconW(null_mut(), IDI_APPLICATION)
    } else {
        icons[index]
    }
}

/// A one-off, non-modal notification; clicking it opens the settings.
unsafe fn notify_conflict(window: HWND) {
    let mut icon = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_INFO,
        dwInfoFlags: NIIF_WARNING | NIIF_NOSOUND,
        ..Default::default()
    };
    let capture = hotkey::current().get(hotkey::Binding::Capture).label();
    copy_text(&mut icon.szInfoTitle, "截图快捷键被占用");
    copy_text(
        &mut icon.szInfo,
        &format!("{capture} 已被其他程序占用。点这里换一个快捷键，或者双击托盘图标截图。"),
    );
    Shell_NotifyIconW(NIM_MODIFY, &icon);
}

fn copy_text(target: &mut [u16], text: &str) {
    let limit = target.len() - 1;
    for (unit, value) in target.iter_mut().take(limit).zip(text.encode_utf16()) {
        *unit = value;
    }
}

/// The live capture shortcut, if Snapshot currently holds it.
pub fn active_hotkey() -> Option<hotkey::Shortcut> {
    HOTKEY_REGISTERED
        .get()
        .then(|| hotkey::current().get(hotkey::Binding::Capture))
}

/// Tries again to take a shortcut that another program held earlier.
pub unsafe fn retry_hotkey(window: HWND) {
    if HOTKEY_REGISTERED.get() {
        return;
    }
    let capture = hotkey::current().get(hotkey::Binding::Capture);
    if RegisterHotKey(window, HOTKEY_ID.get(), capture.modifiers | MOD_NOREPEAT, capture.key) != 0 {
        HOTKEY_REGISTERED.set(true);
        tray_icon(window, NIM_MODIFY);
    }
}

unsafe fn tray_menu(window: HWND) {
    retry_hotkey(window);
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    if !HOTKEY_REGISTERED.get() {
        let conflict = wide(&format!(
            "⚠ {} 被占用，换一个快捷键…",
            hotkey::current().get(hotkey::Binding::Capture).label()
        ));
        AppendMenuW(menu, MF_STRING, MENU_CONFLICT, conflict.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
    }
    let capture = wide(&format!(
        "截图 ({})",
        hotkey::current().get(hotkey::Binding::Capture).label()
    ));
    AppendMenuW(menu, MF_STRING, MENU_CAPTURE, capture.as_ptr());
    AppendMenuW(menu, MF_STRING, MENU_CONFIG, w!("设置"));
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("退出"));
    let mut cursor = POINT::default();
    GetCursorPos(&mut cursor);
    SetForegroundWindow(window);
    let command = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
        cursor.x,
        cursor.y,
        0,
        window,
        null(),
    );
    PostMessageW(window, WM_NULL, 0, 0);
    DestroyMenu(menu);
    match command as usize {
        MENU_CAPTURE => begin_capture(window),
        MENU_CONFIG | MENU_CONFLICT => open_settings(window),
        MENU_EXIT => {
            DestroyWindow(window);
        }
        _ => {}
    }
}

unsafe fn open_settings(window: HWND) {
    if let Err(error) = settings::open(window, GetModuleHandleW(null())) {
        show_error(window, &format!("无法打开设置。\n{error}"));
    }
}

unsafe fn begin_capture(window: HWND) {
    if settings::is_open() {
        return;
    }
    let instance: HINSTANCE = GetModuleHandleW(null());
    if let Err(error) = selection::open(instance) {
        show_error(window, &format!("无法截取桌面。\n{error}"));
    }
}

unsafe fn apply_preferences(
    window: HWND,
    preferences: hotkey::Preferences,
    start_at_login: bool,
) -> Result<(), String> {
    let bindings = preferences.bindings.validate()?;
    let capture = bindings.get(hotkey::Binding::Capture);
    let previous = HOTKEY_ID.get();
    let was_registered = HOTKEY_REGISTERED.get();
    let changed =
        !was_registered || capture != hotkey::current().get(hotkey::Binding::Capture);
    let next = 3 - previous;
    // An occupied shortcut is still saved; the tray badge reports the conflict.
    let registered = !changed
        || RegisterHotKey(window, next, capture.modifiers | MOD_NOREPEAT, capture.key) != 0;
    if let Err(error) = startup::save_with(start_at_login, || hotkey::save(preferences)) {
        if changed && registered {
            UnregisterHotKey(window, next);
        }
        return Err(error);
    }
    if changed {
        if was_registered {
            UnregisterHotKey(window, previous);
        }
        HOTKEY_ID.set(next);
        HOTKEY_REGISTERED.set(registered);
    }
    hotkey::activate(bindings);
    pinned::set_opacity_step(preferences.opacity_step);
    theme::set(preferences.theme);
    tray_icon(window, NIM_MODIFY);
    Ok(())
}

/// Errors use the themed dialog; the first line of `message` is its title.
unsafe fn show_error(window: HWND, message: &str) {
    dialog::show(window, dialog::Kind::Error, message);
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
