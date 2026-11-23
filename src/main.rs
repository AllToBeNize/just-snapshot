#![windows_subsystem = "windows"]

mod annotation;
mod app_icon;
mod annotation_options;
mod capture;
mod clipboard_data;
mod codes;
mod credential;
mod dialog;
mod digest;
mod gdiplus;
mod gif_export;
mod global_hotkey;
mod hotkey;
mod http;
mod icons;
mod input_overlay;
mod mosaic;
mod ocr;
mod output;
mod output_settings;
mod panel_options;
mod recorder;
mod recording_ui;
mod overlay_ui;
mod panel_edges;
mod pin_text;
mod pin_translate;
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
mod translate;
mod translate_overlay;
mod translate_settings;
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
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_NOSOUND,
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
const MENU_WINDOW: usize = 5;
const MENU_LAST: usize = 6;
const MENU_PINS_SHOW: usize = 7;
const MENU_PINS_HIDE: usize = 8;
const MENU_PINS_CLOSE: usize = 9;
const MENU_RECORD: usize = 10;
const MENU_SHOW_KEYS: usize = 11;
const MENU_SHOW_PAD: usize = 12;
const MENU_RECORD_SCREEN: usize = 13;
/// Menu ids for "show group N" start here.
const MENU_GROUP: usize = 100;
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
thread_local! {
    static TRAY_ICONS: Cell<[HICON; 2]> = const { Cell::new([null_mut(); 2]) };
    /// The last balloon was the shortcut conflict, so clicking it opens the settings.
    static CONFLICT_BALLOON: Cell<bool> = const { Cell::new(false) };
    /// The tray window, which owns the global shortcuts.
    static TRAY: Cell<HWND> = const { Cell::new(null_mut()) };
    /// The preferences in effect, to re-register the shortcuts when recording starts or ends.
    static PREFERENCES: Cell<Option<hotkey::Preferences>> = const { Cell::new(None) };
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
    } else {
        let _ = startup::follow_executable();
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
    translate_settings::register().map_err(|error| error.to_string())?;
    output_settings::register().map_err(|error| error.to_string())?;
    input_overlay::register(instance).map_err(|error| error.to_string())?;
    recording_ui::register(instance).map_err(|error| error.to_string())?;
    // A panel that cannot be created only costs that panel, not the app.
    let _ = input_overlay::start(instance);
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
        w!("just-snapshot"),
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

    TRAY.set(window);
    PREFERENCES.set(Some(preferences));
    let conflicts = global_hotkey::set(window, &preferences.globals(false));
    if !tray_icon(window, NIM_ADD) {
        DestroyWindow(window);
        return Err("无法创建 just-snapshot 托盘图标。".into());
    }
    // A conflict is not fatal: the tray shows a warning badge and stays usable.
    if !conflicts.is_empty() {
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
            _ if translate_settings::dialog_message(&message) => {}
            _ if output_settings::dialog_message(&message) => {}
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
        WM_HOTKEY => match global_hotkey::action(wparam) {
            Some(hotkey::Global::Capture) => begin_capture(window),
            Some(hotkey::Global::Window) => capture_window(window),
            Some(hotkey::Global::Last) => restore_last(window),
            Some(hotkey::Global::Record) => toggle_record(window),
            Some(hotkey::Global::RecordPause) => recording_ui::toggle_pause(),
            Some(hotkey::Global::RecordScreen) => toggle_screen_record(window),
            Some(hotkey::Global::ShowKeys) => input_overlay::toggle(input_overlay::Panel::Keys),
            Some(hotkey::Global::ShowPad) => input_overlay::toggle(input_overlay::Panel::Pad),
            None => {}
        },
        TRAY_MESSAGE => match lparam as u32 {
            WM_RBUTTONUP => tray_menu(window),
            WM_LBUTTONDBLCLK => begin_capture(window),
            NIN_BALLOONUSERCLICK if CONFLICT_BALLOON.get() => open_settings(window),
            _ => {}
        },
        WM_CLOSE => {
            DestroyWindow(window);
        }
        WM_DESTROY => {
            tray_icon(window, NIM_DELETE);
            global_hotkey::unregister_all(window);
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
    let conflicts = conflict_label();
    let mut icon = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: TRAY_MESSAGE,
        hIcon: tray_icon_handle(conflicts.is_some()),
        ..Default::default()
    };
    let label = match conflicts {
        None => format!("just-snapshot · {}", global_hotkey::label()),
        Some(taken) => format!("just-snapshot · {taken} 被其他程序占用\n双击截图，右键换快捷键"),
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
    let taken = conflict_label().unwrap_or_default();
    CONFLICT_BALLOON.set(true);
    copy_text(&mut icon.szInfoTitle, "快捷键被占用");
    copy_text(
        &mut icon.szInfo,
        &format!("{taken} 已被其他程序占用。点这里换一个快捷键，或者双击托盘图标截图。"),
    );
    Shell_NotifyIconW(NIM_MODIFY, &icon);
}

fn copy_text(target: &mut [u16], text: &str) {
    let limit = target.len() - 1;
    for (unit, value) in target.iter_mut().take(limit).zip(text.encode_utf16()) {
        *unit = value;
    }
}

/// "Ctrl+Alt+A / F1" for the capture shortcuts another program holds.
fn conflict_label() -> Option<String> {
    let taken = global_hotkey::conflicts();
    (!taken.is_empty()).then(|| {
        taken
            .iter()
            .map(|shortcut| shortcut.label())
            .collect::<Vec<_>>()
            .join(" / ")
    })
}

/// Tries again to take shortcuts that another program held earlier.
pub unsafe fn retry_hotkey(window: HWND) {
    if global_hotkey::retry(window) {
        tray_icon(window, NIM_MODIFY);
    }
}

unsafe fn tray_menu(window: HWND) {
    retry_hotkey(window);
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    if let Some(taken) = conflict_label() {
        let conflict = wide(&format!("⚠ {taken} 被占用，换一个快捷键…"));
        AppendMenuW(menu, MF_STRING, MENU_CONFLICT, conflict.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
    }
    let capture = wide(&format!("截图\t{}", global_hotkey::label()));
    AppendMenuW(menu, MF_STRING, MENU_CAPTURE, capture.as_ptr());
    let shortcut = |action| global_hotkey::shortcut(action).map(|key| format!("\t{}", key.label())).unwrap_or_default();
    let window_item = wide(&format!("截取当前窗口{}", shortcut(hotkey::Global::Window)));
    AppendMenuW(menu, MF_STRING, MENU_WINDOW, window_item.as_ptr());
    let last_item = wide(&format!("找回上次截图{}", shortcut(hotkey::Global::Last)));
    let last_flags = if selection::has_last() { MF_STRING } else { MF_STRING | MF_GRAYED };
    AppendMenuW(menu, last_flags, MENU_LAST, last_item.as_ptr());
    let recording = recording_ui::is_recording();
    let record_label = if recording { "停止录像" } else { "区域录像" };
    let record_item = wide(&format!("{record_label}{}", shortcut(hotkey::Global::Record)));
    // While the file is being saved there is nothing to start or stop.
    let record_flags = if recording_ui::is_busy() && !recording { MF_STRING | MF_GRAYED } else { MF_STRING };
    AppendMenuW(menu, record_flags, MENU_RECORD, record_item.as_ptr());
    if !recording {
        let screen_item = wide(&format!("全屏 / 游戏录像{}", shortcut(hotkey::Global::RecordScreen)));
        AppendMenuW(menu, record_flags, MENU_RECORD_SCREEN, screen_item.as_ptr());
    }
    let overlay = CreatePopupMenu();
    if !overlay.is_null() {
        let check = |panel| if input_overlay::is_visible(panel) { MF_CHECKED } else { 0 };
        let keys = wide(&format!("键鼠{}", shortcut(hotkey::Global::ShowKeys)));
        let pad = wide(&format!("手柄{}", shortcut(hotkey::Global::ShowPad)));
        AppendMenuW(overlay, MF_STRING | check(input_overlay::Panel::Keys), MENU_SHOW_KEYS, keys.as_ptr());
        AppendMenuW(overlay, MF_STRING | check(input_overlay::Panel::Pad), MENU_SHOW_PAD, pad.as_ptr());
        AppendMenuW(menu, MF_POPUP, overlay as usize, w!("按键显示"));
    }
    let pins = pins_menu();
    if !pins.is_null() {
        AppendMenuW(menu, MF_POPUP, pins as usize, w!("贴图"));
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
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
        MENU_WINDOW => capture_window(window),
        MENU_LAST => restore_last(window),
        MENU_RECORD => toggle_record(window),
        MENU_RECORD_SCREEN => toggle_screen_record(window),
        MENU_SHOW_KEYS => input_overlay::toggle(input_overlay::Panel::Keys),
        MENU_SHOW_PAD => input_overlay::toggle(input_overlay::Panel::Pad),
        MENU_PINS_SHOW => pinned::set_all_hidden(false),
        MENU_PINS_HIDE => pinned::set_all_hidden(true),
        MENU_PINS_CLOSE => pinned::close_all(),
        id if id > MENU_GROUP => pinned::toggle_group((id - MENU_GROUP) as u32),
        MENU_CONFIG | MENU_CONFLICT => open_settings(window),
        MENU_EXIT => {
            DestroyWindow(window);
        }
        _ => {}
    }
}

/// The "贴图" submenu: show or hide all pins, switch groups, close all.
/// Grayed out while there are no pins.
unsafe fn pins_menu() -> HMENU {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return menu;
    }
    let groups = pinned::groups();
    let gray = |disabled: bool| if disabled { MF_GRAYED } else { 0 };
    let any_hidden = groups.iter().any(|&(.., shown)| !shown);
    let any_shown = groups.iter().any(|&(.., shown)| shown);
    AppendMenuW(menu, MF_STRING | gray(!any_hidden), MENU_PINS_SHOW, w!("显示全部贴图"));
    AppendMenuW(menu, MF_STRING | gray(!any_shown), MENU_PINS_HIDE, w!("隐藏全部贴图"));
    if groups.len() > 1 || groups.first().is_some_and(|&(group, ..)| group != 1) {
        // One check item per group: shown groups are checked; clicking flips it.
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        for (group, count, shown) in groups.iter().copied() {
            let label = wide(&format!("组 {group}（{count} 张）"));
            let checked = if shown { MF_CHECKED } else { 0 };
            AppendMenuW(menu, MF_STRING | checked, MENU_GROUP + group as usize, label.as_ptr());
        }
    }
    let none = gray(groups.is_empty());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING | none, MENU_PINS_CLOSE, w!("关闭所有贴图"));
    menu
}

unsafe fn open_settings(window: HWND) {
    if let Err(error) = settings::open(window, GetModuleHandleW(null())) {
        show_error(window, &format!("无法打开设置。\n{error}"));
    }
}

unsafe fn begin_capture(window: HWND) {
    // Only while a shortcut is being recorded; with settings open you can still capture them.
    if settings::is_recording() {
        return;
    }
    let instance: HINSTANCE = GetModuleHandleW(null());
    if let Err(error) = selection::open(instance) {
        show_error(window, &format!("无法截取桌面。\n{error}"));
    }
}

/// Opens the capture with the active window already selected.
unsafe fn capture_window(window: HWND) {
    // Only while a shortcut is being recorded; with settings open you can still capture them.
    if settings::is_recording() {
        return;
    }
    let instance: HINSTANCE = GetModuleHandleW(null());
    let result = match targets::active_window() {
        Some(rect) => selection::reopen(instance, rect),
        None => selection::open(instance),
    };
    if let Err(error) = result {
        show_error(window, &format!("无法截取窗口。\n{error}"));
    }
}

/// One shortcut for both: stops and saves a running recording, otherwise
/// opens the area picker for a new one. While a file is still being saved, does nothing.
unsafe fn toggle_record(window: HWND) {
    if recording_ui::is_recording() {
        recording_ui::stop(false);
        return;
    }
    if settings::is_recording() || recording_ui::is_busy() {
        return;
    }
    if let Err(error) = selection::open_record(GetModuleHandleW(null())) {
        show_error(window, &format!("无法开始录像。\n{error}"));
    }
}

/// Like a game recorder: starts recording the active window's whole screen,
/// or stops whatever recording is running.
unsafe fn toggle_screen_record(window: HWND) {
    if recording_ui::is_recording() {
        recording_ui::stop(false);
    } else if !settings::is_recording() && !recording_ui::is_busy() {
        recording_ui::start_screen(window);
    }
}

/// Brings back the last capture: the same frozen screen, selection and annotations.
unsafe fn restore_last(window: HWND) {
    // Only while a shortcut is being recorded; with settings open you can still capture them.
    if settings::is_recording() {
        return;
    }
    match selection::restore_last(GetModuleHandleW(null())) {
        Ok(true) => {}
        Ok(false) => notify(window, "没有可以找回的截图", "截一张图之后，就能用这个功能找回上一次的选区和标注。"),
        Err(error) => show_error(window, &format!("无法找回上次截图。\n{error}")),
    }
}

/// A non-modal tray notification.
unsafe fn notify(window: HWND, title: &str, text: &str) {
    let mut icon = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_INFO,
        dwInfoFlags: NIIF_INFO | NIIF_NOSOUND,
        ..Default::default()
    };
    CONFLICT_BALLOON.set(false);
    copy_text(&mut icon.szInfoTitle, title);
    copy_text(&mut icon.szInfo, text);
    Shell_NotifyIconW(NIM_MODIFY, &icon);
}

unsafe fn apply_preferences(
    window: HWND,
    preferences: hotkey::Preferences,
    start_at_login: bool,
) -> Result<(), String> {
    let bindings = preferences.bindings.validate()?;
    preferences.validate()?;
    startup::save_with(start_at_login, || hotkey::save(preferences))?;
    // An occupied shortcut is still saved; the tray badge reports the conflict.
    PREFERENCES.set(Some(preferences));
    global_hotkey::set(window, &preferences.globals(recording_ui::is_recording()));
    hotkey::activate(bindings);
    pinned::set_opacity_step(preferences.opacity_step);
    theme::set(preferences.theme);
    tray_icon(window, NIM_MODIFY);
    Ok(())
}

/// Takes the pause shortcut while a recording runs and gives it back after,
/// so programs that use the same keys (IDEs, for one) keep them otherwise.
pub unsafe fn refresh_shortcuts() {
    let (window, Some(preferences)) = (TRAY.get(), PREFERENCES.get()) else { return };
    if window.is_null() {
        return;
    }
    global_hotkey::set(window, &preferences.globals(recording_ui::is_recording()));
    tray_icon(window, NIM_MODIFY);
}

/// Errors use the themed dialog; the first line of `message` is its title.
unsafe fn show_error(window: HWND, message: &str) {
    dialog::show(window, dialog::Kind::Error, message);
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
