//! 保存设置: where saves go, how files are named, saving every capture
//! automatically, and the shadow and border added to saved images. Stored in
//! `%LOCALAPPDATA%\just-snapshot\output.txt`.

use std::{
    cell::{Cell, RefCell},
    env, fs, io,
    mem::size_of,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SYSTEMTIME, WPARAM},
        Graphics::Gdi::*,
        System::{LibraryLoader::GetModuleHandleW, SystemInformation::GetLocalTime},
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
    capture::Image,
    gdiplus::Graphics,
    icons::{self, Icon},
    selection_geometry::{contains, mouse_point},
    theme::{self, argb, mix},
    ui::{self, Fonts},
    wide,
};

pub const DEFAULT_TEMPLATE: &str = "just-snapshot {date} {time}";
/// Room around a saved image for its shadow.
const SHADOW_PAD: i32 = 28;
const SHADOW_OFFSET: i32 = 6;
const SHADOW_BLUR: usize = 8;
const SHADOW_ALPHA: f32 = 0.32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputConfig {
    /// Empty means the default, Pictures\Snapshot.
    pub folder: String,
    pub template: String,
    /// Every finished capture is also saved to the folder, and 保存 writes
    /// there directly instead of asking.
    pub auto_save: bool,
    pub shadow: bool,
    pub border: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            folder: String::new(),
            template: DEFAULT_TEMPLATE.into(),
            auto_save: false,
            shadow: false,
            border: false,
        }
    }
}

fn config_path() -> Result<PathBuf, String> {
    Ok(crate::migrate::app_data()?.join("output.txt"))
}

impl OutputConfig {
    pub fn load() -> Result<Self, String> {
        Self::load_from(&config_path()?)
    }

    fn load_from(path: &Path) -> Result<Self, String> {
        let mut config = Self::default();
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(config),
            Err(error) => return Err(format!("无法读取 {}。\n{error}", path.display())),
        };
        for line in contents.trim_start_matches('\u{feff}').lines() {
            let Some((name, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            let flag = value == "1" || value.eq_ignore_ascii_case("true");
            match name.trim() {
                "folder" => config.folder = value.into(),
                "template" if !value.is_empty() => config.template = value.into(),
                "auto_save" => config.auto_save = flag,
                "shadow" => config.shadow = flag,
                "border" => config.border = flag,
                _ => {}
            }
        }
        Ok(config)
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&config_path()?)
    }

    fn save_to(&self, path: &Path) -> Result<(), String> {
        let flag = |value: bool| if value { "1" } else { "0" };
        let contents = format!(
            "folder={}\r\ntemplate={}\r\nauto_save={}\r\nshadow={}\r\nborder={}\r\n",
            self.folder,
            self.template,
            flag(self.auto_save),
            flag(self.shadow),
            flag(self.border)
        );
        let error = |error: io::Error| format!("无法保存保存设置。\n{error}");
        fs::create_dir_all(path.parent().expect("settings directory")).map_err(error)?;
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, contents).map_err(error)?;
        fs::rename(&temporary, path).map_err(|failure| {
            let _ = fs::remove_file(&temporary);
            error(failure)
        })
    }

    /// The folder saves go to: the chosen one, or Pictures\just-snapshot.
    pub fn folder(&self) -> PathBuf {
        if !self.folder.trim().is_empty() {
            return PathBuf::from(self.folder.trim());
        }
        pictures().unwrap_or_else(|| env::temp_dir()).join(crate::migrate::NAME)
    }

    /// The file name for a capture of `width`×`height` taken now.
    pub fn file_name(&self, width: i32, height: i32) -> String {
        let mut time = SYSTEMTIME::default();
        unsafe { GetLocalTime(&mut time) };
        expand(&self.template, &time, width, height)
    }

    pub fn decorates(&self) -> bool {
        self.shadow || self.border
    }
}

/// Fills `{date}`, `{time}`, `{w}` and `{h}`, drops characters Windows does
/// not allow in names and adds `.png`.
pub fn expand(template: &str, time: &SYSTEMTIME, width: i32, height: i32) -> String {
    let name = template
        .replace("{date}", &format!("{:04}-{:02}-{:02}", time.wYear, time.wMonth, time.wDay))
        .replace("{time}", &format!("{:02}{:02}{:02}", time.wHour, time.wMinute, time.wSecond))
        .replace("{w}", &width.to_string())
        .replace("{h}", &height.to_string());
    let name: String = name
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') && !c.is_control())
        .collect();
    let name = name.trim().trim_end_matches('.');
    let name = if name.is_empty() { crate::migrate::NAME } else { name };
    let name = name.strip_suffix(".png").unwrap_or(name);
    format!("{name}.png")
}

/// `name` in `folder`, numbered " (2)", " (3)"… when it is taken.
pub fn unique_path(folder: &Path, name: &str) -> PathBuf {
    let path = folder.join(name);
    if !path.exists() {
        return path;
    }
    let stem = name.strip_suffix(".png").unwrap_or(name);
    (2..)
        .map(|index| folder.join(format!("{stem} ({index}).png")))
        .find(|path| !path.exists())
        .unwrap()
}

/// The user's Pictures folder, wherever it was moved.
pub fn pictures() -> Option<PathBuf> {
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
    };
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as _));
        text.map(PathBuf::from)
    }
}

/// A copy of `image` with a soft drop shadow and/or a hairline border.
/// The result carries real alpha: transparent around the shadow.
pub fn decorate(image: &Image, shadow: bool, border: bool) -> Image {
    let pad = if shadow { SHADOW_PAD } else { 0 };
    let (width, height) = (image.width + pad * 2, image.height + pad * 2);
    let (w, h) = (width as usize, height as usize);
    let mut pixels = vec![0u8; w * h * 4];
    if shadow {
        let mut mask = vec![0f32; w * h];
        for y in (pad + SHADOW_OFFSET) as usize..(pad + SHADOW_OFFSET + image.height).min(height) as usize {
            mask[y * w + pad as usize..y * w + (pad + image.width) as usize].fill(1.0);
        }
        // Three box blurs approximate a Gaussian.
        for _ in 0..3 {
            box_blur(&mut mask, w, h, SHADOW_BLUR);
        }
        for (pixel, alpha) in pixels.chunks_exact_mut(4).zip(&mask) {
            pixel[3] = (alpha * SHADOW_ALPHA * 255.0).round() as u8;
        }
    }
    let row = image.width as usize * 4;
    for y in 0..image.height as usize {
        let target = ((y + pad as usize) * w + pad as usize) * 4;
        let source = &image.pixels[y * row..(y + 1) * row];
        for (out, pixel) in pixels[target..target + row].chunks_exact_mut(4).zip(source.chunks_exact(4)) {
            out.copy_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
    }
    if border {
        // A dark hairline at 22% keeps light and dark captures framed alike.
        let blend = |pixel: &mut [u8]| {
            for channel in &mut pixel[..3] {
                *channel = (f32::from(*channel) * 0.78).round() as u8;
            }
        };
        let (left, top) = (pad as usize, pad as usize);
        let (right, bottom) = (left + image.width as usize - 1, top + image.height as usize - 1);
        for x in left..=right {
            for y in [top, bottom] {
                blend(&mut pixels[(y * w + x) * 4..(y * w + x) * 4 + 4]);
            }
        }
        for y in top + 1..bottom {
            for x in [left, right] {
                blend(&mut pixels[(y * w + x) * 4..(y * w + x) * 4 + 4]);
            }
        }
    }
    Image { width, height, pixels }
}

fn box_blur(values: &mut [f32], width: usize, height: usize, radius: usize) {
    let mut line = Vec::new();
    let mut pass = |values: &mut [f32], count: usize, length: usize, index: &dyn Fn(usize, usize) -> usize| {
        let span = (radius * 2 + 1) as f32;
        for row in 0..count {
            line.clear();
            line.extend((0..length).map(|i| values[index(row, i)]));
            let mut sum: f32 = line.iter().take(radius + 1).sum();
            for i in 0..length {
                values[index(row, i)] = sum / span;
                if i + radius + 1 < length {
                    sum += line[i + radius + 1];
                }
                if i >= radius {
                    sum -= line[i - radius];
                }
            }
        }
    };
    pass(values, height, width, &|row, i| row * width + i);
    pass(values, width, height, &|column, i| i * width + column);
}

// ---------------------------------------------------------------------------
// The window.

const CLASS: *const u16 = w!("Snapshot.OutputSettings");
const WIDTH: i32 = 520;
const PAD: i32 = 24;
const HEADER: i32 = 52;
const FIELD: i32 = 38;
const FIELD_LABEL: i32 = 84;
const BROWSE: i32 = 72;
const ROW: i32 = 46;
const FOOTER: i32 = 60;
const GAP: i32 = 12;
const FOLDER_EDIT: usize = 100;
const TEMPLATE_EDIT: usize = 101;
const EM_SETSEL: u32 = 0x00b1;
const EN_SETFOCUS: u32 = 0x0100;
const EN_KILLFOCUS: u32 = 0x0200;
const EN_CHANGE: u32 = 0x0300;
const SWITCHES: [(&str, &str); 3] = [
    ("自动保存", "每次复制、贴图或保存，都在上面的位置存一份"),
    ("阴影", "保存的图片四周加一圈柔和的投影（透明背景）"),
    ("边框", "保存的图片加一像素的深色细边"),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Close,
    Browse,
    Switch(usize),
    Cancel,
    Save,
}

struct State {
    folder: HWND,
    template: HWND,
    switches: [bool; 3],
    hovered: Option<Target>,
    pressed: Option<Target>,
    focused: Option<HWND>,
    error: Option<String>,
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
    folder: RECT,
    browse: RECT,
    template: RECT,
    preview: RECT,
    switches: [RECT; 3],
    toggles: [RECT; 3],
    error: RECT,
    footer: i32,
    cancel: RECT,
    save: RECT,
}

impl Layout {
    unsafe fn new(state: &State) -> Self {
        let scale = state.scale;
        let s = |value: i32| (value as f32 * scale).round() as i32;
        let inner = s(WIDTH - PAD * 2);
        let error_height = match &state.error {
            Some(error) => {
                let dc = GetDC(null_mut());
                let height = ui::wrapped_height(dc, error, state.fonts.small, inner, DT_WORDBREAK | DT_EDITCONTROL);
                ReleaseDC(null_mut(), dc);
                height + s(10)
            }
            None => 0,
        };
        let close = RECT { left: s(WIDTH - 12 - 36), top: s(10), right: s(WIDTH - 12), bottom: s(46) };
        let mut y = s(HEADER + 4);
        let folder = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD - BROWSE - 8), bottom: y + s(FIELD) };
        let browse = RECT { left: s(WIDTH - PAD - BROWSE), top: y, right: s(WIDTH - PAD), bottom: y + s(FIELD) };
        y += s(FIELD + GAP);
        let template = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + s(FIELD) };
        y += s(FIELD + 6);
        let preview = RECT { left: s(PAD + FIELD_LABEL), top: y, right: s(WIDTH - PAD), bottom: y + s(20) };
        y += s(20 + GAP);
        let mut switches = [RECT::default(); 3];
        let mut toggles = [RECT::default(); 3];
        for index in 0..3 {
            switches[index] = RECT { left: s(PAD), top: y, right: s(WIDTH - PAD), bottom: y + s(ROW) };
            let top = y + s((ROW - 24) / 2);
            toggles[index] = RECT { left: s(WIDTH - PAD - 44), top, right: s(WIDTH - PAD), bottom: top + s(24) };
            y += s(ROW);
        }
        let error = RECT { left: s(PAD), top: y + s(8), right: s(WIDTH - PAD), bottom: y + error_height };
        y += error_height + s(GAP);
        let footer = y;
        let button_top = y + s((FOOTER - 34) / 2);
        let save = RECT { left: s(WIDTH - PAD - 84), top: button_top, right: s(WIDTH - PAD), bottom: button_top + s(34) };
        let cancel = RECT {
            left: s(WIDTH - PAD - 84 * 2 - 10),
            top: button_top,
            right: s(WIDTH - PAD - 84 - 10),
            bottom: button_top + s(34),
        };
        Self {
            scale,
            width: s(WIDTH),
            height: y + s(FOOTER),
            close,
            folder,
            browse,
            template,
            preview,
            switches,
            toggles,
            error,
            footer,
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

    fn targets(&self) -> impl Iterator<Item = (Target, RECT)> + '_ {
        (0..3)
            .map(|index| (Target::Switch(index), self.switches[index]))
            .chain([
                (Target::Close, self.close),
                (Target::Browse, self.browse),
                (Target::Cancel, self.cancel),
                (Target::Save, self.save),
            ])
    }

    fn hit(&self, point: POINT) -> Option<Target> {
        self.targets().find(|(_, rect)| contains(*rect, point)).map(|(target, _)| target)
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

/// Opens the window near `near`, or brings it forward if it is open.
pub unsafe fn open(near: HWND) -> io::Result<()> {
    let existing = WINDOW.get();
    if !existing.is_null() {
        SetForegroundWindow(existing);
        return Ok(());
    }
    let config = OutputConfig::load().map_err(io::Error::other)?;
    let window = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_CONTROLPARENT,
        CLASS,
        w!("just-snapshot 保存设置"),
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
    let edit = |id: usize, value: &str| {
        let edit = CreateWindowExW(
            0,
            w!("EDIT"),
            wide(value).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            0,
            0,
            0,
            0,
            window,
            id as _,
            GetModuleHandleW(null()),
            null(),
        );
        edit
    };
    let folder = edit(FOLDER_EDIT, &config.folder().to_string_lossy());
    let template = edit(TEMPLATE_EDIT, &config.template);
    STATE.set(Some(State {
        folder,
        template,
        switches: [config.auto_save, config.shadow, config.border],
        hovered: None,
        pressed: None,
        focused: None,
        error: None,
        scale,
        fonts: Fonts::new(scale),
        brush: CreateSolidBrush(theme::palette().card),
        tracking: false,
    }));
    WINDOW.set(window);
    place(window, near);
    ui::round_corners(window);
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    SetFocus(template);
    SendMessageW(template, EM_SETSEL, 0, -1);
    Ok(())
}

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
        Some([
            (state.folder, layout.edit_rect(layout.folder, metrics.tmHeight)),
            (state.template, layout.edit_rect(layout.template, metrics.tmHeight)),
        ]
        .map(|(edit, rect)| (edit, state.fonts.body, rect)))
    });
    for (edit, font, rect) in boxes.into_iter().flatten() {
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

unsafe fn layout() -> Option<Layout> {
    STATE.with_borrow(|state| state.as_ref().map(|state| Layout::new(state)))
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

/// The config as typed so far.
unsafe fn form() -> Option<OutputConfig> {
    let (folder, template, switches) = STATE.with_borrow(|state| {
        let state = state.as_ref()?;
        Some((state.folder, state.template, state.switches))
    })?;
    let template = text_of(template);
    Some(OutputConfig {
        folder: text_of(folder),
        template: if template.is_empty() { DEFAULT_TEMPLATE.into() } else { template },
        auto_save: switches[0],
        shadow: switches[1],
        border: switches[2],
    })
}

unsafe fn set_error(window: HWND, error: Option<String>) {
    with_state(|state| state.error = error);
    place(window, null_mut());
}

unsafe fn save(window: HWND) {
    let Some(config) = form() else { return };
    let folder = config.folder();
    if let Err(error) = fs::create_dir_all(&folder) {
        set_error(window, Some(format!("无法使用这个保存位置：{}\n{error}", folder.display())));
        return;
    }
    match config.save() {
        Ok(()) => {
            DestroyWindow(window);
        }
        Err(error) => set_error(window, Some(error)),
    }
}

/// The folder picker, starting at `current`.
pub unsafe fn pick_folder(owner: HWND, current: &str) -> Option<String> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::HWND as Hwnd,
            System::{
                Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER},
                Ole::OleInitialize,
            },
            UI::Shell::{
                FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM,
                FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
            },
        },
    };
    let _ = OleInitialize(None);
    let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    let options = dialog.GetOptions().ok()?;
    dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
    let start = wide(current);
    if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(start.as_ptr()), None) {
        let _ = dialog.SetFolder(&item);
    }
    dialog.Show(Some(Hwnd(owner as _))).ok()?;
    let name = dialog.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let text = name.to_string().ok();
    CoTaskMemFree(Some(name.0 as _));
    text
}

unsafe fn activate(window: HWND, target: Target) {
    match target {
        Target::Close | Target::Cancel => {
            DestroyWindow(window);
        }
        Target::Browse => {
            let Some(edit) = STATE.with_borrow(|state| state.as_ref().map(|state| state.folder)) else { return };
            if let Some(folder) = pick_folder(window, &text_of(edit)) {
                SetWindowTextW(edit, wide(&folder).as_ptr());
            }
        }
        Target::Switch(index) => {
            with_state(|state| state.switches[index] = !state.switches[index]);
            InvalidateRect(window, null(), 0);
        }
        Target::Save => save(window),
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
                1 => save(window),
                2 => {
                    DestroyWindow(window);
                }
                FOLDER_EDIT | TEMPLATE_EDIT => match code {
                    EN_SETFOCUS => {
                        with_state(|state| state.focused = Some(lparam as HWND));
                        InvalidateRect(window, null(), 0);
                    }
                    EN_KILLFOCUS => {
                        with_state(|state| state.focused = None);
                        InvalidateRect(window, null(), 0);
                    }
                    // The preview line follows the template.
                    EN_CHANGE => {
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
    let template = STATE.with_borrow(|state| state.as_ref().map(|state| text_of(state.template)));
    STATE.with_borrow(|state| {
        if let Some(state) = state.as_ref() {
            paint(buffer.dc, client, state, template.as_deref().unwrap_or(DEFAULT_TEMPLATE));
        }
    });
    BitBlt(target, 0, 0, client.right, client.bottom, buffer.dc, 0, 0, SRCCOPY);
}

unsafe fn paint(dc: HDC, client: RECT, state: &State, template: &str) {
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
    }
    ui::text(
        dc,
        RECT { left: s(PAD), top: 0, right: layout.close.left, bottom: s(HEADER) },
        "保存设置",
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

    for (row, label, edit) in [(layout.folder, "保存位置", state.folder), (layout.template, "文件名", state.template)] {
        ui::text(dc, RECT { right: row.left + s(FIELD_LABEL - 8), ..row }, label, fonts.body, palette.secondary, DT_LEFT | single);
        let field = RECT { left: row.left + s(FIELD_LABEL), ..row };
        let focused = state.focused == Some(edit);
        if let Some(graphics) = Graphics::new(dc) {
            graphics.fill_round(ui::to_f(field), radius, argb(palette.card, 255));
            let (border, width) = if focused { (palette.accent, 1.5) } else { (palette.key_border, 1.0) };
            graphics.stroke_round(ui::inset(ui::to_f(field), width / 2.0), radius, argb(border, 255), width);
        }
    }
    ui::button(dc, layout.browse, "选择…", fonts.body, palette, false, face(Target::Browse, palette.card), radius);
    let mut time = SYSTEMTIME::default();
    GetLocalTime(&mut time);
    let example = expand(template, &time, 1280, 720);
    ui::text(
        dc,
        layout.preview,
        &format!("示例：{example}　可用 {{date}} {{time}} {{w}} {{h}}"),
        fonts.small,
        palette.tertiary,
        DT_LEFT | single | DT_END_ELLIPSIS,
    );

    for (index, (title, description)) in SWITCHES.into_iter().enumerate() {
        let row = layout.switches[index];
        ui::fill(dc, RECT { bottom: row.top + 1, ..row }, palette.divider);
        let middle = (row.top + row.bottom) / 2;
        let text = RECT { right: layout.toggles[index].left - s(12), ..row };
        ui::text(dc, RECT { top: middle - s(20), bottom: middle, ..text }, title, fonts.body, palette.text, DT_LEFT | DT_BOTTOM | DT_SINGLELINE);
        ui::text(
            dc,
            RECT { top: middle + s(1), bottom: middle + s(20), ..text },
            description,
            fonts.small,
            palette.secondary,
            DT_LEFT | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        let on = state.switches[index];
        let hover = if state.hovered == Some(Target::Switch(index)) { 1.0 } else { 0.0 };
        if let Some(graphics) = Graphics::new(dc) {
            let bounds = ui::to_f(layout.toggles[index]);
            let height = bounds[3] - bounds[1];
            let off = mix(palette.track_off, palette.secondary, hover * 0.3);
            let track = if on { mix(palette.accent, palette.accent_hover, hover) } else { off };
            graphics.fill_round(bounds, height / 2.0, argb(track, 255));
            let margin = 3.0 * layout.scale;
            let knob = height - margin * 2.0;
            let left = if on { bounds[2] - margin - knob } else { bounds[0] + margin };
            let top = bounds[1] + margin;
            graphics.fill_ellipse([left, top, left + knob, top + knob], argb(0x00ff_ffff, 255));
        }
    }
    if let Some(error) = &state.error {
        ui::text(dc, layout.error, error, fonts.small, palette.error, DT_LEFT | DT_TOP | DT_WORDBREAK | DT_EDITCONTROL);
    }

    ui::fill(
        dc,
        RECT { left: 1, top: layout.footer, right: layout.width - 1, bottom: layout.footer + 1 },
        palette.card_border,
    );
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
    fn templates_fill_in_the_time_and_size_and_stay_valid_names() {
        let time = SYSTEMTIME { wYear: 2026, wMonth: 11, wDay: 20, wHour: 9, wMinute: 5, wSecond: 7, ..Default::default() };
        assert_eq!(expand(DEFAULT_TEMPLATE, &time, 800, 600), "just-snapshot 2026-11-20 090507.png");
        assert_eq!(expand("{w}x{h}", &time, 800, 600), "800x600.png");
        assert_eq!(expand("bug: a/b?.png", &time, 1, 1), "bug ab.png");
        assert_eq!(expand("  ...  ", &time, 1, 1), "just-snapshot.png");
    }

    #[test]
    fn taken_names_get_a_number() {
        let folder = env::temp_dir().join(format!("snapshot-output-test-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        assert_eq!(unique_path(&folder, "a.png"), folder.join("a.png"));
        fs::write(folder.join("a.png"), b"x").unwrap();
        fs::write(folder.join("a (2).png"), b"x").unwrap();
        assert_eq!(unique_path(&folder, "a.png"), folder.join("a (3).png"));
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn settings_round_trip() {
        let folder = env::temp_dir().join(format!("snapshot-output-config-{}", std::process::id()));
        let path = folder.join("output.txt");
        assert_eq!(OutputConfig::load_from(&path).unwrap(), OutputConfig::default());
        let config = OutputConfig {
            folder: r"D:\截图".into(),
            template: "{date} 需求".into(),
            auto_save: true,
            shadow: true,
            border: false,
        };
        config.save_to(&path).unwrap();
        assert_eq!(OutputConfig::load_from(&path).unwrap(), config);
        fs::remove_dir_all(&folder).unwrap();
    }

    /// The window as drawn (native edit boxes do not render in an SSH
    /// session). Run with `<test binary> output_settings --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        unsafe {
            register().ok();
            open(null_mut()).unwrap();
            let window = WINDOW.get();
            with_state(|state| {
                state.switches = [true, true, false];
                state.hovered = Some(Target::Browse);
            });
            let mut client = RECT::default();
            GetClientRect(window, &mut client);
            let surface = crate::capture::Surface::new(null_mut(), client.right, client.bottom).unwrap();
            STATE.with_borrow(|state| paint(surface.dc, client, state.as_ref().unwrap(), DEFAULT_TEMPLATE));
            GdiFlush();
            let mut image = surface.image(client.right, client.bottom).unwrap();
            for pixel in image.pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            let folder = env::temp_dir().join("snapshot-preview");
            fs::create_dir_all(&folder).unwrap();
            image.save_png(&folder.join("output-settings.png")).unwrap();
            let decorated = decorate(&Image { width: 160, height: 90, pixels: vec![235; 160 * 90 * 4] }, true, true);
            decorated.save_png_alpha(&folder.join("output-shadow.png")).unwrap();
            DestroyWindow(window);
        }
    }

    #[test]
    fn shadow_pads_the_image_and_fades_out() {
        let image = Image { width: 40, height: 30, pixels: vec![200; 40 * 30 * 4] };
        let decorated = decorate(&image, true, true);
        assert_eq!((decorated.width, decorated.height), (40 + SHADOW_PAD * 2, 30 + SHADOW_PAD * 2));
        let alpha = |x: i32, y: i32| decorated.pixels[((y * decorated.width + x) * 4 + 3) as usize];
        assert_eq!(alpha(0, 0), 0, "the corner is transparent");
        assert_eq!(alpha(SHADOW_PAD + 20, SHADOW_PAD + 15), 255, "the image is opaque");
        let below = alpha(SHADOW_PAD + 20, SHADOW_PAD + 30 + 3);
        assert!(below > 20 && below < 255, "the shadow shows under the image: {below}");
        // The border darkens the outermost pixels of the image.
        let edge = ((SHADOW_PAD * decorated.width + SHADOW_PAD + 10) * 4) as usize;
        assert!(decorated.pixels[edge] < 200);
        let plain = decorate(&image, false, false);
        assert_eq!(plain.pixels[..4], [200, 200, 200, 255]);
    }
}
