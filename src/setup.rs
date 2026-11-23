//! Installing and uninstalling. A release ships the app a second time as
//! `just-snapshot-v<version>-setup.exe`. Started under a name containing
//! "setup" (or with `--setup`), the executable shows the install window
//! instead of starting the app and copies itself to
//! `<chosen folder>\just-snapshot\just-snapshot.exe`. It also adds shortcuts
//! and an entry under Settings › Apps, whose command `--uninstall` opens the
//! same window to remove it.
//!
//! The user picks a parent folder (D:\ is enough) and the program folder is
//! created inside it under an editable name, so nobody has to invent one.
//! Picking a folder that already has that name (reinstalling into
//! D:\just-snapshot) installs into it rather than one level deeper.
//!
//! Uninstalling removes only what installing wrote. The `Videos` folder that
//! recordings default to (next to the executable) stays, and so does the
//! install folder while anything else is in it.

use std::{
    cell::{Cell, RefCell},
    env, fs, io,
    mem::{size_of, size_of_val},
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
    ptr::{null, null_mut},
    sync::Mutex,
    thread,
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::*,
        System::{
            LibraryLoader::GetModuleHandleW,
            Registry::{
                RegDeleteTreeW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RRF_RT_REG_SZ,
            },
            SystemInformation::GetLocalTime,
            Threading::{GetCurrentProcessId, OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        },
        UI::{
            Controls::WM_MOUSELEAVE,
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{
                EnableWindow, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
            },
            WindowsAndMessaging::*,
        },
    },
};

use crate::{
    gdiplus::Graphics,
    icons::{self, Icon},
    migrate::NAME,
    selection_geometry::{contains, mouse_point},
    theme::{self, argb, mix},
    ui::{self, Badge, Fonts},
    update::{REPOSITORY, VERSION},
    wide,
};

pub const EXE: &str = "just-snapshot.exe";
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\just-snapshot";
const SHORTCUT: &str = "just-snapshot.lnk";
/// Written next to the executable; removed again by uninstalling.
const NOTICES: [(&str, &str); 3] = [
    ("LICENSE.txt", include_str!("../LICENSE")),
    (r"licenses\lucide.txt", include_str!("../licenses/lucide.txt")),
    (r"licenses\rxing.txt", include_str!("../licenses/rxing.txt")),
];
/// How long a running copy gets to leave the tray before installing goes on.
const STOP_WAIT_MS: u32 = 5_000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Install,
    Uninstall,
}

/// Install with `--setup` or under a name like `just-snapshot-setup (1).exe`;
/// uninstall with `--uninstall`. `None` starts the app.
pub fn mode(args: &[String]) -> Option<Mode> {
    match args.get(1).map(String::as_str) {
        Some("--uninstall") => return Some(Mode::Uninstall),
        Some("--setup") => return Some(Mode::Install),
        _ => {}
    }
    let exe = env::current_exe().ok()?;
    let stem = exe.file_stem()?.to_string_lossy().to_lowercase();
    stem.contains("setup").then_some(Mode::Install)
}

// ---------------------------------------------------------------------------
// Where things go.

/// The program folder: `name` inside `parent`, or `parent` itself when it
/// already carries that name.
pub fn install_dir(parent: &str, name: &str) -> Result<PathBuf, String> {
    let name = name.trim().trim_end_matches(['.', ' ']);
    if name.is_empty() {
        return Err("文件夹名不能为空。".into());
    }
    if name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control()) {
        return Err("文件夹名里不能有 \\ / : * ? \" < > | 这些字符。".into());
    }
    let parent = parent.trim().trim_matches('"').trim();
    if parent.is_empty() {
        return Err("请选择安装位置。".into());
    }
    // To Windows "D:" is the drive's current folder; the user means its root.
    let parent = if parent.len() == 2 && parent.ends_with(':') { format!("{parent}\\") } else { parent.to_string() };
    let parent = PathBuf::from(parent);
    if !parent.is_absolute() {
        return Err(format!("安装位置要写完整路径，例如 D:\\ 或 D:\\Tools，而不是「{}」。", parent.display()));
    }
    if parent.file_name().is_some_and(|last| last.to_string_lossy().eq_ignore_ascii_case(name)) {
        return Ok(parent);
    }
    Ok(parent.join(name))
}

/// Everything outside the program folder that installing touches. Tests
/// point it at temporary folders and a test registry key.
#[derive(Clone, Debug)]
pub struct Places {
    pub start_menu: Option<PathBuf>,
    pub desktop: Option<PathBuf>,
    pub settings: Option<PathBuf>,
    pub uninstall_key: String,
}

impl Places {
    pub fn system() -> Self {
        use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Programs};
        Self {
            start_menu: known_folder(&FOLDERID_Programs),
            desktop: known_folder(&FOLDERID_Desktop),
            settings: crate::migrate::app_data().ok(),
            uninstall_key: UNINSTALL_KEY.into(),
        }
    }
}

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT},
    };
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as _));
        text.map(PathBuf::from)
    }
}

/// `%LOCALAPPDATA%\Programs`: per user, so no administrator prompt, and the
/// in-app update can replace the executable.
fn default_parent() -> String {
    env::var_os("LOCALAPPDATA")
        .map(|folder| PathBuf::from(folder).join("Programs").to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Where the form starts: the existing installation, else the default.
fn initial_location(places: &Places) -> (String, String) {
    if let Some(dir) = registered_dir(places) {
        if let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) {
            return (parent.to_string_lossy().into_owned(), name.to_string_lossy().into_owned());
        }
    }
    (default_parent(), NAME.into())
}

// ---------------------------------------------------------------------------
// Installing.

#[derive(Clone, Copy, Debug)]
pub struct Choices {
    pub start_menu: bool,
    pub desktop: bool,
    pub launch: bool,
}

/// Copies `source` into `dir` as just-snapshot.exe with the license notices,
/// adds the chosen shortcuts and the Settings › Apps entry. Returns the
/// installed executable.
pub fn install(source: &Path, dir: &Path, choices: Choices, places: &Places) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|error| {
        let hint = if error.kind() == io::ErrorKind::PermissionDenied {
            "\n这个位置需要管理员权限，换一个位置吧，例如 D:\\ 或默认位置。"
        } else {
            ""
        };
        format!("无法创建文件夹 {}。\n{error}{hint}", dir.display())
    })?;
    let target = dir.join(EXE);
    if !same_file(source, &target) {
        place_file(source, &target)?;
    }
    for (name, text) in NOTICES {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&path, text).map_err(|error| format!("无法写入 {}。\n{error}", path.display()))?;
    }
    for (folder, wanted, place) in [
        (&places.start_menu, choices.start_menu, "开始菜单"),
        (&places.desktop, choices.desktop, "桌面"),
    ] {
        if let (Some(folder), true) = (folder, wanted) {
            create_shortcut(&folder.join(SHORTCUT), &target)
                .map_err(|error| format!("无法在{place}创建快捷方式。\n{error}"))?;
        }
    }
    register(places, dir, &target)?;
    Ok(target)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Puts a copy of `source` at `target`. An existing executable may still be
/// running (Windows allows renaming it, not overwriting it), so it moves
/// aside to `.old` first; the app deletes that file on its next start.
fn place_file(source: &Path, target: &Path) -> Result<(), String> {
    let staged = with_suffix(target, ".new");
    fs::copy(source, &staged).map_err(|error| format!("无法复制程序到 {}。\n{error}", target.display()))?;
    let old = with_suffix(target, ".old");
    let moved_aside = target.exists();
    if moved_aside {
        if old.exists() {
            fs::remove_file(&old).map_err(|error| {
                let _ = fs::remove_file(&staged);
                format!("无法删除上次留下的 {}。\n{error}", old.display())
            })?;
        }
        fs::rename(target, &old).map_err(|error| {
            let _ = fs::remove_file(&staged);
            format!("无法替换已安装的 {}。\n{error}", target.display())
        })?;
    }
    if let Err(error) = fs::rename(&staged, target) {
        let _ = fs::remove_file(&staged);
        if moved_aside {
            let _ = fs::rename(&old, target);
        }
        return Err(format!("无法放入 {}。\n{error}", target.display()));
    }
    if moved_aside {
        let _ = fs::remove_file(&old);
    }
    Ok(())
}

fn create_shortcut(link: &Path, target: &Path) -> Result<(), String> {
    use windows::{
        core::{Interface, PCWSTR},
        Win32::{
            System::{
                Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED},
            },
            UI::Shell::{IShellLinkW, ShellLink},
        },
    };
    let text = |path: &Path| path.as_os_str().encode_wide().chain([0]).collect::<Vec<u16>>();
    let (target_text, folder_text, link_text) = (
        text(target),
        text(target.parent().unwrap_or(target)),
        text(link),
    );
    let description = wide("截图、贴图、录屏");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let shell_link: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|error| error.message())?;
        shell_link.SetPath(PCWSTR(target_text.as_ptr())).map_err(|error| error.message())?;
        shell_link.SetWorkingDirectory(PCWSTR(folder_text.as_ptr())).map_err(|error| error.message())?;
        shell_link.SetIconLocation(PCWSTR(target_text.as_ptr()), 0).map_err(|error| error.message())?;
        shell_link.SetDescription(PCWSTR(description.as_ptr())).map_err(|error| error.message())?;
        let file: IPersistFile = shell_link.cast().map_err(|error| error.message())?;
        file.Save(PCWSTR(link_text.as_ptr()), true).map_err(|error| error.message())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The Settings › Apps entry.

fn set_string(key: &str, name: &str, value: &str) -> Result<(), String> {
    let (key, name, value) = (wide(key), wide(name), wide(value));
    let status = unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, value.as_ptr().cast(), size_of_val(value.as_slice()) as u32)
    };
    registry_result(status)
}

fn set_number(key: &str, name: &str, value: u32) -> Result<(), String> {
    let (key, name) = (wide(key), wide(name));
    let status = unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_DWORD, (&value as *const u32).cast(), size_of::<u32>() as u32)
    };
    registry_result(status)
}

fn registry_result(status: u32) -> Result<(), String> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("无法写入「设置 › 应用」里的卸载信息。\n{}", io::Error::from_raw_os_error(status as i32)))
    }
}

fn get_string(key: &str, name: &str) -> Option<String> {
    let (key, name) = (wide(key), wide(name));
    let mut bytes = 0u32;
    unsafe {
        let status = RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut bytes);
        if status != ERROR_SUCCESS {
            return None;
        }
        let mut buffer = vec![0u16; bytes as usize / 2 + 1];
        let mut bytes = size_of_val(buffer.as_slice()) as u32;
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        );
        if status != ERROR_SUCCESS {
            return None;
        }
        let length = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..length]))
    }
}

fn register(places: &Places, dir: &Path, exe: &Path) -> Result<(), String> {
    let key = places.uninstall_key.as_str();
    let exe_text = exe.to_string_lossy();
    let mut time = windows_sys::Win32::Foundation::SYSTEMTIME::default();
    unsafe { GetLocalTime(&mut time) };
    let size_kb = fs::metadata(exe).map_or(0, |metadata| (metadata.len() / 1024) as u32);
    set_string(key, "DisplayName", NAME)?;
    set_string(key, "DisplayVersion", VERSION)?;
    set_string(key, "Publisher", "AllToBeNize")?;
    set_string(key, "DisplayIcon", &format!("{exe_text},0"))?;
    set_string(key, "InstallLocation", &dir.to_string_lossy())?;
    set_string(key, "UninstallString", &format!("\"{exe_text}\" --uninstall"))?;
    set_string(key, "URLInfoAbout", REPOSITORY)?;
    set_string(key, "InstallDate", &format!("{:04}{:02}{:02}", time.wYear, time.wMonth, time.wDay))?;
    set_number(key, "EstimatedSize", size_kb)?;
    set_number(key, "NoModify", 1)?;
    set_number(key, "NoRepair", 1)
}

fn registered_dir(places: &Places) -> Option<PathBuf> {
    get_string(&places.uninstall_key, "InstallLocation")
        .filter(|dir| !dir.trim().is_empty())
        .map(PathBuf::from)
}

fn unregister(places: &Places) -> Result<(), String> {
    let key = wide(&places.uninstall_key);
    match unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) } {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => {
            // RegDeleteTreeW empties the key; this removes the key itself.
            let status = unsafe { windows_sys::Win32::System::Registry::RegDeleteKeyW(HKEY_CURRENT_USER, key.as_ptr()) };
            match status {
                ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
                status => Err(format!("无法删除卸载信息。\n{}", io::Error::from_raw_os_error(status as i32))),
            }
        }
        status => Err(format!("无法删除卸载信息。\n{}", io::Error::from_raw_os_error(status as i32))),
    }
}

/// Keeps the version shown under Settings › Apps current after an in-app
/// update. Only for the registered installation.
pub fn refresh_registration() {
    let places = Places { start_menu: None, desktop: None, settings: None, uninstall_key: UNINSTALL_KEY.into() };
    let (Some(dir), Ok(exe)) = (registered_dir(&places), env::current_exe()) else { return };
    if exe.parent().is_some_and(|parent| same_file(parent, &dir))
        && get_string(UNINSTALL_KEY, "DisplayVersion").as_deref() != Some(VERSION)
    {
        let _ = set_string(UNINSTALL_KEY, "DisplayVersion", VERSION);
    }
}

// ---------------------------------------------------------------------------
// Uninstalling.

pub struct Removed {
    /// Folders left in place because they hold the user's files.
    pub kept: Vec<PathBuf>,
    /// The running executable, deleted after this process exits.
    pub pending: Option<PathBuf>,
}

/// Removes what installing wrote into `dir` and elsewhere. `running` is this
/// process's executable: if it lives in `dir` it cannot delete itself and
/// comes back as `pending`. With `settings`, the settings folder and the
/// translation keys go too (never the screenshots or recordings).
pub fn uninstall(dir: &Path, running: &Path, settings: bool, places: &Places) -> Result<Removed, String> {
    let mut failures = Vec::new();
    for folder in [&places.start_menu, &places.desktop].into_iter().flatten() {
        let link = folder.join(SHORTCUT);
        if link.exists() {
            if let Err(error) = fs::remove_file(&link) {
                failures.push(format!("{}：{error}", link.display()));
            }
        }
    }
    if let Err(error) = crate::startup::remove_if_inside(dir) {
        failures.push(error);
    }
    if settings {
        if let Some(folder) = places.settings.as_ref().filter(|folder| folder.exists()) {
            if let Err(error) = fs::remove_dir_all(folder) {
                failures.push(format!("{}：{error}", folder.display()));
            }
        }
        failures.extend(crate::credential::delete_all("translate"));
    }
    let exe = dir.join(EXE);
    let pending = same_file(&exe, running).then(|| exe.clone());
    let mut files: Vec<PathBuf> = NOTICES.iter().map(|(name, _)| dir.join(name)).collect();
    files.extend([with_suffix(&exe, ".old"), with_suffix(&exe, ".new")]);
    if pending.is_none() {
        files.push(exe);
    }
    for file in files.iter().filter(|file| file.exists()) {
        if let Err(error) = fs::remove_file(file) {
            failures.push(format!("{}：{error}", file.display()));
        }
    }
    let _ = fs::remove_dir(dir.join("licenses"));
    if pending.is_none() {
        let _ = fs::remove_dir(dir);
    }
    unregister(places)?;
    if !failures.is_empty() {
        return Err(format!("有些文件没能删除，可以手动删掉：\n{}", failures.join("\n")));
    }
    let kept = ["Videos"].iter().map(|name| dir.join(name)).filter(|folder| folder.is_dir()).collect();
    Ok(Removed { kept, pending })
}

/// Deletes `exe` and then its folder (only if empty) once this process has
/// exited, through a hidden cmd that waits a moment first.
fn delete_after_exit(exe: &Path) {
    let Some(dir) = exe.parent() else { return };
    let command = format!(
        "/d /c ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & rd \"{}\"",
        exe.display(),
        dir.display()
    );
    let _ = Command::new("cmd.exe").raw_arg(command).creation_flags(CREATE_NO_WINDOW).spawn();
}

/// Asks every running just-snapshot to leave the tray and waits for it, so
/// the new executable can take the shortcuts. Returns whether one ran.
fn stop_running() -> bool {
    let mut stopped = false;
    let own = unsafe { GetCurrentProcessId() };
    // Each closed instance takes its tray window with it; give up after a few.
    for _ in 0..8 {
        let window = unsafe { FindWindowW(w!("Snapshot.Tray"), null()) };
        if window.is_null() {
            break;
        }
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
        if pid == own {
            break;
        }
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            PostMessageW(window, WM_CLOSE, 0, 0);
            if !process.is_null() {
                WaitForSingleObject(process, STOP_WAIT_MS);
                CloseHandle(process);
            }
        }
        stopped = true;
    }
    stopped
}

// ---------------------------------------------------------------------------
// The window.

const CLASS: *const u16 = w!("Snapshot.Setup");
const WIDTH: i32 = 540;
const PAD: i32 = 28;
const HEADER: i32 = 84;
const ICON: i32 = 40;
const FIELD: i32 = 38;
const FIELD_LABEL: i32 = 84;
const BROWSE: i32 = 72;
const ROW: i32 = 50;
const FOOTER: i32 = 64;
const GAP: i32 = 12;
const BUTTON: i32 = 96;
const PARENT_EDIT: usize = 100;
const NAME_EDIT: usize = 101;
const EM_SETSEL: u32 = 0x00b1;
const EN_SETFOCUS: u32 = 0x0100;
const EN_KILLFOCUS: u32 = 0x0200;
const EN_CHANGE: u32 = 0x0300;
/// Posted by the worker thread when installing or uninstalling finished.
const WORK_DONE: u32 = WM_APP + 40;
const WRAP: u32 = DT_WORDBREAK | DT_EDITCONTROL;
const INSTALL_SWITCHES: [(&str, &str); 3] = [
    ("开始菜单快捷方式", "在开始菜单里搜 just-snapshot 就能打开"),
    ("桌面快捷方式", "在桌面放一个图标"),
    ("安装完成后启动", "启动后在托盘运行，按 Ctrl+Alt+A 截图"),
];
const UNINSTALL_SWITCHES: [(&str, &str); 1] =
    [("同时删除我的设置", "快捷键、保存和翻译设置、翻译密钥；截图和录像不会删除")];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Close,
    Browse,
    Switch(usize),
    Cancel,
    Primary,
    OpenFolder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Form,
    Working,
    Done { title: String, detail: String },
}

struct State {
    mode: Mode,
    phase: Phase,
    places: Places,
    /// The folder being uninstalled, or the one installed into.
    dir: Option<PathBuf>,
    parent: HWND,
    name: HWND,
    switches: Vec<bool>,
    hovered: Option<Target>,
    pressed: Option<Target>,
    focused: Option<HWND>,
    error: Option<String>,
    /// The running executable to delete once the window is gone.
    pending_delete: Option<PathBuf>,
    scale: f32,
    fonts: Fonts,
    brush: HBRUSH,
    icon: crate::capture::Image,
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

/// The worker thread's result, picked up on `WORK_DONE`.
static RESULT: Mutex<Option<Result<Phase, String>>> = Mutex::new(None);

struct Layout {
    scale: f32,
    width: i32,
    height: i32,
    close: RECT,
    icon: RECT,
    /// Install: the two fields and the resulting path. Uninstall: the
    /// description of what goes.
    parent: RECT,
    browse: RECT,
    name: RECT,
    summary: RECT,
    switches: Vec<RECT>,
    toggles: Vec<RECT>,
    badge: RECT,
    done_title: RECT,
    done_detail: RECT,
    error: RECT,
    footer: i32,
    secondary: RECT,
    primary: RECT,
}

impl Layout {
    unsafe fn new(state: &State) -> Self {
        let scale = state.scale;
        let s = |value: i32| (value as f32 * scale).round() as i32;
        let inner = s(WIDTH - PAD * 2);
        let dc = GetDC(null_mut());
        let wrapped = |text: &str, font: HFONT, width: i32| ui::wrapped_height(dc, text, font, width, WRAP);
        let close = RECT { left: s(WIDTH - 12 - 36), top: s(12), right: s(WIDTH - 12), bottom: s(48) };
        let icon_top = s((HEADER - ICON) / 2);
        let icon = RECT { left: s(PAD), top: icon_top, right: s(PAD + ICON), bottom: icon_top + s(ICON) };
        let mut y = s(HEADER);
        let empty = RECT { left: s(PAD), top: y, right: s(PAD), bottom: y };
        let (mut parent, mut browse, mut name, mut summary) = (empty, empty, empty, empty);
        let (mut badge, mut done_title, mut done_detail) = (empty, empty, empty);
        let mut switches = Vec::new();
        let mut toggles = Vec::new();
        if let Phase::Done { title, detail } = &state.phase {
            y += s(8);
            badge = RECT { left: s(PAD), top: y, right: s(PAD + 44), bottom: y + s(44) };
            let left = s(PAD + 44 + 16);
            let title_height = wrapped(title, state.fonts.title, s(WIDTH - PAD) - left);
            done_title = RECT { left, top: y + s(2), right: s(WIDTH - PAD), bottom: y + s(2) + title_height };
            let detail_height = wrapped(detail, state.fonts.body, s(WIDTH - PAD) - left);
            done_detail = RECT { left, top: done_title.bottom + s(8), right: s(WIDTH - PAD), bottom: done_title.bottom + s(8) + detail_height };
            y = done_detail.bottom.max(badge.bottom) + s(24);
        } else {
            let labels: &[(&str, &str)] = match state.mode {
                Mode::Install => {
                    parent = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD - BROWSE - 8), bottom: y + s(FIELD) };
                    browse = RECT { left: s(WIDTH - PAD - BROWSE), top: y, right: s(WIDTH - PAD), bottom: y + s(FIELD) };
                    y += s(FIELD + GAP);
                    name = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + s(FIELD) };
                    y += s(FIELD + 8);
                    let text = summary_text(state);
                    let height = wrapped(&text, state.fonts.small, inner - s(FIELD_LABEL));
                    summary = RECT { left: s(PAD + FIELD_LABEL), top: y, right: s(WIDTH - PAD), bottom: y + height };
                    y += height + s(GAP + 4);
                    &INSTALL_SWITCHES
                }
                Mode::Uninstall => {
                    let text = summary_text(state);
                    let height = wrapped(&text, state.fonts.body, inner);
                    summary = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + height };
                    y += height + s(GAP + 4);
                    &UNINSTALL_SWITCHES
                }
            };
            for _ in labels {
                switches.push(RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + s(ROW) });
                let top = y + s((ROW - 24) / 2);
                toggles.push(RECT { left: s(WIDTH - PAD - 44), top, right: s(WIDTH - PAD), bottom: top + s(24) });
                y += s(ROW);
            }
        }
        let error_height = match &state.error {
            Some(error) => wrapped(error, state.fonts.small, inner) + s(10),
            None => 0,
        };
        ReleaseDC(null_mut(), dc);
        let error = RECT { left: s(PAD), top: y + s(8), right: s(WIDTH - PAD), bottom: y + error_height };
        y += error_height + s(GAP);
        let footer = y;
        let top = y + s((FOOTER - 36) / 2);
        let primary = RECT { left: s(WIDTH - PAD - BUTTON), top, right: s(WIDTH - PAD), bottom: top + s(36) };
        let secondary_width = if matches!(state.phase, Phase::Done { .. }) { 128 } else { BUTTON };
        let secondary = RECT {
            left: primary.left - s(10 + secondary_width),
            top,
            right: primary.left - s(10),
            bottom: top + s(36),
        };
        Self {
            scale,
            width: s(WIDTH),
            height: y + s(FOOTER),
            close,
            icon,
            parent,
            browse,
            name,
            summary,
            switches,
            toggles,
            badge,
            done_title,
            done_detail,
            error,
            footer,
            secondary,
            primary,
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

    fn targets(&self, state: &State) -> Vec<(Target, RECT)> {
        let mut targets = vec![(Target::Primary, self.primary)];
        match &state.phase {
            Phase::Working => return Vec::new(),
            Phase::Done { .. } => {
                if state.mode == Mode::Install {
                    targets.push((Target::OpenFolder, self.secondary));
                }
            }
            Phase::Form => {
                targets.push((Target::Close, self.close));
                targets.push((Target::Cancel, self.secondary));
                if state.mode == Mode::Install {
                    targets.push((Target::Browse, self.browse));
                }
                targets.extend(self.switches.iter().enumerate().map(|(index, &rect)| (Target::Switch(index), rect)));
            }
        }
        targets
    }

    fn hit(&self, state: &State, point: POINT) -> Option<Target> {
        self.targets(state).into_iter().find(|(_, rect)| contains(*rect, point)).map(|(target, _)| target)
    }
}

unsafe fn text_of(edit: HWND) -> String {
    let length = GetWindowTextLengthW(edit);
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(edit, buffer.as_mut_ptr(), buffer.len() as i32);
    String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
}

/// The install folder as typed so far.
unsafe fn typed_dir(state: &State) -> Result<PathBuf, String> {
    install_dir(&text_of(state.parent), &text_of(state.name))
}

/// The line under the fields (install) or the description (uninstall).
unsafe fn summary_text(state: &State) -> String {
    match state.mode {
        Mode::Install => match typed_dir(state) {
            Ok(dir) => format!("将安装到  {}", dir.display()),
            Err(error) => error,
        },
        Mode::Uninstall => {
            let dir = state.dir.as_ref().map_or_else(String::new, |dir| dir.display().to_string());
            format!("将删除 {dir} 里的 just-snapshot，以及开始菜单和桌面快捷方式、开机启动项。正在运行的 just-snapshot 会先退出。")
        }
    }
}

/// Opens the window and runs until it closes.
pub unsafe fn run(mode: Mode) -> Result<(), String> {
    // Match the app's look when its settings exist.
    if let Ok(preferences) = crate::hotkey::load() {
        theme::set(preferences.theme);
    }
    let instance = GetModuleHandleW(null());
    let big = crate::app_icon::create(32, false);
    let class = WNDCLASSW {
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hIcon: big,
        lpszClassName: CLASS,
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let places = Places::system();
    let dir = match mode {
        Mode::Install => None,
        Mode::Uninstall => registered_dir(&places)
            .or_else(|| env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf))),
    };
    let title = match mode {
        Mode::Install => w!("安装 just-snapshot"),
        Mode::Uninstall => w!("卸载 just-snapshot"),
    };
    let window = CreateWindowExW(
        WS_EX_APPWINDOW | WS_EX_CONTROLPARENT,
        CLASS,
        title,
        WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX,
        0,
        0,
        WIDTH,
        WIDTH,
        null_mut(),
        null_mut(),
        instance,
        null(),
    );
    if window.is_null() {
        return Err(io::Error::last_os_error().to_string());
    }
    SendMessageW(window, WM_SETICON, ICON_SMALL as WPARAM, crate::app_icon::create(16, false) as LPARAM);
    let scale = match GetDpiForWindow(window) {
        0 => 1.0,
        dpi => dpi as f32 / 96.0,
    };
    let (parent_text, name_text) = initial_location(&places);
    let edit = |id: usize, value: &str, visible: bool| {
        CreateWindowExW(
            0,
            w!("EDIT"),
            wide(value).as_ptr(),
            WS_CHILD | WS_TABSTOP | ES_AUTOHSCROLL as u32 | if visible { WS_VISIBLE } else { 0 },
            0,
            0,
            0,
            0,
            window,
            id as _,
            instance,
            null(),
        )
    };
    let install = mode == Mode::Install;
    let parent = edit(PARENT_EDIT, &parent_text, install);
    let name = edit(NAME_EDIT, &name_text, install);
    STATE.set(Some(State {
        mode,
        phase: Phase::Form,
        places,
        dir,
        parent,
        name,
        switches: if install { vec![true, false, true] } else { vec![false] },
        hovered: None,
        pressed: None,
        focused: None,
        error: None,
        pending_delete: None,
        scale,
        fonts: Fonts::new(scale),
        brush: CreateSolidBrush(theme::palette().card),
        icon: crate::app_icon::render((ICON as f32 * scale).round() as i32, false),
        tracking: false,
    }));
    WINDOW.set(window);
    place(window, true);
    ui::round_corners(window);
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    if install {
        SetFocus(parent);
        SendMessageW(parent, EM_SETSEL, 0, -1);
    } else {
        SetFocus(window);
    }
    let mut message = MSG::default();
    while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
        if IsDialogMessageW(window, &message) != 0 {
            continue;
        }
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    Ok(())
}

/// Sizes the window to its layout (centered on the screen when `center`)
/// and positions the edit boxes.
unsafe fn place(window: HWND, center: bool) {
    let Some(layout) = layout() else { return };
    let mut bounds = RECT::default();
    GetWindowRect(window, &mut bounds);
    if center {
        let mut monitor = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mut cursor = POINT::default();
        GetCursorPos(&mut cursor);
        GetMonitorInfoW(MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST), &mut monitor);
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
        let shown = state.mode == Mode::Install && !matches!(state.phase, Phase::Done { .. });
        Some([
            (state.parent, layout.edit_rect(layout.parent, metrics.tmHeight)),
            (state.name, layout.edit_rect(layout.name, metrics.tmHeight)),
        ]
        .map(|(edit, rect)| (edit, state.fonts.body, rect, shown)))
    });
    for (edit, font, rect, shown) in boxes.into_iter().flatten() {
        SendMessageW(edit, WM_SETFONT, font as WPARAM, 0);
        SetWindowPos(
            edit,
            null_mut(),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOZORDER | SWP_NOACTIVATE | if shown { SWP_SHOWWINDOW } else { SWP_HIDEWINDOW },
        );
    }
    InvalidateRect(window, null(), 0);
}

unsafe fn layout() -> Option<Layout> {
    STATE.with_borrow(|state| state.as_ref().map(|state| Layout::new(state)))
}

fn with_state<R>(action: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with_borrow_mut(|state| state.as_mut().map(action))
}

unsafe fn hit(lparam: LPARAM) -> Option<Target> {
    let point = mouse_point(lparam);
    STATE.with_borrow(|state| state.as_ref().and_then(|state| Layout::new(state).hit(state, point)))
}

/// Starts installing or uninstalling on a worker thread; the window shows
/// the working state until `WORK_DONE`.
unsafe fn start(window: HWND) {
    let Some(job) = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        let dir = match state.mode {
            Mode::Install => typed_dir(state),
            Mode::Uninstall => state.dir.clone().ok_or_else(|| "找不到安装位置。".to_string()),
        };
        Some((state.mode, dir, state.switches.clone(), state.places.clone()))
    }) else {
        return;
    };
    let (mode, dir, switches, places) = job;
    let dir = match dir {
        Ok(dir) => dir,
        Err(error) => {
            with_state(|state| state.error = Some(error));
            place(window, false);
            return;
        }
    };
    with_state(|state| {
        state.phase = Phase::Working;
        state.error = None;
        state.hovered = None;
        state.dir = Some(dir.clone());
    });
    set_editable(false);
    place(window, false);
    UpdateWindow(window);
    let handle = window as isize;
    thread::spawn(move || {
        let result = match mode {
            Mode::Install => work_install(&dir, Choices { start_menu: switches[0], desktop: switches[1], launch: switches[2] }, &places),
            Mode::Uninstall => work_uninstall(&dir, switches[0], &places),
        };
        *RESULT.lock().unwrap() = Some(result);
        unsafe { PostMessageW(handle as HWND, WORK_DONE, 0, 0) };
    });
}

fn work_install(dir: &Path, choices: Choices, places: &Places) -> Result<Phase, String> {
    let source = env::current_exe().map_err(|error| format!("无法获取安装程序的路径。\n{error}"))?;
    let was_running = stop_running();
    let exe = install(&source, dir, choices, places)?;
    let launched = choices.launch
        && Command::new(&exe)
            .current_dir(dir)
            .spawn()
            .is_ok();
    let detail = if launched {
        format!("just-snapshot 已经在托盘运行，按 Ctrl+Alt+A 开始截图。\n\n安装位置：{}", dir.display())
    } else if was_running {
        format!("之前运行的 just-snapshot 已退出，从开始菜单或安装位置重新打开即可。\n\n安装位置：{}", dir.display())
    } else {
        format!("安装位置：{}", dir.display())
    };
    Ok(Phase::Done { title: "安装完成".into(), detail })
}

fn work_uninstall(dir: &Path, settings: bool, places: &Places) -> Result<Phase, String> {
    stop_running();
    let running = env::current_exe().map_err(|error| format!("无法获取程序路径。\n{error}"))?;
    let removed = uninstall(dir, &running, settings, places)?;
    *PENDING_DELETE.lock().unwrap() = removed.pending;
    let mut detail = String::from("感谢使用 just-snapshot。");
    if !settings {
        detail.push_str("\n设置还保留着，以后重新安装会接着用。");
    }
    for folder in removed.kept {
        detail.push_str(&format!("\n录像还在 {}，需要的话自己删除。", folder.display()));
    }
    Ok(Phase::Done { title: "已卸载".into(), detail })
}

/// The uninstalled executable that is still running, from the worker thread.
static PENDING_DELETE: Mutex<Option<PathBuf>> = Mutex::new(None);

unsafe fn finish_work(window: HWND) {
    let Some(result) = RESULT.lock().unwrap().take() else { return };
    if let Some(exe) = PENDING_DELETE.lock().unwrap().take() {
        with_state(|state| state.pending_delete = Some(exe));
    }
    set_editable(true);
    with_state(|state| match result {
        Ok(phase) => state.phase = phase,
        Err(error) => {
            state.phase = Phase::Form;
            state.error = Some(error);
        }
    });
    place(window, false);
}

/// The boxes stay visible but locked while working.
unsafe fn set_editable(editable: bool) {
    let edits = STATE.with_borrow(|state| state.as_ref().map(|state| [state.parent, state.name]));
    for edit in edits.into_iter().flatten() {
        EnableWindow(edit, editable as i32);
    }
}

unsafe fn activate(window: HWND, target: Target) {
    let phase = STATE.with_borrow(|state| state.as_ref().map(|state| state.phase.clone()));
    match target {
        Target::Close | Target::Cancel => {
            DestroyWindow(window);
        }
        Target::Browse => {
            let Some(edit) = STATE.with_borrow(|state| state.as_ref().map(|state| state.parent)) else { return };
            if let Some(folder) = crate::output_settings::pick_folder(window, text_of(edit).trim()) {
                SetWindowTextW(edit, wide(&folder).as_ptr());
            }
        }
        Target::Switch(index) => {
            with_state(|state| state.switches[index] = !state.switches[index]);
            InvalidateRect(window, null(), 0);
        }
        Target::Primary => match phase {
            Some(Phase::Form) => start(window),
            Some(Phase::Done { .. }) => {
                DestroyWindow(window);
            }
            _ => {}
        },
        Target::OpenFolder => {
            if let Some(dir) = STATE.with_borrow(|state| state.as_ref().and_then(|state| state.dir.clone())) {
                let _ = Command::new("explorer.exe").arg(&dir).spawn();
            }
            DestroyWindow(window);
        }
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
            let header = STATE.with_borrow(|state| {
                state.as_ref().is_some_and(|state| {
                    let layout = Layout::new(state);
                    point.y < layout.s(HEADER) && layout.hit(state, point).is_none()
                })
            });
            return (if header { HTCAPTION } else { HTCLIENT }) as LRESULT;
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
            let hit = hit(lparam);
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
            let (clickable, working) = STATE.with_borrow(|state| {
                state.as_ref().map_or((false, false), |state| (state.hovered.is_some(), state.phase == Phase::Working))
            });
            let cursor = if working {
                IDC_APPSTARTING
            } else if clickable {
                IDC_HAND
            } else {
                IDC_ARROW
            };
            SetCursor(LoadCursorW(null_mut(), cursor));
            return 1;
        }
        WM_LBUTTONDOWN => {
            let hit = hit(lparam);
            with_state(|state| state.pressed = hit);
            SetCapture(window);
            InvalidateRect(window, null(), 0);
        }
        WM_LBUTTONUP => {
            let pressed = with_state(|state| state.pressed.take()).flatten();
            ReleaseCapture();
            InvalidateRect(window, null(), 0);
            if let Some(target) = pressed.filter(|&target| Some(target) == hit(lparam)) {
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
                // Enter and Esc, through IsDialogMessage.
                1 => activate(window, Target::Primary),
                2 => {
                    let idle = STATE.with_borrow(|state| state.as_ref().is_some_and(|state| state.phase != Phase::Working));
                    if idle {
                        DestroyWindow(window);
                    }
                }
                PARENT_EDIT | NAME_EDIT => match code {
                    EN_SETFOCUS => {
                        with_state(|state| state.focused = Some(lparam as HWND));
                        InvalidateRect(window, null(), 0);
                    }
                    EN_KILLFOCUS => {
                        with_state(|state| state.focused = None);
                        InvalidateRect(window, null(), 0);
                    }
                    // The resulting path follows the typing; it may wrap to
                    // another line, so lay out again.
                    EN_CHANGE => {
                        with_state(|state| state.error = None);
                        place(window, false);
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
        WORK_DONE => finish_work(window),
        WM_DPICHANGED => {
            let scale = (wparam & 0xffff) as f32 / 96.0;
            with_state(|state| {
                state.scale = scale;
                state.fonts = Fonts::new(scale);
                state.icon = crate::app_icon::render((ICON as f32 * scale).round() as i32, false);
            });
            let suggested = *(lparam as *const RECT);
            SetWindowPos(window, null_mut(), suggested.left, suggested.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            place(window, false);
        }
        WM_CLOSE => {
            let idle = STATE.with_borrow(|state| state.as_ref().is_some_and(|state| state.phase != Phase::Working));
            if idle {
                DestroyWindow(window);
            }
        }
        WM_DESTROY => {
            if let Some(exe) = STATE.take().and_then(|state| state.pending_delete.clone()) {
                delete_after_exit(&exe);
            }
            WINDOW.set(null_mut());
            PostQuitMessage(0);
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
    let single = DT_SINGLELINE | DT_VCENTER;
    ui::fill(dc, client, palette.background);
    if let Some(graphics) = Graphics::new(dc) {
        graphics.stroke_round(ui::inset(ui::to_f(client), 0.5), 0.0, argb(palette.card_border, 255), 1.0);
        let [left, top, right, bottom] = ui::to_f(layout.icon);
        use windows_sys::Win32::Graphics::GdiPlus::PointF;
        graphics.image(
            &state.icon,
            [PointF { X: left, Y: top }, PointF { X: right, Y: top }, PointF { X: left, Y: bottom }],
            true,
        );
    }

    // Header: icon, title and version.
    let title = match state.mode {
        Mode::Install => "安装 just-snapshot",
        Mode::Uninstall => "卸载 just-snapshot",
    };
    let text_left = layout.icon.right + s(14);
    let middle = (layout.icon.top + layout.icon.bottom) / 2;
    ui::text(
        dc,
        RECT { left: text_left, top: layout.icon.top - s(4), right: layout.close.left, bottom: middle + s(2) },
        title,
        fonts.title,
        palette.text,
        DT_LEFT | DT_BOTTOM | DT_SINGLELINE,
    );
    ui::text(
        dc,
        RECT { left: text_left, top: middle + s(4), right: layout.close.left, bottom: layout.icon.bottom + s(4) },
        &format!("v{VERSION} · 截图、贴图、录屏"),
        fonts.small,
        palette.secondary,
        DT_LEFT | DT_TOP | DT_SINGLELINE,
    );
    if state.phase == Phase::Form {
        if state.hovered == Some(Target::Close) || state.pressed == Some(Target::Close) {
            if let Some(graphics) = Graphics::new(dc) {
                graphics.fill_round(ui::to_f(layout.close), radius, argb(face(Target::Close, palette.background), 255));
            }
        }
        icons::paint(dc, ui::inflate(layout.close, -s(9)), Icon::Close, palette.secondary);
    }

    match &state.phase {
        Phase::Done { title, detail } => {
            ui::badge(dc, layout.badge, palette.success, Badge::Check, palette.on_accent);
            ui::text(dc, layout.done_title, title, fonts.title, palette.text, DT_LEFT | DT_TOP | WRAP);
            ui::text(dc, layout.done_detail, detail, fonts.body, palette.secondary, DT_LEFT | DT_TOP | WRAP);
        }
        _ => {
            let working = state.phase == Phase::Working;
            if state.mode == Mode::Install {
                for (row, label, edit) in [(layout.parent, "安装位置", state.parent), (layout.name, "文件夹名", state.name)] {
                    ui::text(dc, RECT { right: row.left + s(FIELD_LABEL - 8), ..row }, label, fonts.body, palette.secondary, DT_LEFT | single);
                    let field = RECT { left: row.left + s(FIELD_LABEL), ..row };
                    let focused = state.focused == Some(edit) && !working;
                    if let Some(graphics) = Graphics::new(dc) {
                        graphics.fill_round(ui::to_f(field), radius, argb(palette.card, 255));
                        let (border, width) = if focused { (palette.accent, 1.5) } else { (palette.key_border, 1.0) };
                        graphics.stroke_round(ui::inset(ui::to_f(field), width / 2.0), radius, argb(border, 255), width);
                    }
                }
                ui::button(dc, layout.browse, "选择…", fonts.body, palette, false, face(Target::Browse, palette.card), radius);
                let valid = typed_dir(state).is_ok();
                ui::text(
                    dc,
                    layout.summary,
                    &summary_text(state),
                    fonts.small,
                    if valid { palette.tertiary } else { palette.warning },
                    DT_LEFT | DT_TOP | WRAP,
                );
            } else {
                ui::text(dc, layout.summary, &summary_text(state), fonts.body, palette.secondary, DT_LEFT | DT_TOP | WRAP);
            }
            let labels: &[(&str, &str)] = if state.mode == Mode::Install { &INSTALL_SWITCHES } else { &UNINSTALL_SWITCHES };
            for (index, (title, description)) in labels.iter().enumerate() {
                let row = layout.switches[index];
                ui::fill(dc, RECT { bottom: row.top + 1, ..row }, palette.divider);
                let middle = (row.top + row.bottom) / 2;
                let text = RECT { right: layout.toggles[index].left - s(12), ..row };
                ui::text(dc, RECT { top: middle - s(21), bottom: middle, ..text }, title, fonts.body, palette.text, DT_LEFT | DT_BOTTOM | DT_SINGLELINE);
                ui::text(
                    dc,
                    RECT { top: middle + s(1), bottom: middle + s(21), ..text },
                    description,
                    fonts.small,
                    palette.secondary,
                    DT_LEFT | DT_TOP | DT_SINGLELINE,
                );
                let on = state.switches[index];
                let hover = if state.hovered == Some(Target::Switch(index)) { 1.0 } else { 0.0 };
                if let Some(graphics) = Graphics::new(dc) {
                    let bounds = ui::to_f(layout.toggles[index]);
                    let height = bounds[3] - bounds[1];
                    let off = mix(palette.track_off, palette.secondary, hover * 0.3);
                    let track = if on { mix(palette.accent, palette.accent_hover, hover) } else { off };
                    graphics.fill_round(bounds, height / 2.0, argb(track, if working { 140 } else { 255 }));
                    let margin = 3.0 * layout.scale;
                    let knob = height - margin * 2.0;
                    let left = if on { bounds[2] - margin - knob } else { bounds[0] + margin };
                    let top = bounds[1] + margin;
                    graphics.fill_ellipse([left, top, left + knob, top + knob], argb(0x00ff_ffff, 255));
                }
            }
        }
    }
    if let Some(error) = &state.error {
        ui::text(dc, layout.error, error, fonts.small, palette.error, DT_LEFT | DT_TOP | WRAP);
    }

    ui::fill(dc, RECT { left: 1, top: layout.footer, right: layout.width - 1, bottom: layout.footer + 1 }, palette.card_border);
    let (secondary, primary) = match (&state.phase, state.mode) {
        (Phase::Done { .. }, Mode::Install) => (Some("打开安装位置"), "完成"),
        (Phase::Done { .. }, Mode::Uninstall) => (None, "完成"),
        (Phase::Working, Mode::Install) => (None, "正在安装…"),
        (Phase::Working, Mode::Uninstall) => (None, "正在卸载…"),
        (Phase::Form, Mode::Install) => (Some("取消"), "安装"),
        (Phase::Form, Mode::Uninstall) => (Some("取消"), "卸载"),
    };
    let working = state.phase == Phase::Working;
    if let Some(label) = secondary {
        let target = if matches!(state.phase, Phase::Done { .. }) { Target::OpenFolder } else { Target::Cancel };
        ui::button(dc, layout.secondary, label, fonts.body, palette, false, face(target, palette.card), radius);
    }
    // Uninstalling is the one destructive action: red instead of the accent.
    let (base, hover) = if state.mode == Mode::Uninstall && state.phase == Phase::Form {
        (palette.error, mix(palette.error, 0x00ff_ffff, 0.12))
    } else {
        (palette.accent, palette.accent_hover)
    };
    let primary_face = match (working, state.pressed == Some(Target::Primary), state.hovered == Some(Target::Primary)) {
        (true, ..) => mix(base, palette.background, 0.35),
        (_, true, _) => mix(hover, 0, 0.12),
        (_, _, true) => hover,
        _ => base,
    };
    ui::button(dc, layout.primary, primary, fonts.heading, palette, true, primary_face, radius);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_program_folder_goes_inside_the_chosen_one() {
        assert_eq!(install_dir(r"D:\", NAME).unwrap(), PathBuf::from(r"D:\just-snapshot"));
        assert_eq!(install_dir("D:", NAME).unwrap(), PathBuf::from(r"D:\just-snapshot"));
        assert_eq!(install_dir(r" D:\Tools\ ", "截图").unwrap(), PathBuf::from(r"D:\Tools\截图"));
        assert_eq!(install_dir(r#""E:\Apps""#, NAME).unwrap(), PathBuf::from(r"E:\Apps\just-snapshot"));
        assert_eq!(install_dir(r"\\nas\tools", NAME).unwrap(), PathBuf::from(r"\\nas\tools\just-snapshot"));
    }

    #[test]
    fn a_folder_that_already_has_the_name_is_not_nested() {
        assert_eq!(install_dir(r"D:\just-snapshot", NAME).unwrap(), PathBuf::from(r"D:\just-snapshot"));
        assert_eq!(install_dir(r"D:\Just-Snapshot\", NAME).unwrap(), PathBuf::from(r"D:\Just-Snapshot"));
    }

    #[test]
    fn bad_locations_and_names_are_explained() {
        assert!(install_dir("", NAME).unwrap_err().contains("选择安装位置"));
        assert!(install_dir("Tools", NAME).unwrap_err().contains("完整路径"));
        assert!(install_dir(r"D:\", "  ").unwrap_err().contains("不能为空"));
        assert!(install_dir(r"D:\", "a/b").unwrap_err().contains("不能有"));
        assert_eq!(install_dir(r"D:\", "tools. ").unwrap(), PathBuf::from(r"D:\tools"));
    }

    #[test]
    fn setup_names_start_the_installer() {
        let args = |extra: &str| vec!["x.exe".to_string(), extra.to_string()];
        assert_eq!(mode(&args("--uninstall")), Some(Mode::Uninstall));
        assert_eq!(mode(&args("--setup")), Some(Mode::Install));
        // The test binary's own name has no "setup" in it.
        assert_eq!(mode(&args("--after")), None);
    }

    fn scratch(name: &str) -> PathBuf {
        let folder = env::temp_dir().join(format!("snapshot-setup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn placing_replaces_an_existing_executable() {
        let folder = scratch("place");
        let (source, target) = (folder.join("new.exe"), folder.join(EXE));
        fs::write(&source, b"new").unwrap();
        place_file(&source, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        fs::write(&source, b"newer").unwrap();
        fs::write(with_suffix(&target, ".old"), b"left over").unwrap();
        place_file(&source, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"newer");
        assert!(!with_suffix(&target, ".old").exists());
        assert!(!with_suffix(&target, ".new").exists());
        fs::remove_dir_all(&folder).unwrap();
    }

    fn test_places(folder: &Path) -> Places {
        let start_menu = folder.join("start");
        let desktop = folder.join("desktop");
        fs::create_dir_all(&start_menu).unwrap();
        fs::create_dir_all(&desktop).unwrap();
        Places {
            start_menu: Some(start_menu),
            desktop: Some(desktop),
            settings: Some(folder.join("settings")),
            uninstall_key: format!(r"Software\just-snapshot-test\Uninstall-{}", std::process::id()),
        }
    }

    /// Installs into a scratch folder with shortcuts and a test registry
    /// key, then uninstalls and checks that only the user's files remain.
    #[test]
    fn install_then_uninstall_leaves_only_the_users_files() {
        let folder = scratch("round-trip");
        let places = test_places(&folder);
        let source = folder.join("just-snapshot-setup.exe");
        fs::write(&source, b"program").unwrap();
        let dir = install_dir(&folder.to_string_lossy(), NAME).unwrap();
        let choices = Choices { start_menu: true, desktop: true, launch: false };
        let exe = install(&source, &dir, choices, &places).unwrap();
        assert_eq!(exe, dir.join(EXE));
        assert_eq!(fs::read(&exe).unwrap(), b"program");
        assert!(dir.join(r"licenses\rxing.txt").exists());
        assert!(places.start_menu.as_ref().unwrap().join(SHORTCUT).exists());
        assert!(places.desktop.as_ref().unwrap().join(SHORTCUT).exists());
        assert_eq!(registered_dir(&places), Some(dir.clone()));
        assert_eq!(get_string(&places.uninstall_key, "UninstallString"), Some(format!("\"{}\" --uninstall", exe.display())));
        // Installing again over it works and keeps the registration.
        install(&source, &dir, Choices { desktop: false, ..choices }, &places).unwrap();

        // A recording the user made next to the program.
        fs::create_dir_all(dir.join("Videos")).unwrap();
        fs::write(dir.join(r"Videos\clip.mp4"), b"video").unwrap();
        let removed = uninstall(&dir, &source, false, &places).unwrap();
        assert!(removed.pending.is_none());
        assert_eq!(removed.kept, vec![dir.join("Videos")]);
        assert!(!exe.exists());
        assert!(!dir.join("LICENSE.txt").exists() && !dir.join("licenses").exists());
        assert!(dir.join(r"Videos\clip.mp4").exists());
        assert!(!places.start_menu.as_ref().unwrap().join(SHORTCUT).exists());
        assert!(!places.desktop.as_ref().unwrap().join(SHORTCUT).exists());
        assert_eq!(registered_dir(&places), None);

        // Without user files the folder goes too; the running copy waits.
        let dir = install_dir(&folder.to_string_lossy(), "second").unwrap();
        let exe = install(&source, &dir, Choices { start_menu: false, desktop: false, launch: false }, &places).unwrap();
        let removed = uninstall(&dir, &exe, false, &places).unwrap();
        assert_eq!(removed.pending, Some(exe.clone()));
        assert!(exe.exists() && !dir.join("LICENSE.txt").exists());
        fs::remove_file(&exe).unwrap();
        fs::remove_dir(&dir).unwrap();
        let _ = unsafe {
            RegDeleteTreeW(HKEY_CURRENT_USER, wide(r"Software\just-snapshot-test").as_ptr())
        };
        let _ = unsafe { windows_sys::Win32::System::Registry::RegDeleteKeyW(HKEY_CURRENT_USER, wide(r"Software\just-snapshot-test").as_ptr()) };
        fs::remove_dir_all(&folder).unwrap();
    }

    unsafe fn render(mode: Mode, setup: impl FnOnce(&mut State), file: &str) {
        let window = WINDOW.get();
        if window.is_null() {
            let class = WNDCLASSW {
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: GetModuleHandleW(null()),
                lpszClassName: w!("Snapshot.SetupPreview"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let window = CreateWindowExW(0, class.lpszClassName, null(), WS_POPUP, 0, 0, 10, 10, null_mut(), null_mut(), class.hInstance, null());
            WINDOW.set(window);
        }
        let window = WINDOW.get();
        let edit = |value: &str| {
            CreateWindowExW(0, w!("EDIT"), wide(value).as_ptr(), WS_CHILD, 0, 0, 0, 0, window, null_mut(), GetModuleHandleW(null()), null())
        };
        let install = mode == Mode::Install;
        let mut state = State {
            mode,
            phase: Phase::Form,
            places: Places { start_menu: None, desktop: None, settings: None, uninstall_key: String::new() },
            dir: Some(PathBuf::from(r"C:\Users\eric\AppData\Local\Programs\just-snapshot")),
            parent: edit(r"C:\Users\eric\AppData\Local\Programs"),
            name: edit(NAME),
            switches: if install { vec![true, false, true] } else { vec![false] },
            hovered: None,
            pressed: None,
            focused: None,
            error: None,
            pending_delete: None,
            scale: 1.0,
            fonts: Fonts::new(1.0),
            brush: null_mut(),
            icon: crate::app_icon::render(ICON, false),
            tracking: false,
        };
        setup(&mut state);
        let layout = Layout::new(&state);
        let client = RECT { left: 0, top: 0, right: layout.width, bottom: layout.height };
        let surface = crate::capture::Surface::new(null_mut(), client.right, client.bottom).unwrap();
        paint(surface.dc, client, &state);
        GdiFlush();
        let mut image = surface.image(client.right, client.bottom).unwrap();
        for pixel in image.pixels.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        let folder = env::temp_dir().join("snapshot-preview");
        fs::create_dir_all(&folder).unwrap();
        image.save_png(&folder.join(file)).unwrap();
    }

    /// The window in each state (native edit boxes do not render here, the
    /// fields are drawn empty). Run with `<test binary> setup --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        unsafe {
            for theme in theme::THEMES {
                theme::set(theme);
                render(Mode::Install, |state| state.hovered = Some(Target::Primary), &format!("setup-install-{}.png", theme.key()));
            }
            theme::set(theme::Theme::Graphite);
            render(Mode::Install, |state| { SetWindowTextW(state.parent, w!("Tools")); }, "setup-install-invalid.png");
            render(Mode::Install, |state| state.phase = Phase::Working, "setup-install-working.png");
            render(
                Mode::Install,
                |state| {
                    state.phase = Phase::Done {
                        title: "安装完成".into(),
                        detail: "just-snapshot 已经在托盘运行，按 Ctrl+Alt+A 开始截图。\n\n安装位置：D:\\just-snapshot".into(),
                    }
                },
                "setup-install-done.png",
            );
            render(
                Mode::Install,
                |state| state.error = Some("无法创建文件夹 C:\\Program Files\\just-snapshot。\n拒绝访问。 (os error 5)\n这个位置需要管理员权限，换一个位置吧，例如 D:\\ 或默认位置。".into()),
                "setup-install-error.png",
            );
            render(Mode::Uninstall, |state| state.hovered = Some(Target::Primary), "setup-uninstall.png");
            render(
                Mode::Uninstall,
                |state| {
                    state.phase = Phase::Done {
                        title: "已卸载".into(),
                        detail: "感谢使用 just-snapshot。\n设置还保留着，以后重新安装会接着用。\n录像还在 D:\\just-snapshot\\Videos，需要的话自己删除。".into(),
                    }
                },
                "setup-uninstall-done.png",
            );
        }
    }
}
