use crate::{
    capture::{Image, Surface},
    hotkey::DEFAULT_OPACITY_STEP,
    codes, ocr,
    output::copy_text,
    pin_text::{Status, TextLayer, View},
    pin_translate::{self, Phase, Target as PaneTarget, TranslatePane},
    selection_geometry::{contains, mouse_point},
    show_error,
    theme::{self, mix},
    toolbar::paint_size_badge,
    translate::{self, Service, LANGUAGES},
    translate_settings,
    ui::{self, Fonts},
};
use std::{
    cell::{Cell, RefCell},
    io,
    mem::size_of,
    ptr::{null, null_mut},
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::WM_MOUSELEAVE,
            HiDpi::GetDpiForWindow,
            Shell::ShellExecuteW,
            Input::KeyboardAndMouse::{
                GetDoubleClickTime, GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE,
                TME_NONCLIENT, TRACKMOUSEEVENT, VK_CONTROL, VK_ESCAPE,
            },
            WindowsAndMessaging::*,
        },
    },
};

const CLASS: *const u16 = w!("Snapshot.Pinned");
const TOGGLE_ORIGINAL: &str = "显示原文";
const TOGGLE_TRANSLATION: &str = "显示译文";
const POPUP_CLASS: *const u16 = w!("Snapshot.Translation");
/// Gap between a pin and its translation popup.
const POPUP_GAP: i32 = 8;
const GLOW: i32 = 5;
const BADGE_TIMER: usize = 1;
const BADGE_DURATION_MS: u32 = 1200;
const MIN_OPACITY: i32 = 15;
const WHEEL_NOTCH: i32 = 120;
const TOAST_TIMER: usize = 2;
const TOAST_MS: u32 = 2400;
/// Posted by the recognition thread; `lparam` owns a `Box<Recognized>`.
const OCR_DONE: u32 = WM_APP + 1;
/// Posted by the translation thread; `wparam` is the request id and `lparam`
/// owns a `Box<Outcome>`.
const TRANSLATE_DONE: u32 = WM_APP + 2;
/// The translation settings changed; pins waiting for them try again.
const RETRANSLATE: u32 = WM_APP + 3;
/// Posted by the image search thread; `lparam` owns a `Box<image_search::Outcome>`.
const SEARCH_DONE: u32 = WM_APP + 4;
const SHIMMER_TIMER: usize = 3;
const SHIMMER_FRAME_MS: u32 = 33;
const WHEEL_SCROLL: i32 = 48;
const WM_NCMOUSELEAVE: u32 = 0x02a2;

thread_local! {
    static OPACITY_STEP: Cell<u8> = const { Cell::new(DEFAULT_OPACITY_STEP) };
    /// The group new pins join: the one last picked in a pin's menu.
    static CURRENT_GROUP: Cell<u32> = const { Cell::new(1) };
    /// Groups hidden from the tray or a pin's menu.
    static HIDDEN_GROUPS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

const MENU_COPY: usize = 1;
const MENU_SAVE: usize = 2;
const MENU_TOPMOST: usize = 3;
const MENU_NEW_GROUP: usize = 4;
const MENU_HIDE_GROUP: usize = 5;
const MENU_CLOSE: usize = 6;
const MENU_ABOVE: usize = 7;
const MENU_HIDE: usize = 8;
const MENU_DESELECT: usize = 9;
/// "Move to group N" items start here.
const MENU_GROUP: usize = 100;
/// The image search engines, past any group number.
const MENU_SEARCH: usize = 0x8000;

pub fn opacity_step() -> u8 {
    OPACITY_STEP.get()
}

pub fn set_opacity_step(step: u8) {
    OPACITY_STEP.set(step);
}

/// What a pin does with its image besides showing it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Plain,
    /// Recognize the text so it can be selected and copied.
    Text,
    /// Recognize the text and translate it in a panel beside the image.
    Translate,
}

/// Text lines and QR codes / barcodes found in a pin.
type Recognized = Result<(Vec<ocr::Line>, Vec<codes::Code>), String>;

enum Outcome {
    Done(String),
    Unconfigured,
    Failed(String),
}

#[derive(Clone, Copy)]
enum Badge {
    Zoom,
    Opacity,
    /// A short confirmation such as "已复制图片", for pins without a text layer.
    Note,
}

struct Pin {
    surface: Surface,
    width: i32,
    height: i32,
    zoom: f64,
    opacity: i32,
    opacity_wheel_remainder: i32,
    badge: Option<Badge>,
    /// Present on pins made with "extract text" or "translate".
    text: Option<TextLayer>,
    /// Present on pins made with "translate".
    translate: Option<TranslatePane>,
    /// The small window beside the pin that shows the translation.
    popup: HWND,
    /// Kept for recognizing again after a failure.
    source: Option<Image>,
    /// Whether the current text drag has moved past its first word.
    dragged: bool,
    scale: f32,
    fonts: Fonts,
    /// Pins are shown and hidden by group from the tray.
    group: u32,
    topmost: bool,
    /// Kept above the other pins even when they are clicked.
    above: bool,
    /// Picked with Ctrl+click; dragging, the wheel, Esc and the menu act on
    /// every selected pin together.
    selected: bool,
    /// Hidden on its own (not with its group) until the tray shows pins again.
    hidden: bool,
    /// When and where the last double-click was, so a third click selects the line.
    double_click: Option<(u32, POINT)>,
    note: String,
    /// The image with the translation drawn over the original lines.
    translated: Option<Surface>,
    /// Showing the original even though a translation is ready.
    show_original: bool,
}

impl Pin {
    fn s(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    /// The image size at the current zoom.
    fn display_size(&self) -> (i32, i32) {
        let size = |value: i32| (f64::from(value) * self.zoom).round().max(1.0) as i32;
        (size(self.width), size(self.height))
    }

    /// The whole window: the image and the glow.
    fn window_size(&self) -> (i32, i32) {
        let (width, height) = self.display_size();
        (width + GLOW * 2, height + GLOW * 2)
    }

    /// The image area inside the glow, in client pixels.
    fn image_rect(&self) -> RECT {
        let (width, height) = self.display_size();
        RECT {
            left: GLOW,
            top: GLOW,
            right: GLOW + width,
            bottom: GLOW + height,
        }
    }

    /// The translation popup's layout, in its client pixels.
    unsafe fn pane_layout(&self) -> Option<pin_translate::Layout> {
        let pane = self.translate.as_ref()?;
        if self.popup.is_null() {
            return None;
        }
        let mut client = RECT::default();
        GetClientRect(self.popup, &mut client);
        let dc = GetDC(self.popup);
        let layout = pane.layout(client, self.scale, dc, &self.fonts);
        ReleaseDC(self.popup, dc);
        Some(layout)
    }

    unsafe fn pane_hit(&self, point: POINT) -> Option<PaneTarget> {
        let layout = self.pane_layout()?;
        self.translate.as_ref()?.hit(&layout, point)
    }

    unsafe fn view(&self, _window: HWND) -> View {
        let image = self.image_rect();
        View {
            origin: POINT {
                x: image.left,
                y: image.top,
            },
            zoom: (image.right - image.left) as f32 / self.width as f32,
            scale: self.scale,
        }
    }

    /// Whether the translation covers the original right now.
    fn showing_translation(&self) -> bool {
        self.translated.is_some() && !self.show_original
    }

    /// The 显示原文 / 显示译文 switch in the top-left corner of the image.
    unsafe fn toggle_button(&self, dc: HDC) -> Option<RECT> {
        self.translated.as_ref()?;
        let image = self.image_rect();
        let width = ui::measure(dc, TOGGLE_ORIGINAL, self.fonts.small) + self.s(20);
        let (height, margin) = (self.s(26), self.s(6));
        let rect = RECT {
            left: image.left + margin,
            top: image.top + margin,
            right: image.left + margin + width,
            bottom: image.top + margin + height,
        };
        (rect.right <= image.right && rect.bottom <= image.bottom).then_some(rect)
    }

    unsafe fn over_toggle(&self, window: HWND, point: POINT) -> bool {
        let dc = GetDC(window);
        let button = self.toggle_button(dc);
        ReleaseDC(window, dc);
        button.is_some_and(|button| contains(button, point))
    }

    unsafe fn over_button(&self, window: HWND, point: POINT) -> bool {
        let Some(text) = &self.text else { return false };
        let dc = GetDC(window);
        let button = text.button(self.view(window), self.image_rect(), &self.fonts, dc);
        ReleaseDC(window, dc);
        button.is_some_and(|button| contains(button, point))
    }
}

pub unsafe fn register(instance: HINSTANCE) -> io::Result<()> {
    let class = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hCursor: LoadCursorW(null_mut(), IDC_SIZEALL),
        lpszClassName: CLASS,
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        return Err(io::Error::last_os_error());
    }
    let popup = WNDCLASSW {
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(popup_proc),
        hInstance: instance,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        lpszClassName: POPUP_CLASS,
        ..Default::default()
    };
    if RegisterClassW(&popup) == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Pins `image` at `bounds`. Text and translate pins recognize the text in
/// the background; translate pins then show the translation in a popup beside it.
pub unsafe fn open(image: Image, bounds: RECT, mode: Mode) -> io::Result<()> {
    let source = (mode != Mode::Plain).then(|| image.clone());
    let (surface, width, height) = image.into_surface()?;
    let window = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
        CLASS,
        w!("just-snapshot 贴图"),
        WS_POPUP,
        bounds.left - GLOW,
        bounds.top - GLOW,
        width + GLOW * 2,
        height + GLOW * 2,
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
    let pin = Box::new(Pin {
        surface,
        width,
        height,
        zoom: 1.0,
        opacity: 100,
        opacity_wheel_remainder: 0,
        badge: None,
        text: source.is_some().then(TextLayer::new),
        translate: (mode == Mode::Translate).then(TranslatePane::new),
        popup: null_mut(),
        source,
        dragged: false,
        scale,
        fonts: Fonts::new(scale),
        group: CURRENT_GROUP.get(),
        topmost: true,
        above: false,
        selected: false,
        hidden: false,
        double_click: None,
        note: String::new(),
        translated: None,
        show_original: false,
    });
    // A new pin in a hidden group brings the group back.
    let group = pin.group;
    let regrouped = HIDDEN_GROUPS.with_borrow_mut(|hidden| {
        let before = hidden.len();
        hidden.retain(|&other| other != group);
        hidden.len() != before
    });
    let recognize = pin.source.is_some();
    let pin = Box::into_raw(pin);
    SetWindowLongPtrW(window, GWLP_USERDATA, pin as isize);
    if SetLayeredWindowAttributes(window, 0, 255, LWA_ALPHA) == 0 {
        let error = io::Error::last_os_error();
        DestroyWindow(window);
        return Err(error);
    }
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    if regrouped {
        // The rest of the group comes back with it.
        apply_visibility();
    }
    if (*pin).translate.is_some() {
        open_popup(window, &mut *pin);
    }
    if recognize {
        start_recognition(window, &mut *pin);
    }
    // A new pin starts on top; pins kept above the others go back over it.
    raise_above_pins();
    Ok(())
}

/// A millisecond clock for animations; only differences matter.
fn now_ms() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_millis() as u32)
}

/// Every pin window with its state.
unsafe fn pins() -> Vec<(HWND, *mut Pin)> {
    let mut found = Vec::new();
    let mut window = FindWindowExW(null_mut(), null_mut(), CLASS, null());
    while !window.is_null() {
        let pin = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Pin;
        if !pin.is_null() {
            found.push((window, pin));
        }
        window = FindWindowExW(null_mut(), window, CLASS, null());
    }
    found
}

/// Each group with its pin count and whether it is shown, in order.
pub unsafe fn groups() -> Vec<(u32, usize, bool)> {
    let mut groups: Vec<(u32, usize, bool)> = Vec::new();
    for (_, pin) in pins() {
        let group = (*pin).group;
        match groups.iter_mut().find(|(other, ..)| *other == group) {
            Some(entry) => entry.1 += 1,
            None => groups.push((group, 1, !is_hidden(group))),
        }
    }
    groups.sort_by_key(|&(group, ..)| group);
    groups
}

fn is_hidden(group: u32) -> bool {
    HIDDEN_GROUPS.with_borrow(|hidden| hidden.contains(&group))
}

/// Shows or hides every pin, with its translation popup.
unsafe fn apply_visibility() {
    for (window, pin) in pins() {
        let show = if is_hidden((*pin).group) || (*pin).hidden { SW_HIDE } else { SW_SHOWNOACTIVATE };
        ShowWindow(window, show);
        if !(*pin).popup.is_null() {
            ShowWindow((*pin).popup, show);
        }
    }
}

pub unsafe fn set_all_hidden(hidden: bool) {
    let all: Vec<u32> = if hidden { groups().into_iter().map(|(group, ..)| group).collect() } else { Vec::new() };
    if !hidden {
        // "全部显示" also brings back pins hidden on their own.
        for (_, pin) in pins() {
            (*pin).hidden = false;
        }
    }
    HIDDEN_GROUPS.set(all);
    apply_visibility();
}

/// Shows a hidden group or hides a shown one. Showing a group also brings
/// back its pins that were hidden on their own.
pub unsafe fn toggle_group(group: u32) {
    let shown = HIDDEN_GROUPS.with_borrow_mut(|hidden| {
        if let Some(index) = hidden.iter().position(|&other| other == group) {
            hidden.remove(index);
            true
        } else {
            hidden.push(group);
            false
        }
    });
    if shown {
        for (_, pin) in pins().into_iter().filter(|&(_, pin)| (*pin).group == group) {
            (*pin).hidden = false;
        }
    }
    apply_visibility();
}

/// The pins an action on `window` applies to: every selected pin when it is
/// one of them, else just it.
unsafe fn targets(window: HWND) -> Vec<(HWND, *mut Pin)> {
    let all = pins();
    let in_selection = all.iter().any(|&(other, pin)| other == window && (*pin).selected);
    all.into_iter()
        .filter(|&(other, pin)| if in_selection { (*pin).selected } else { other == window })
        .collect()
}

unsafe fn set_selected(window: HWND, pin: &mut Pin, selected: bool) {
    if pin.selected != selected {
        pin.selected = selected;
        InvalidateRect(window, null(), 0);
    }
}

/// Ctrl+click: adds the pin to the selection or takes it out.
unsafe fn toggle_selected(window: HWND, pin: &mut Pin) {
    set_selected(window, pin, !pin.selected);
}

/// A plain click on a pin outside the selection ends the selection.
unsafe fn click_without_ctrl(window: HWND, pin: &Pin) {
    if !pin.selected {
        for (other, other_pin) in pins() {
            if other != window {
                set_selected(other, &mut *other_pin, false);
            }
        }
    }
}

/// A press soon after a double-click, close to it, is the third click.
unsafe fn is_triple_click(pin: &mut Pin, point: POINT) -> bool {
    let Some((time, at)) = pin.double_click.take() else { return false };
    let soon = (GetMessageTime() as u32).wrapping_sub(time) <= GetDoubleClickTime();
    let near = (point.x - at.x).abs() <= GetSystemMetrics(SM_CXDOUBLECLK) && (point.y - at.y).abs() <= GetSystemMetrics(SM_CYDOUBLECLK);
    soon && near && pin.text.is_some()
}

fn ctrl_down() -> bool {
    unsafe { GetKeyState(i32::from(VK_CONTROL)) < 0 }
}

/// Puts the pins kept above the others back on top of the pin stack. A
/// hidden one stays hidden: the z-order changes without showing it.
unsafe fn raise_above_pins() {
    for (window, pin) in pins() {
        if (*pin).above {
            SetWindowPos(window, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

/// Moves the other selected pins along with `window`, which is being dragged to `next`.
unsafe fn drag_selection(window: HWND, next: RECT) {
    let mut current = RECT::default();
    GetWindowRect(window, &mut current);
    let (dx, dy) = (next.left - current.left, next.top - current.top);
    if dx == 0 && dy == 0 {
        return;
    }
    for (other, _) in targets(window).into_iter().filter(|&(other, _)| other != window) {
        let mut bounds = RECT::default();
        GetWindowRect(other, &mut bounds);
        SetWindowPos(other, null_mut(), bounds.left + dx, bounds.top + dy, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// Zooms the pin by `factor`, keeping its top-left corner.
unsafe fn zoom_by(window: HWND, pin: &mut Pin, factor: f64) {
    pin.zoom = (pin.zoom * factor).clamp(0.15, 4.0);
    let (width, height) = pin.window_size();
    SetWindowPos(window, null_mut(), 0, 0, width, height, SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER);
    show_badge(window, pin, Badge::Zoom);
}

unsafe fn change_opacity(window: HWND, pin: &mut Pin, notches: i32) {
    let opacity = (pin.opacity + notches * i32::from(opacity_step())).clamp(MIN_OPACITY, 100);
    let alpha = ((opacity * 255 + 50) / 100) as u8;
    if SetLayeredWindowAttributes(window, 0, alpha, LWA_ALPHA) != 0 {
        pin.opacity = opacity;
    }
    show_badge(window, pin, Badge::Opacity);
}

unsafe fn show_badge(window: HWND, pin: &mut Pin, badge: Badge) {
    if SetTimer(window, BADGE_TIMER, BADGE_DURATION_MS, None) != 0 {
        pin.badge = Some(badge);
    }
    InvalidateRect(window, null(), 0);
}

pub unsafe fn close_all() {
    for (window, _) in pins() {
        DestroyWindow(window);
    }
}

/// Keeps a pin above other windows, or lets them cover it.
unsafe fn set_topmost(window: HWND, pin: &mut Pin, topmost: bool) {
    pin.topmost = topmost;
    if !topmost {
        pin.above = false;
    }
    let after = if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
    SetWindowPos(window, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    if !pin.popup.is_null() {
        SetWindowPos(pin.popup, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// The pin's image as shown, at its original size.
unsafe fn pin_image(pin: &Pin) -> io::Result<Image> {
    pin.surface.image(pin.width, pin.height)
}

unsafe fn copy_image(window: HWND, pin: &mut Pin) {
    let result = pin_image(pin)
        .map_err(|error| error.to_string())
        .and_then(|image| crate::output::copy_image(window, &image));
    match result {
        Ok(()) => toast(window, pin, "已复制图片".into()),
        Err(error) => show_error(window, &format!("无法复制图片。\n{error}")),
    }
}

unsafe fn save_image(window: HWND, pin: &mut Pin) {
    let result = pin_image(pin)
        .map_err(|error| format!("无法保存图片。\n{error}"))
        .and_then(|image| crate::output::deliver(window, &image, crate::output::Target::Save));
    match result {
        Ok(true) => toast(window, pin, "已保存".into()),
        Ok(false) => {}
        Err(error) => show_error(window, &error),
    }
}

/// The right-click menu: copy, save, image search, stay on top, groups and
/// close. With several pins selected, the window and group items act on all of them.
unsafe fn context_menu(window: HWND, pin: &mut Pin) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let targets = targets(window);
    let count = targets.len();
    let several = |single: &str, many: &str| crate::wide(&if count > 1 { many.replace("{n}", &count.to_string()) } else { single.to_string() });
    AppendMenuW(menu, MF_STRING, MENU_COPY, w!("复制图片\tCtrl+C"));
    AppendMenuW(menu, MF_STRING, MENU_SAVE, w!("保存…"));
    // A settings file that cannot be read still leaves the way to fix it.
    let engines = crate::image_search::load().unwrap_or_default();
    let search_menu = CreatePopupMenu();
    if !search_menu.is_null() {
        crate::image_search::append_items(search_menu, MENU_SEARCH, &engines);
        AppendMenuW(menu, MF_POPUP, search_menu as usize, w!("识图"));
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let check = |on: bool| if on { MF_CHECKED } else { 0 };
    let topmost_label = several("置顶", "置顶（选中的 {n} 张）");
    AppendMenuW(menu, MF_STRING | check(pin.topmost), MENU_TOPMOST, topmost_label.as_ptr());
    let above_label = several("在其他贴图上面", "在其他贴图上面（选中的 {n} 张）");
    AppendMenuW(menu, MF_STRING | check(pin.above), MENU_ABOVE, above_label.as_ptr());
    let groups_menu = CreatePopupMenu();
    let groups = groups();
    let newest = groups.iter().map(|&(group, ..)| group).max().unwrap_or(1);
    if !groups_menu.is_null() {
        for (group, count, _) in &groups {
            let label = crate::wide(&format!("组 {group}（{count} 张）"));
            let flags = MF_STRING | if *group == pin.group { MF_CHECKED } else { 0 };
            AppendMenuW(groups_menu, flags, MENU_GROUP + *group as usize, label.as_ptr());
        }
        AppendMenuW(groups_menu, MF_SEPARATOR, 0, null());
        let new_group = several("移到新组", "把选中的 {n} 张放进新组");
        AppendMenuW(groups_menu, MF_STRING, MENU_NEW_GROUP, new_group.as_ptr());
        let label = crate::wide(&if count > 1 { format!("分组（选中的 {count} 张）") } else { format!("分组：组 {}", pin.group) });
        AppendMenuW(menu, MF_POPUP, groups_menu as usize, label.as_ptr());
    }
    let hide_label = several("隐藏这张", "隐藏选中的 {n} 张");
    AppendMenuW(menu, MF_STRING, MENU_HIDE, hide_label.as_ptr());
    AppendMenuW(menu, MF_STRING, MENU_HIDE_GROUP, w!("隐藏本组"));
    if count > 1 {
        AppendMenuW(menu, MF_STRING, MENU_DESELECT, w!("取消选择"));
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let close_label = several("关闭\tEsc", "关闭选中的 {n} 张\tEsc");
    AppendMenuW(menu, MF_STRING, MENU_CLOSE, close_label.as_ptr());
    let mut cursor = POINT::default();
    GetCursorPos(&mut cursor);
    let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, cursor.x, cursor.y, 0, window, null());
    DestroyMenu(menu);
    match chosen as usize {
        MENU_COPY => copy_image(window, pin),
        MENU_SAVE => save_image(window, pin),
        MENU_TOPMOST => {
            let topmost = !pin.topmost;
            for (other, other_pin) in &targets {
                set_topmost(*other, &mut **other_pin, topmost);
            }
        }
        MENU_ABOVE => {
            let above = !pin.above;
            for (other, other_pin) in &targets {
                if above && !(**other_pin).topmost {
                    set_topmost(*other, &mut **other_pin, true);
                }
                (**other_pin).above = above;
            }
            raise_above_pins();
        }
        MENU_HIDE => {
            for (_, other_pin) in &targets {
                (**other_pin).hidden = true;
                (**other_pin).selected = false;
            }
            apply_visibility();
        }
        MENU_DESELECT => {
            for (other, other_pin) in &targets {
                set_selected(*other, &mut **other_pin, false);
            }
        }
        MENU_NEW_GROUP => {
            let group = newest + 1;
            for (_, other_pin) in &targets {
                (**other_pin).group = group;
            }
            CURRENT_GROUP.set(group);
            let moved = if count > 1 { format!("{count} 张已放进组 {group}") } else { format!("已移到组 {group}") };
            toast(window, pin, format!("{moved}，新贴图也会放进这一组"));
        }
        MENU_HIDE_GROUP => toggle_group(pin.group),
        id if (MENU_SEARCH..MENU_SEARCH + crate::image_search::MENU_IDS).contains(&id) => {
            match crate::image_search::picked(id, MENU_SEARCH, engines) {
                Some(crate::image_search::Pick::Engines(engines)) => match pin_image(pin) {
                    Ok(image) => {
                        toast(window, pin, crate::image_search::busy_label(&engines));
                        crate::image_search::start(window, SEARCH_DONE, engines, image);
                    }
                    Err(error) => show_error(window, &format!("无法识图。\n{error}")),
                },
                Some(crate::image_search::Pick::Settings) => {
                    if let Err(error) = crate::image_search_settings::open(window) {
                        show_error(window, &format!("无法打开识图设置。\n{error}"));
                    }
                }
                None => {}
            }
        }
        MENU_CLOSE => {
            for (other, _) in targets {
                DestroyWindow(other);
            }
        }
        id if id > MENU_GROUP => {
            let group = (id - MENU_GROUP) as u32;
            for (_, other_pin) in &targets {
                (**other_pin).group = group;
            }
            CURRENT_GROUP.set(group);
            // Joining a hidden group hides the pins with it.
            apply_visibility();
        }
        _ => {}
    }
}

/// Tells every translate pin that the translation settings changed.
pub unsafe fn translation_settings_changed() {
    let mut window = FindWindowExW(null_mut(), null_mut(), CLASS, null());
    while !window.is_null() {
        PostMessageW(window, RETRANSLATE, 0, 0);
        window = FindWindowExW(null_mut(), window, CLASS, null());
    }
}

/// Owned by the pin, so it stays above it and closes with it.
unsafe fn open_popup(window: HWND, pin: &mut Pin) {
    let popup = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        POPUP_CLASS,
        w!("just-snapshot 翻译"),
        WS_POPUP,
        0,
        0,
        0,
        0,
        window,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if popup.is_null() {
        show_error(window, &format!("无法打开翻译窗口。\n{}", io::Error::last_os_error()));
        return;
    }
    pin.popup = popup;
    ui::round_corners(popup);
    place_popup(window, pin);
    ShowWindow(popup, SW_SHOWNOACTIVATE);
}

/// Sizes the popup to its content and puts it beside the pin: to the right,
/// else to the left, else overlapping inside the screen, top-aligned.
unsafe fn place_popup(window: HWND, pin: &Pin) {
    let Some(pane) = &pin.translate else { return };
    if pin.popup.is_null() {
        return;
    }
    let width = pin.s(pin_translate::WIDTH);
    let dc = GetDC(pin.popup);
    let height = pane.preferred_height(width, pin.scale, dc, &pin.fonts);
    ReleaseDC(pin.popup, dc);
    let mut bounds = RECT::default();
    GetWindowRect(window, &mut bounds);
    let mut monitor = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    GetMonitorInfoW(MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST), &mut monitor);
    let work = monitor.rcWork;
    let gap = pin.s(POPUP_GAP);
    let mut left = bounds.right + gap;
    if left + width > work.right {
        left = bounds.left - gap - width;
    }
    if left < work.left {
        left = (work.right - width).max(work.left);
    }
    let top = bounds.top.min(work.bottom - height).max(work.top);
    SetWindowPos(pin.popup, null_mut(), left, top, width, height, SWP_NOZORDER | SWP_NOACTIVATE);
}

/// After the translation state changes: resize to fit and repaint.
unsafe fn refresh_popup(window: HWND, pin: &Pin) {
    place_popup(window, pin);
    if !pin.popup.is_null() {
        InvalidateRect(pin.popup, null(), 0);
    }
}

unsafe fn start_recognition(window: HWND, pin: &mut Pin) {
    let Some(source) = pin.source.clone() else { return };
    if let Some(text) = &mut pin.text {
        text.status = Status::Recognizing;
    }
    if let Some(pane) = &mut pin.translate {
        pane.phase = Phase::Recognizing;
        SetTimer(window, SHIMMER_TIMER, SHIMMER_FRAME_MS, None);
    }
    InvalidateRect(window, null(), 0);
    refresh_popup(window, pin);
    let target = window as isize;
    std::thread::spawn(move || {
        let recognized: Recognized = ocr::recognize(&source).map(|lines| (lines, codes::detect(&source)));
        let result = Box::into_raw(Box::new(recognized));
        if PostMessageW(target as HWND, OCR_DONE, 0, result as LPARAM) == 0 {
            // The pin was closed before recognition finished.
            drop(Box::from_raw(result));
        }
    });
}

/// Translates the recognized text into the chosen language in the background.
unsafe fn start_translation(window: HWND, pin: &mut Pin) {
    // Back to the original until the new translation arrives.
    if pin.translated.take().is_some() {
        InvalidateRect(window, null(), 0);
    }
    let Some(pane) = &mut pin.translate else { return };
    let lines = pin.text.as_ref().map_or(&[][..], TextLayer::lines);
    pane.scroll = 0;
    if lines.is_empty() {
        pane.phase = Phase::NoText;
        refresh_popup(window, pin);
        return;
    }
    let text = ocr::text(lines);
    let to = pane.language.resolve(&text);
    let config = translate::Config::load();
    pane.request = pane.request.wrapping_add(1);
    pane.phase = Phase::Translating {
        to,
        provider: config.as_ref().ok().and_then(|config| config.provider),
    };
    let request = pane.request;
    SetTimer(window, SHIMMER_TIMER, SHIMMER_FRAME_MS, None);
    refresh_popup(window, pin);
    let target = window as isize;
    std::thread::spawn(move || {
        let outcome = match config.and_then(|config| Service::from_config(&config)) {
            Ok(Some(service)) => match translate::translate(&service, &text, to) {
                Ok(translation) => Outcome::Done(translation),
                Err(error) => Outcome::Failed(error),
            },
            Ok(None) => Outcome::Unconfigured,
            Err(error) => Outcome::Failed(error),
        };
        let outcome = Box::into_raw(Box::new(outcome));
        if PostMessageW(target as HWND, TRANSLATE_DONE, request as WPARAM, outcome as LPARAM) == 0 {
            drop(Box::from_raw(outcome));
        }
    });
}

unsafe fn copy_translation(window: HWND, pin: &mut Pin) {
    let Some(translation) = pin.translate.as_ref().and_then(TranslatePane::translation) else {
        return;
    };
    match copy_text(window, &translation.replace('\n', "\r\n")) {
        Ok(()) => toast(window, pin, "已复制译文".into()),
        Err(error) => show_error(window, &format!("无法复制译文。\n{error}")),
    }
}

unsafe fn open_translation_settings(window: HWND) {
    if let Err(error) = translate_settings::open(window) {
        show_error(window, &format!("无法打开翻译设置。\n{error}"));
    }
}

/// The language picker: a menu under its button.
unsafe fn pick_language(window: HWND, pin: &mut Pin) {
    let Some(layout) = pin.pane_layout() else { return };
    let Some(pane) = &pin.translate else { return };
    let menu = CreatePopupMenu();
    for (index, language) in LANGUAGES.iter().enumerate() {
        let label = match language {
            translate::Language::Auto => "自动（中文译成英文，其他译成中文）".to_string(),
            language => language.label().to_string(),
        };
        let flags = if *language == pane.language { MF_STRING | MF_CHECKED } else { MF_STRING };
        AppendMenuW(menu, flags, index + 1, crate::wide(&label).as_ptr());
    }
    let anchor = layout.language();
    let mut point = POINT {
        x: anchor.left,
        y: anchor.bottom + pin.s(4),
    };
    ClientToScreen(pin.popup, &mut point);
    let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY, point.x, point.y, 0, window, null());
    DestroyMenu(menu);
    let Some(&language) = (chosen as usize).checked_sub(1).and_then(|index| LANGUAGES.get(index)) else {
        return;
    };
    if let Some(pane) = &mut pin.translate {
        pane.language = language;
    }
    start_translation(window, pin);
}

unsafe fn activate_pane(window: HWND, pin: &mut Pin, target: PaneTarget) {
    match target {
        PaneTarget::Language => pick_language(window, pin),
        PaneTarget::Copy => copy_translation(window, pin),
        PaneTarget::Settings | PaneTarget::Secondary => open_translation_settings(window),
        PaneTarget::Primary => {
            let phase = pin.translate.as_ref().map(|pane| &pane.phase);
            if matches!(phase, Some(Phase::Unconfigured)) {
                open_translation_settings(window);
            } else if pin.text.as_ref().is_some_and(|text| matches!(text.status, Status::Failed)) {
                start_recognition(window, pin);
            } else {
                start_translation(window, pin);
            }
        }
    }
}

/// "识别到 3 行文字、1 个二维码，…" after recognition.
fn recognized_message(lines: usize, codes: &[codes::Code]) -> String {
    let mut found = Vec::new();
    if lines > 0 {
        found.push(format!("{lines} 行文字"));
    }
    for kind in ["二维码", "条形码"] {
        let count = codes.iter().filter(|code| code.kind() == kind).count();
        if count > 0 {
            found.push(format!("{count} 个{kind}"));
        }
    }
    if found.is_empty() {
        "没有识别到文字".into()
    } else if lines > 0 {
        format!("识别到 {}，拖动选择，Ctrl+C 复制", found.join("、"))
    } else {
        format!("识别到 {}，单击复制内容", found.join("、"))
    }
}

/// Copies a code's content; with Ctrl, a link opens in the browser instead.
unsafe fn use_code(window: HWND, pin: &mut Pin, index: usize) {
    let Some(code) = pin.text.as_ref().and_then(|text| text.codes.get(index)).cloned() else { return };
    if GetKeyState(i32::from(VK_CONTROL)) < 0 {
        if let Some(link) = code.link() {
            let result = ShellExecuteW(
                window,
                w!("open"),
                crate::wide(link).as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            );
            if result as isize <= 32 {
                show_error(window, &format!("无法打开链接。\n{link}"));
            }
            return;
        }
    }
    match copy_text(window, &code.text) {
        Ok(()) => toast(window, pin, format!("已复制{}内容", code.kind())),
        Err(error) => show_error(window, &format!("无法复制{}内容。\n{error}", code.kind())),
    }
}

unsafe fn toast(window: HWND, pin: &mut Pin, message: String) {
    if let Some(text) = &mut pin.text {
        text.toast = Some(message);
        SetTimer(window, TOAST_TIMER, TOAST_MS, None);
    } else if SetTimer(window, BADGE_TIMER, TOAST_MS, None) != 0 {
        pin.note = message;
        pin.badge = Some(Badge::Note);
    }
    InvalidateRect(window, null(), 0);
}

unsafe fn copy(window: HWND, pin: &mut Pin, all: bool) {
    let Some(text) = &pin.text else { return };
    if text.lines().is_empty() {
        return;
    }
    let value = if all {
        ocr::text(text.lines())
    } else {
        text.copy_text()
    };
    match copy_text(window, &value) {
        Ok(()) => {
            let count = value.chars().filter(|c| !c.is_whitespace()).count();
            toast(window, pin, format!("已复制 {count} 个字"));
        }
        Err(error) => show_error(window, &format!("无法复制文字。\n{error}")),
    }
}

/// Starts leave tracking for the client area or the frame.
unsafe fn track_leave(window: HWND, client: bool) {
    let mut track = TRACKMOUSEEVENT {
        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: if client { TME_LEAVE } else { TME_LEAVE | TME_NONCLIENT },
        hwndTrack: window,
        dwHoverTime: 0,
    };
    TrackMouseEvent(&mut track);
}

unsafe fn set_hover(window: HWND, pin: &mut Pin, hovering: bool, button: bool) {
    if let Some(text) = &mut pin.text {
        if text.hovering != hovering || text.button_hovered != button {
            text.hovering = hovering;
            text.button_hovered = button;
            InvalidateRect(window, null(), 0);
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pin = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Pin;
    match message {
        WM_NCHITTEST => {
            if !pin.is_null() && (*pin).translated.is_some() {
                let mut point = mouse_point(lparam);
                ScreenToClient(window, &mut point);
                if (*pin).over_toggle(window, point) {
                    return HTCLIENT as LRESULT;
                }
                // Over the translation the whole image drags; the original text is hidden.
                if (*pin).showing_translation() {
                    return HTCAPTION as LRESULT;
                }
            }
            if !pin.is_null() && (*pin).text.is_some() {
                let mut point = mouse_point(lparam);
                ScreenToClient(window, &mut point);
                let view = (*pin).view(window);
                let text = (*pin).text.as_ref().unwrap();
                if text.selecting || text.over_text(view, point) || (*pin).over_button(window, point) {
                    return HTCLIENT as LRESULT;
                }
            }
            return HTCAPTION as LRESULT;
        }
        WM_SETCURSOR if !pin.is_null() && (lparam & 0xffff) as u32 == HTCLIENT => {
            let mut point = POINT::default();
            GetCursorPos(&mut point);
            ScreenToClient(window, &mut point);
            let view = (*pin).view(window);
            let over_code = (*pin).text.as_ref().is_some_and(|text| text.code_at(view, point).is_some());
            let cursor = if over_code || (*pin).over_button(window, point) || (*pin).over_toggle(window, point) {
                IDC_HAND
            } else {
                IDC_IBEAM
            };
            SetCursor(LoadCursorW(null_mut(), cursor));
            return 1;
        }
        WM_MOUSEMOVE if !pin.is_null() => {
            let point = mouse_point(lparam);
            track_leave(window, true);
            let button = (*pin).over_button(window, point);
            set_hover(window, &mut *pin, true, button);
            let view = (*pin).view(window);
            if let Some(text) = &mut (*pin).text {
                let code = if text.selecting { None } else { text.code_at(view, point) };
                if std::mem::replace(&mut text.hovered_code, code) != code {
                    InvalidateRect(window, null(), 0);
                }
            }
            let view = (*pin).view(window);
            if let Some(text) = &mut (*pin).text {
                if text.selecting && text.extend(view, point) {
                    (*pin).dragged = true;
                    InvalidateRect(window, null(), 0);
                }
            }
        }
        WM_NCMOUSEMOVE if !pin.is_null() => {
            track_leave(window, false);
            set_hover(window, &mut *pin, true, false);
        }
        WM_MOUSELEAVE | WM_NCMOUSELEAVE if !pin.is_null() => {
            // Moving between the frame and the text also "leaves"; only the real exit counts.
            let mut point = POINT::default();
            let mut bounds = RECT::default();
            GetCursorPos(&mut point);
            GetWindowRect(window, &mut bounds);
            if !contains(bounds, point) {
                set_hover(window, &mut *pin, false, false);
                if let Some(text) = &mut (*pin).text {
                    if text.hovered_code.take().is_some() {
                        InvalidateRect(window, null(), 0);
                    }
                }
            }
        }
        WM_NCLBUTTONDOWN if !pin.is_null() && wparam == HTCAPTION as usize => {
            if ctrl_down() {
                toggle_selected(window, &mut *pin);
                SetForegroundWindow(window);
                return 0;
            }
            click_without_ctrl(window, &*pin);
            return DefWindowProcW(window, message, wparam, lparam);
        }
        WM_LBUTTONDOWN if !pin.is_null() && ctrl_down() => {
            toggle_selected(window, &mut *pin);
        }
        WM_MOVING if !pin.is_null() && (*pin).selected => {
            drag_selection(window, *(lparam as *const RECT));
            return DefWindowProcW(window, message, wparam, lparam);
        }
        WM_LBUTTONDOWN if !pin.is_null() && (*pin).over_toggle(window, mouse_point(lparam)) => {
            (*pin).show_original = !(*pin).show_original;
            InvalidateRect(window, null(), 0);
        }
        WM_LBUTTONDOWN if !pin.is_null() && is_triple_click(&mut *pin, mouse_point(lparam)) => {
            let view = (*pin).view(window);
            if let Some(text) = &mut (*pin).text {
                text.select_line(view, mouse_point(lparam));
                InvalidateRect(window, null(), 0);
            }
        }
        WM_LBUTTONDOWN if !pin.is_null() => {
            click_without_ctrl(window, &*pin);
            let point = mouse_point(lparam);
            let view = (*pin).view(window);
            let code = (*pin).text.as_ref().and_then(|text| text.code_at(view, point));
            if let Some(index) = code {
                use_code(window, &mut *pin, index);
            } else if (*pin).over_button(window, point) {
                copy(window, &mut *pin, true);
            } else {
                let view = (*pin).view(window);
                if let Some(text) = &mut (*pin).text {
                    text.begin(view, point);
                    (*pin).dragged = false;
                    SetCapture(window);
                    InvalidateRect(window, null(), 0);
                }
            }
        }
        WM_LBUTTONUP if !pin.is_null() => {
            if let Some(text) = &mut (*pin).text {
                if text.selecting {
                    text.selecting = false;
                    // A click without dragging clears the selection.
                    if !(*pin).dragged {
                        text.clear();
                    }
                    ReleaseCapture();
                    InvalidateRect(window, null(), 0);
                }
            }
        }
        WM_LBUTTONDBLCLK if !pin.is_null() => {
            // Like a terminal: double-click picks the word, a third click the line.
            (*pin).double_click = Some((GetMessageTime() as u32, mouse_point(lparam)));
            let view = (*pin).view(window);
            if let Some(text) = &mut (*pin).text {
                text.select_word(view, mouse_point(lparam));
                InvalidateRect(window, null(), 0);
            }
        }
        WM_CAPTURECHANGED if !pin.is_null() => {
            if let Some(text) = &mut (*pin).text {
                text.selecting = false;
            }
        }
        OCR_DONE if !pin.is_null() => {
            let result = *Box::from_raw(lparam as *mut Recognized);
            let translating = (*pin).translate.is_some();
            match result {
                Ok((lines, codes)) => {
                    let message = recognized_message(lines.len(), &codes);
                    if let Some(text) = &mut (*pin).text {
                        text.status = Status::Ready(lines);
                        text.codes = codes;
                    }
                    if translating {
                        // The panel reports progress; Ctrl+C copies the translation.
                        start_translation(window, &mut *pin);
                    } else {
                        toast(window, &mut *pin, message);
                    }
                }
                Err(error) => {
                    if let Some(text) = &mut (*pin).text {
                        text.status = Status::Failed;
                    }
                    InvalidateRect(window, null(), 0);
                    match &mut (*pin).translate {
                        // The popup shows the whole error with a retry button.
                        Some(pane) => {
                            pane.phase = Phase::Failed(error);
                            refresh_popup(window, &*pin);
                        }
                        None => show_error(window, &error),
                    }
                }
            }
        }
        TRANSLATE_DONE if !pin.is_null() => {
            let outcome = *Box::from_raw(lparam as *mut Outcome);
            if let Some(pane) = &mut (*pin).translate {
                let to = match pane.phase {
                    Phase::Translating { to, .. } if pane.request == wparam as u32 => to,
                    // A newer request replaced this one.
                    _ => return 0,
                };
                pane.scroll = 0;
                if let Outcome::Done(text) = &outcome {
                    let lines = (*pin).text.as_ref().map_or(&[][..], TextLayer::lines);
                    let overlay = (*pin)
                        .source
                        .as_ref()
                        .and_then(|source| crate::translate_overlay::render(source, lines, text).ok())
                        .and_then(|image| image.into_surface().ok());
                    (*pin).translated = overlay.map(|(surface, _, _)| surface);
                    (*pin).show_original = false;
                    InvalidateRect(window, null(), 0);
                }
                let pane = (*pin).translate.as_mut().unwrap();
                pane.phase = match outcome {
                    Outcome::Done(text) => Phase::Done { text, to },
                    Outcome::Unconfigured => Phase::Unconfigured,
                    Outcome::Failed(error) => Phase::Failed(error),
                };
                refresh_popup(window, &*pin);
            }
        }
        SEARCH_DONE => {
            let outcome = crate::image_search::Outcome::from_message(lparam);
            let (opened, failure) = outcome.open();
            if let Some(message) = failure {
                show_error(window, &message);
            } else if !pin.is_null() && opened > 0 {
                toast(window, &mut *pin, "已在浏览器打开识图结果".into());
            }
        }
        RETRANSLATE if !pin.is_null() => {
            let waiting = (*pin)
                .translate
                .as_ref()
                .is_some_and(|pane| matches!(pane.phase, Phase::Unconfigured | Phase::Failed(_)));
            let recognized = (*pin).text.as_ref().is_some_and(|text| matches!(text.status, Status::Ready(_)));
            if waiting && recognized {
                start_translation(window, &mut *pin);
            }
        }
        WM_TIMER if !pin.is_null() && wparam == SHIMMER_TIMER => {
            if !(*pin).translate.as_ref().is_some_and(TranslatePane::busy) {
                KillTimer(window, SHIMMER_TIMER);
            }
            if !(*pin).popup.is_null() {
                InvalidateRect((*pin).popup, null(), 0);
            }
        }
        WM_TIMER if !pin.is_null() && wparam == TOAST_TIMER => {
            KillTimer(window, TOAST_TIMER);
            if let Some(text) = &mut (*pin).text {
                text.toast = None;
            }
            InvalidateRect(window, null(), 0);
        }
        WM_WINDOWPOSCHANGED if !pin.is_null() => {
            if !(*pin).popup.is_null() {
                place_popup(window, &*pin);
            }
            // Another pin came to the front: the ones kept above go back over it.
            let flags = (*(lparam as *const WINDOWPOS)).flags;
            if flags & SWP_NOZORDER == 0 && !(*pin).above {
                raise_above_pins();
            }
            return DefWindowProcW(window, message, wparam, lparam);
        }
        WM_ERASEBKGND => return 1,
        WM_NCDESTROY if !pin.is_null() => {
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            drop(Box::from_raw(pin));
            return DefWindowProcW(window, message, wparam, lparam);
        }
        WM_NCRBUTTONUP | WM_RBUTTONUP if !pin.is_null() => context_menu(window, &mut *pin),
        WM_NCLBUTTONDBLCLK => {
            DestroyWindow(window);
        }
        WM_KEYDOWN if wparam == VK_ESCAPE as usize => {
            let cleared = !pin.is_null() && (*pin).text.as_mut().is_some_and(TextLayer::clear);
            if cleared {
                InvalidateRect(window, null(), 0);
            } else {
                // Esc on a selected pin closes the whole selection.
                for (other, _) in targets(window) {
                    DestroyWindow(other);
                }
                if IsWindow(window) != 0 {
                    DestroyWindow(window);
                }
            }
        }
        WM_KEYDOWN if !pin.is_null() && GetKeyState(i32::from(VK_CONTROL)) < 0 => {
            match wparam as u8 {
                b'C' => {
                    // Without a selection, a translate pin copies its translation.
                    let selected = (*pin).text.as_ref().is_some_and(TextLayer::has_selection);
                    let translated = (*pin).translate.as_ref().and_then(TranslatePane::translation).is_some();
                    if translated && !selected {
                        copy_translation(window, &mut *pin);
                    } else if (*pin).text.is_none() {
                        copy_image(window, &mut *pin);
                    } else {
                        copy(window, &mut *pin, false);
                    }
                }
                b'A' => {
                    if let Some(text) = &mut (*pin).text {
                        text.select_all();
                        InvalidateRect(window, null(), 0);
                    }
                }
                _ => {}
            }
        }
        WM_MOUSEWHEEL if !pin.is_null() => {
            let delta = i32::from((wparam >> 16) as i16);
            // Wheel messages follow focus; the popup scrolls when the cursor is on it.
            if !(*pin).popup.is_null() && WindowFromPoint(mouse_point(lparam)) == (*pin).popup {
                return SendMessageW((*pin).popup, message, wparam, lparam);
            }
            // A selected pin zooms or fades the whole selection with it.
            let targets = targets(window);
            if wparam & 8 != 0 {
                let pending = (*pin).opacity_wheel_remainder + delta;
                (*pin).opacity_wheel_remainder = pending % WHEEL_NOTCH;
                let notches = pending / WHEEL_NOTCH;
                if notches == 0 {
                    return 0;
                }
                for (other, other_pin) in targets {
                    change_opacity(other, &mut *other_pin, notches);
                }
            } else {
                let factor = 1.15_f64.powf(f64::from(delta) / f64::from(WHEEL_NOTCH));
                for (other, other_pin) in targets {
                    zoom_by(other, &mut *other_pin, factor);
                }
            }
        }
        WM_TIMER if !pin.is_null() && wparam == BADGE_TIMER => {
            KillTimer(window, BADGE_TIMER);
            (*pin).badge = None;
            InvalidateRect(window, null(), 0);
        }
        WM_PAINT if !pin.is_null() => {
            let mut paint = PAINTSTRUCT::default();
            let target = BeginPaint(window, &mut paint);
            let mut client = RECT::default();
            GetClientRect(window, &mut client);
            // Paint off screen so the selection does not flicker while dragging.
            let Ok(buffer) = Surface::new(target, client.right, client.bottom) else {
                EndPaint(window, &paint);
                return 0;
            };
            let dc = buffer.dc;
            let mut bounds = client;
            let brush = GetStockObject(DC_BRUSH);
            // A glow that fades from the theme accent into its shadow.
            let palette = theme::palette();
            for step in 0..5 {
                let color = mix(palette.shadow, palette.accent_hover, step as f32 / 4.0);
                SetDCBrushColor(dc, color);
                FrameRect(dc, &bounds, brush);
                InflateRect(&mut bounds, -1, -1);
            }
            let bounds = (*pin).image_rect();
            SetStretchBltMode(dc, HALFTONE);
            SetBrushOrgEx(dc, 0, 0, null_mut());
            StretchBlt(
                dc,
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                match &(*pin).translated {
                    Some(translated) if !(*pin).show_original => translated.dc,
                    _ => (*pin).surface.dc,
                },
                0,
                0,
                (*pin).width,
                (*pin).height,
                SRCCOPY,
            );
            if let Some(text) = (*pin).text.as_ref().filter(|_| !(*pin).showing_translation()) {
                text.paint(dc, (*pin).view(window), bounds, &(*pin).fonts);
            }
            if let Some(button) = (*pin).toggle_button(dc) {
                let palette = theme::palette();
                let label = if (*pin).show_original { TOGGLE_TRANSLATION } else { TOGGLE_ORIGINAL };
                ui::button(dc, button, label, (*pin).fonts.small, palette, false, palette.card, 7.0 * (*pin).scale);
            }
            if (*pin).selected {
                // A solid accent frame over the image edge marks the selection.
                SetDCBrushColor(dc, palette.accent);
                let mut frame = bounds;
                for _ in 0..(*pin).s(3).max(2) {
                    FrameRect(dc, &frame, brush);
                    InflateRect(&mut frame, -1, -1);
                }
            }
            if let Some(badge) = (*pin).badge {
                paint_badge(dc, bounds, &*pin, badge);
            }
            BitBlt(target, 0, 0, client.right, client.bottom, dc, 0, 0, SRCCOPY);
            drop(buffer);
            EndPaint(window, &paint);
        }
        _ => return DefWindowProcW(window, message, wparam, lparam),
    }
    0
}

/// The translation popup. Its state lives in the owning pin; keys go to the
/// pin, which keeps the focus.
unsafe extern "system" fn popup_proc(popup: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let owner = GetWindow(popup, GW_OWNER);
    let pin = if owner.is_null() {
        null_mut()
    } else {
        GetWindowLongPtrW(owner, GWLP_USERDATA) as *mut Pin
    };
    if pin.is_null() {
        return DefWindowProcW(popup, message, wparam, lparam);
    }
    let pin = &mut *pin;
    let set_hovered = |pin: &mut Pin, hit: Option<PaneTarget>| {
        if let Some(pane) = &mut pin.translate {
            if std::mem::replace(&mut pane.hovered, hit) != hit {
                InvalidateRect(popup, null(), 0);
            }
        }
    };
    match message {
        WM_ERASEBKGND => return 1,
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_SETCURSOR if (lparam & 0xffff) as u32 == HTCLIENT => {
            let hovered = pin.translate.as_ref().and_then(|pane| pane.hovered);
            SetCursor(LoadCursorW(null_mut(), if hovered.is_some() { IDC_HAND } else { IDC_ARROW }));
            return 1;
        }
        WM_MOUSEMOVE => {
            track_leave(popup, true);
            let hit = pin.pane_hit(mouse_point(lparam));
            set_hovered(pin, hit);
        }
        WM_MOUSELEAVE => set_hovered(pin, None),
        WM_LBUTTONDOWN => {
            let hit = pin.pane_hit(mouse_point(lparam));
            if let Some(pane) = &mut pin.translate {
                pane.pressed = hit;
            }
            SetCapture(popup);
            InvalidateRect(popup, null(), 0);
        }
        WM_LBUTTONUP => {
            let pressed = pin.translate.as_mut().and_then(|pane| pane.pressed.take());
            ReleaseCapture();
            InvalidateRect(popup, null(), 0);
            if let Some(pressed) = pressed.filter(|&pressed| pin.pane_hit(mouse_point(lparam)) == Some(pressed)) {
                activate_pane(owner, pin, pressed);
            }
        }
        WM_CAPTURECHANGED => {
            if let Some(pane) = &mut pin.translate {
                pane.pressed = None;
            }
        }
        WM_MOUSEWHEEL => {
            let delta = i32::from((wparam >> 16) as i16);
            let Some(layout) = pin.pane_layout() else { return 0 };
            let dc = GetDC(popup);
            let step = -delta * pin.s(WHEEL_SCROLL) / WHEEL_NOTCH;
            let fonts = &pin.fonts;
            let moved = pin
                .translate
                .as_mut()
                .is_some_and(|pane| pane.scroll_by(&layout, dc, fonts, step));
            ReleaseDC(popup, dc);
            if moved {
                InvalidateRect(popup, null(), 0);
            }
        }
        WM_KEYDOWN | WM_KEYUP => return SendMessageW(owner, message, wparam, lparam),
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let target = BeginPaint(popup, &mut paint);
            let mut client = RECT::default();
            GetClientRect(popup, &mut client);
            if let (Ok(buffer), Some(pane), Some(layout)) = (
                Surface::new(target, client.right, client.bottom),
                pin.translate.as_ref(),
                pin.pane_layout(),
            ) {
                pane.paint(buffer.dc, &layout, &pin.fonts, now_ms());
                if let Some(graphics) = crate::gdiplus::Graphics::new(buffer.dc) {
                    let palette = theme::palette();
                    graphics.stroke_round(ui::inset(ui::to_f(client), 0.5), 0.0, theme::argb(palette.card_border, 255), 1.0);
                }
                BitBlt(target, 0, 0, client.right, client.bottom, buffer.dc, 0, 0, SRCCOPY);
            }
            EndPaint(popup, &paint);
        }
        _ => return DefWindowProcW(popup, message, wparam, lparam),
    }
    0
}

unsafe fn paint_badge(dc: HDC, image: RECT, pin: &Pin, badge: Badge) {
    const WIDTH: i32 = 110;
    const HEIGHT: i32 = 24;

    let width = image.right - image.left;
    if width < 44 || image.bottom - image.top < HEIGHT {
        return;
    }
    let (name, percent) = match badge {
        Badge::Zoom => ("大小", (pin.zoom * 100.0).round() as i32),
        Badge::Opacity => ("不透明度", pin.opacity),
        Badge::Note => {
            let label: Vec<u16> = pin.note.encode_utf16().chain([0]).collect();
            let wide = (pin.note.chars().count() as i32 * 14 + 24).min(width);
            let bounds = RECT { left: image.left, top: image.top, right: image.left + wide, bottom: image.top + HEIGHT };
            paint_size_badge(dc, bounds, label.as_ptr());
            return;
        }
    };
    let text = if width >= WIDTH {
        format!("{name} {percent}%")
    } else {
        format!("{percent}%")
    };
    let label: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let bounds = RECT {
        left: image.left,
        top: image.top,
        right: image.left + width.min(WIDTH),
        bottom: image.top + HEIGHT,
    };
    paint_size_badge(dc, bounds, label.as_ptr());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recognition_toast_counts_text_and_codes() {
        let qr = codes::Code { text: "x".into(), matrix: true, rect: [0.0; 4] };
        let bar = codes::Code { text: "y".into(), matrix: false, rect: [0.0; 4] };
        assert_eq!(recognized_message(0, &[]), "没有识别到文字");
        assert_eq!(recognized_message(3, &[]), "识别到 3 行文字，拖动选择，Ctrl+C 复制");
        assert_eq!(recognized_message(2, &[qr.clone()]), "识别到 2 行文字、1 个二维码，拖动选择，Ctrl+C 复制");
        assert_eq!(recognized_message(0, &[qr.clone(), qr, bar]), "识别到 2 个二维码、1 个条形码，单击复制内容");
    }

    #[test]
    fn pins_hide_and_show_by_group_and_can_leave_the_top() {
        unsafe {
            let _ = register(GetModuleHandleW(null()));
            close_all();
            HIDDEN_GROUPS.set(Vec::new());
            CURRENT_GROUP.set(1);
            let image = || Image { width: 40, height: 30, pixels: vec![128; 40 * 30 * 4] };
            open(image(), RECT { left: 10, top: 10, right: 50, bottom: 40 }, Mode::Plain).unwrap();
            CURRENT_GROUP.set(2);
            open(image(), RECT { left: 80, top: 10, right: 120, bottom: 40 }, Mode::Plain).unwrap();
            // The style bit: an SSH session's desktop counts as invisible itself.
            let shown = |window: HWND| i32::from(GetWindowLongW(window, GWL_STYLE) as u32 & WS_VISIBLE != 0);
            let all = pins();
            assert_eq!(all.len(), 2);
            assert_eq!(groups(), vec![(1, 1, true), (2, 1, true)]);
            let window_of = |group: u32| all.iter().find(|(_, pin)| (**pin).group == group).unwrap().0;
            // A process started hidden ignores its first ShowWindow; show once more.
            set_all_hidden(false);
            toggle_group(2);
            assert!(shown(window_of(1)) != 0);
            assert!(shown(window_of(2)) == 0, "group 2 is hidden");
            assert_eq!(groups()[1], (2, 1, false));
            set_all_hidden(true);
            assert!(shown(window_of(1)) == 0);
            set_all_hidden(false);
            assert!(shown(window_of(1)) != 0 && shown(window_of(2)) != 0);
            // A new pin in a hidden group shows that group again.
            toggle_group(2);
            open(image(), RECT { left: 150, top: 10, right: 190, bottom: 40 }, Mode::Plain).unwrap();
            assert!(shown(window_of(2)) != 0);

            let (window, pin) = all[0];
            assert!(GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0);
            set_topmost(window, &mut *pin, false);
            assert!(GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST == 0, "置顶 off");
            let copy = pin_image(&*pin).unwrap();
            assert_eq!((copy.width, copy.height), (40, 30));
            assert_eq!(copy.pixels[..3], [128, 128, 128]);

            // Ctrl+click picks pins; actions on a selected pin reach the whole selection.
            let current = pins();
            assert_eq!(current.len(), 3);
            let [(a, pa), (b, pb), (c, pc)] = [current[0], current[1], current[2]];
            toggle_selected(a, &mut *pa);
            toggle_selected(b, &mut *pb);
            let mut picked: Vec<isize> = targets(a).iter().map(|(window, _)| *window as isize).collect();
            picked.sort();
            let mut expected = vec![a as isize, b as isize];
            expected.sort();
            assert_eq!(picked, expected);
            assert_eq!(targets(c).len(), 1, "a pin outside the selection acts alone");
            let rect = |window: HWND| {
                let mut bounds = RECT::default();
                GetWindowRect(window, &mut bounds);
                (bounds.left, bounds.top)
            };
            let (before_a, before_b, before_c) = (rect(a), rect(b), rect(c));
            drag_selection(a, RECT { left: before_a.0 + 30, top: before_a.1 + 20, right: 0, bottom: 0 });
            assert_eq!(rect(b), (before_b.0 + 30, before_b.1 + 20), "the selection moves together");
            assert_eq!(rect(c), before_c);
            SendMessageW(a, WM_MOUSEWHEEL, (WHEEL_NOTCH as usize) << 16, 0);
            assert!((*pa).zoom > 1.0 && (*pb).zoom > 1.0 && (*pc).zoom == 1.0, "the wheel zooms the selection");
            click_without_ctrl(c, &*pc);
            assert!(!(*pa).selected && !(*pb).selected, "a plain click elsewhere ends the selection");

            // Hidden on its own until the tray shows everything again.
            (*pc).hidden = true;
            apply_visibility();
            assert!(shown(c) == 0);
            set_all_hidden(false);
            assert!(shown(c) != 0);

            // Kept above the other pins even when another comes to the front.
            set_topmost(a, &mut *pa, true);
            (*pa).above = true;
            raise_above_pins();
            SetWindowPos(b, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            let mut above = GetWindow(b, GW_HWNDPREV);
            while !above.is_null() && above != a {
                above = GetWindow(above, GW_HWNDPREV);
            }
            assert!(above == a, "the pin kept above stays over the raised one");
            close_all();
            assert!(pins().is_empty());
            CURRENT_GROUP.set(1);
        }
    }

    /// A text pin over a QR code: recognition finds it and a click copies it.
    /// Run with `<test binary> pinned --ignored --nocapture`.
    #[test]
    #[ignore]
    fn clicking_a_qr_code_on_a_pin_copies_it() {
        use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
        unsafe {
            let _ = register(GetModuleHandleW(null()));
            let image = codes::tests::render(300, 300, &[(rxing::BarcodeFormat::QR_CODE, "https://example.com/扫码", 40, 40, 6)]);
            let bounds = RECT { left: 60, top: 60, right: 360, bottom: 360 };
            open(image, bounds, Mode::Text).unwrap();
            let window = FindWindowW(CLASS, null());
            let pin = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Pin;
            let start = std::time::Instant::now();
            let mut message = MSG::default();
            while matches!((*pin).text.as_ref().unwrap().status, Status::Recognizing) && start.elapsed().as_secs() < 10 {
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    DispatchMessageW(&message);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let text = (*pin).text.as_ref().unwrap();
            assert_eq!(text.codes.len(), 1, "found in {:?}", start.elapsed());
            println!("{:?}", text.toast);
            assert!(text.toast.as_deref().is_some_and(|toast| toast.contains("1 个二维码")));
            let view = (*pin).view(window);
            let rect = text.codes[0].rect;
            let center = POINT {
                x: view.origin.x + ((rect[0] + rect[2]) / 2.0) as i32,
                y: view.origin.y + ((rect[1] + rect[3]) / 2.0) as i32,
            };
            assert!(text.over_text(view, center), "codes take clicks instead of dragging the pin");
            SendMessageW(window, WM_LBUTTONDOWN, 1, (center.x as u16 as isize) | ((center.y as isize) << 16));
            SendMessageW(window, WM_LBUTTONUP, 0, (center.x as u16 as isize) | ((center.y as isize) << 16));
            assert!(OpenClipboard(null_mut()) != 0);
            let handle = GetClipboardData(13);
            let data = windows_sys::Win32::System::Memory::GlobalLock(handle) as *const u16;
            let mut length = 0;
            while *data.add(length) != 0 {
                length += 1;
            }
            let copied = String::from_utf16_lossy(std::slice::from_raw_parts(data, length));
            windows_sys::Win32::System::Memory::GlobalUnlock(handle);
            CloseClipboard();
            assert_eq!(copied, "https://example.com/扫码");
            assert_eq!((*pin).text.as_ref().unwrap().toast.as_deref(), Some("已复制二维码内容"));
            DestroyWindow(window);
        }
    }

    /// Opens a text pin and pumps messages until background recognition lands.
    /// Run with `<test binary> pinned --ignored`.
    #[test]
    #[ignore]
    fn text_pin_becomes_selectable_after_recognition() {
        unsafe {
            let _ = register(GetModuleHandleW(null()));
            let image = crate::ocr::tests::sample("识别文字 Snapshot 保存到 C:\\Users\\eric\\Pictures", 18, true);
            let bounds = RECT { left: 100, top: 100, right: 100 + image.width, bottom: 100 + image.height };
            open(image, bounds, Mode::Text).unwrap();
            let window = FindWindowW(CLASS, null());
            assert!(!window.is_null());
            let pin = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Pin;
            let ready = || matches!((*pin).text.as_ref().unwrap().status, Status::Ready(_));
            assert!(!ready(), "recognition runs in the background");
            let start = std::time::Instant::now();
            let mut message = MSG::default();
            while !ready() && start.elapsed().as_secs() < 10 {
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let text = (*pin).text.as_mut().unwrap();
            assert!(ready(), "recognized within 10 s");
            println!("{:?} in {:?}", text.copy_text(), start.elapsed());
            assert!(text.copy_text().contains("Pictures"));
            assert!(text.toast.as_deref().is_some_and(|toast| toast.contains("1 行")));
            // Text is a selection target; the margin around it still drags the pin.
            let view = (*pin).view(window);
            let word = text.lines()[0].words[0].rect;
            let on_word = POINT {
                x: view.origin.x + ((word[0] + word[2]) / 2.0) as i32,
                y: view.origin.y + ((word[1] + word[3]) / 2.0) as i32,
            };
            assert!(text.over_text(view, on_word));
            assert!(!text.over_text(view, POINT { x: view.origin.x + 2, y: view.origin.y + 2 }));
            text.select_all();
            SendMessageW(window, WM_KEYDOWN, VK_ESCAPE as usize, 0);
            assert!(IsWindow(window) != 0, "Esc first clears the selection");
            assert!(!(*pin).text.as_ref().unwrap().has_selection());
            SendMessageW(window, WM_KEYDOWN, VK_ESCAPE as usize, 0);
            assert!(IsWindow(window) == 0, "then closes the pin");
        }
    }

    /// Opens a translate pin and checks the popup sits beside it and follows
    /// it. Run with `<test binary> pinned --ignored --nocapture`.
    #[test]
    #[ignore]
    fn translate_pin_keeps_its_popup_beside_it() {
        unsafe {
            let _ = register(GetModuleHandleW(null()));
            let image = crate::ocr::tests::sample("Save the file to your desktop", 18, true);
            let bounds = RECT { left: 20, top: 150, right: 20 + image.width, bottom: 150 + image.height };
            open(image, bounds, Mode::Translate).unwrap();
            let window = FindWindowW(CLASS, null());
            let pin = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Pin;
            let popup = (*pin).popup;
            assert!(!popup.is_null());
            assert_eq!(GetWindow(popup, GW_OWNER), window, "the pin owns its popup");
            let rects = || {
                let (mut a, mut b) = (RECT::default(), RECT::default());
                GetWindowRect(window, &mut a);
                GetWindowRect(popup, &mut b);
                (a, b)
            };
            let (pin_rect, popup_rect) = rects();
            assert_eq!((pin_rect.left, pin_rect.top), (bounds.left - GLOW, bounds.top - GLOW), "the pin stays on the capture");
            assert_eq!(popup_rect.left, pin_rect.right + (*pin).s(POPUP_GAP));
            assert_eq!(popup_rect.top, pin_rect.top);
            assert_eq!(popup_rect.right - popup_rect.left, (*pin).s(pin_translate::WIDTH));
            SetWindowPos(window, null_mut(), 40, 260, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            let (pin_rect, popup_rect) = rects();
            assert_eq!((popup_rect.left, popup_rect.top), (pin_rect.right + (*pin).s(POPUP_GAP), pin_rect.top), "follows the pin");
            let start = std::time::Instant::now();
            let mut message = MSG::default();
            while (*pin).translate.as_ref().unwrap().busy() && start.elapsed().as_secs() < 30 {
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    DispatchMessageW(&message);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let (_, popup_rect) = rects();
            let phase = match &(*pin).translate.as_ref().unwrap().phase {
                Phase::Unconfigured => "unconfigured".to_string(),
                Phase::Done { text, .. } => format!("done: {text}"),
                Phase::Failed(error) => format!("failed: {error}"),
                _ => "busy".into(),
            };
            println!("{phase}; popup {}x{}", popup_rect.right - popup_rect.left, popup_rect.bottom - popup_rect.top);
            assert_ne!(phase, "busy");
            // Near the right edge of the screen it flips to the left.
            let mut monitor = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            GetMonitorInfoW(MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST), &mut monitor);
            let (pin_rect, _) = rects();
            let width = pin_rect.right - pin_rect.left;
            SetWindowPos(window, null_mut(), monitor.rcWork.right - width, 260, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            let (pin_rect, popup_rect) = rects();
            if pin_rect.left - (*pin).s(POPUP_GAP + pin_translate::WIDTH) >= monitor.rcWork.left {
                assert_eq!(popup_rect.right, pin_rect.left - (*pin).s(POPUP_GAP), "flips to the left");
            }
            DestroyWindow(window);
            assert_eq!(IsWindow(popup), 0, "closing the pin closes the popup");
        }
    }

}
