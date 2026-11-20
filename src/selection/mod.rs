use std::{
    borrow::Cow,
    cell::RefCell,
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
            Controls::{EM_GETSEL, EM_SETRECT, EM_SETSEL, WM_MOUSELEAVE},
            Input::Ime::{
                ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow, ImmSetCompositionWindow,
                CANDIDATEFORM, CFS_CANDIDATEPOS, CFS_FORCE_POSITION, COMPOSITIONFORM,
                IMN_OPENCANDIDATE,
            },
            Input::KeyboardAndMouse::{
                GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, MOD_ALT, MOD_CONTROL, MOD_SHIFT,
                TME_LEAVE, TRACKMOUSEEVENT, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_ESCAPE, VK_LEFT,
                VK_RETURN, VK_RIGHT, VK_UP,
            },
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
};

use crate::{
    annotation::{Draft, Grip, History, Mark, Style, Tool},
    annotation_options::{Choice, Options},
    capture::{Desktop, Image, Surface},
    output,
    hotkey::{self, Shortcut},
    icons::Cursor,
    overlay_ui::{monitor_info, HintMode, HintPanel, Loupe},
    scroll_capture,
    selection_geometry::{
        contains, drag_rect, hit_drag, intersect, mouse_point, DragKind, Handle, BORDER_BLUE,
        HANDLES,
    },
    show_error,
    targets::{self, ElementProbe, Request, WindowTarget, ELEMENT_READY},
    text_mark::{handle_cursor, TextHandle, TextMark},
    text_render::{self, PRINT_BACKGROUND},
    toolbar::{paint_size_badge, paint_tooltip, Action, Toolbar},
    typography,
};

mod annotate;
mod chrome;
mod export;
mod layers;
mod marks;
mod paint;
mod pick;
mod pointer;
mod size;
mod text;
#[cfg(test)]
mod tests {
    mod clipboard;
    mod marks;
    mod select;
    mod text;
}

use annotate::*;
use chrome::*;
use export::*;
use layers::*;
use marks::*;
use paint::*;
use pick::*;
use pointer::*;
use size::*;
use text::*;

const CLASS: *const u16 = w!("Snapshot.Selection");
const FINISH_TEXT: u32 = WM_APP + 3;
const TEXT_INPUT_CLICK: u32 = WM_APP + 8;
const TEXT_CARET_TIMER: usize = 3;

thread_local! {
    static SESSION: RefCell<Option<Selection>> = const { RefCell::new(None) };
    /// The last locked capture, kept so it can be brought back.
    static LAST: RefCell<Option<Kept>> = const { RefCell::new(None) };
}

/// What "找回上次截图" restores: the frozen screen, the selection and the
/// annotations with their undo history.
struct Last {
    desktop: Desktop,
    rect: RECT,
    annotations: History,
    style: Style,
}

/// The last capture while it waits: the frozen screen is squeezed into a
/// PNG in the background, a few MB instead of a full-size bitmap.
struct Kept {
    bounds: RECT,
    /// The background thread compressing the screen; joining it hands the result over.
    frozen: std::thread::JoinHandle<Packed>,
    rect: RECT,
    annotations: History,
    style: Style,
}

enum Packed {
    Png(Vec<u8>),
    /// Encoding failed; the pixels are kept as they are.
    Raw(Image),
}

impl Kept {
    /// The screen back as a bitmap, ready to show again.
    unsafe fn unpack(self) -> io::Result<Last> {
        let packed = self.frozen.join().map_err(|_| io::Error::other("上次的截图没能保存下来。"))?;
        let mut image = match packed {
            Packed::Raw(image) => image,
            Packed::Png(bytes) => crate::app_icon::decode(&bytes).ok_or_else(|| io::Error::other("上次的截图没能读回来。"))?,
        };
        // GDI captures carry no alpha; keep the bitmap exactly as captured.
        for pixel in image.pixels.chunks_exact_mut(4) {
            pixel[3] = 0;
        }
        let (surface, _, _) = image.into_surface()?;
        Ok(Last {
            desktop: Desktop { bounds: self.bounds, surface },
            rect: self.rect,
            annotations: self.annotations,
            style: self.style,
        })
    }
}

enum Start {
    Fresh,
    /// Screen coordinates to select right away.
    Region(RECT),
    Last(Last),
}

struct Selection {
    desktop: Desktop,
    dimmed: Surface,
    back_buffer: Surface,
    canvas: RECT,
    targets: Vec<WindowTarget>,
    probe: Option<ElementProbe>,
    target: Option<usize>,
    pointer: Option<POINT>,
    visual_probe: Option<(POINT, Option<RECT>)>,
    automatic: bool,
    drag: Option<Drag>,
    rect: RECT,
    hint: HintPanel,
    loupe: Option<Loupe>,
    pointer_monitor: RECT,
    annotations: History,
    toolbar: Option<Toolbar>,
    toolbar_hover: Option<Action>,
    options: Option<Options>,
    options_hover: Option<Choice>,
    style: Style,
    selected_text: Option<usize>,
    text_drag: Option<TextDrag>,
    /// A drawn shape picked up again to move, resize or restyle.
    selected_mark: Option<usize>,
    mark_drag: Option<MarkDrag>,
    /// The box for typing an exact size, over the size badge.
    size_input: Option<HWND>,
    /// Elements under the pointer from the last probe, in screen coordinates.
    element_layers: Vec<RECT>,
    /// A layer chosen with the side buttons, kept until the pointer moves.
    layer: Option<LayerPick>,
    /// Marks picked with the select tool, moved together.
    picked: Vec<usize>,
    pick_drag: Option<PickDrag>,
    editor: Option<Editor>,
    brush_cursor: Option<Cursor>,
    mosaic_cursor: Option<(i32, Cursor)>,
}

struct MarkDrag {
    index: usize,
    grip: Grip,
    anchor: POINT,
    /// The shape before the drag; every move starts from it.
    original: Mark,
    moved: bool,
}

struct TextDrag {
    index: Option<usize>,
    anchor: POINT,
    handle: TextHandle,
    original: TextMark,
    frame: Option<TextMark>,
    caret: Option<RECT>,
}

struct Editor {
    window: HWND,
    rect: RECT,
    index: Option<usize>,
    text: TextMark,
    preview: Option<Image>,
    caret_visible: bool,
}

impl Editor {
    fn frame(&self) -> Cow<'_, TextMark> {
        if self.text.is_rotated() {
            Cow::Borrowed(&self.text)
        } else {
            let mut frame = TextMark::new(self.rect, self.text.size, self.text.color);
            frame.look = self.text.look;
            frame.align = self.text.align;
            frame.valign = self.text.valign;
            Cow::Owned(frame)
        }
    }
}

impl Selection {
    #[cfg(test)]
    fn selection_rect(&self) -> (i32, i32, i32, i32) {
        (self.rect.left, self.rect.top, self.rect.right, self.rect.bottom)
    }

    fn selected_text(&self) -> Option<&TextMark> {
        self.selected_text
            .and_then(|index| self.annotations.text(index))
    }

    fn text_frame(&self) -> Option<Cow<'_, TextMark>> {
        if let Some(editor) = &self.editor {
            return Some(if self.text_drag.is_some() {
                Cow::Borrowed(&editor.text)
            } else {
                editor.frame()
            });
        }
        self.selected_text().map(Cow::Borrowed)
    }

    fn option_style(&self) -> Style {
        let mut style = self.style;
        if let Some(text) = self
            .editor
            .as_ref()
            .map(|editor| &editor.text)
            .or_else(|| self.selected_text())
        {
            style.text_size = text.size;
            style.text_color = text.color;
            style.text_look = text.look;
            style.text_align = text.align;
            style.text_valign = text.valign;
        }
        if let Some(mark) = self.selected_mark_frame() {
            mark.style_into(&mut style);
        }
        style
    }
}

struct Drag {
    anchor: POINT,
    previous: RECT,
    /// The selection was already locked, so a change is an undoable edit.
    locked: bool,
    kind: DragKind,
    started: bool,
    copy_on_click: bool,
}

pub unsafe fn register(instance: HINSTANCE) -> io::Result<()> {
    let class = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hCursor: LoadCursorW(null_mut(), IDC_CROSS),
        lpszClassName: CLASS,
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub unsafe fn open(instance: HINSTANCE) -> io::Result<()> {
    open_selection(instance, Start::Fresh)
}

pub unsafe fn reopen(instance: HINSTANCE, region: RECT) -> io::Result<()> {
    open_selection(instance, Start::Region(region))
}

pub fn has_last() -> bool {
    LAST.with_borrow(Option::is_some)
}

/// Opens the last capture again. `Ok(false)` when there is none.
pub unsafe fn restore_last(instance: HINSTANCE) -> Result<bool, String> {
    if SESSION.with_borrow(Option::is_some) || scroll_capture::is_open() {
        return Ok(true);
    }
    let Some(kept) = LAST.take() else {
        return Ok(false);
    };
    let current = RECT {
        left: GetSystemMetrics(SM_XVIRTUALSCREEN),
        top: GetSystemMetrics(SM_YVIRTUALSCREEN),
        right: GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN),
        bottom: GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN),
    };
    if EqualRect(&current, &kept.bounds) == 0 {
        return Err("屏幕的排列或分辨率变了，上次的截图对不上现在的屏幕。".into());
    }
    let last = kept.unpack().map_err(|error| error.to_string())?;
    open_selection(instance, Start::Last(last)).map(|()| true).map_err(|error| error.to_string())
}

unsafe fn open_selection(instance: HINSTANCE, start: Start) -> io::Result<()> {
    if SESSION.with_borrow(Option::is_some) || scroll_capture::is_open() {
        return Ok(());
    }
    let mut cursor = POINT::default();
    GetCursorPos(&mut cursor);
    let (targets, desktop, region, annotations, style) = match start {
        Start::Last(last) => {
            let bounds = last.desktop.bounds;
            let screen = RECT {
                left: last.rect.left + bounds.left,
                top: last.rect.top + bounds.top,
                right: last.rect.right + bounds.left,
                bottom: last.rect.bottom + bounds.top,
            };
            // The windows have moved since; the old screen has no live targets.
            (Vec::new(), last.desktop, Some(screen), last.annotations, last.style)
        }
        start => {
            let targets = targets::windows_at_capture();
            let region = match start {
                Start::Region(rect) => Some(rect),
                _ => None,
            };
            (targets, Desktop::capture()?, region, History::default(), Style::default())
        }
    };
    let bounds = desktop.bounds;
    let restored = region.and_then(|rect| overlay_rect(rect, bounds));
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    let canvas = RECT {
        right: width,
        bottom: height,
        ..Default::default()
    };
    let source = desktop.surface.dc;
    let monitor = monitor_info(region.map_or(cursor, |rect| POINT {
        x: rect.left + (rect.right - rect.left) / 2,
        y: rect.top + (rect.bottom - rect.top) / 2,
    }));
    let work_area = monitor
        .and_then(|info| overlay_rect(info.rcWork, bounds))
        .unwrap_or(canvas);
    let pointer_monitor = monitor
        .and_then(|info| overlay_rect(info.rcMonitor, bounds))
        .unwrap_or(canvas);
    let hint = HintPanel::new(source, work_area)?;
    let dimmed = Surface::new(source, width, height)?;
    FillRect(dimmed.dc, &canvas, GetStockObject(BLACK_BRUSH));
    let blend = BLENDFUNCTION {
        SourceConstantAlpha: 102,
        ..Default::default()
    };
    if AlphaBlend(
        dimmed.dc, 0, 0, width, height, source, 0, 0, width, height, blend,
    ) == 0
    {
        return Err(io::Error::last_os_error());
    }
    let back_buffer = Surface::new(source, width, height)?;
    let window = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        CLASS,
        w!("Snapshot Selection"),
        WS_POPUP | WS_CLIPCHILDREN,
        bounds.left,
        bounds.top,
        bounds.right - bounds.left,
        bounds.bottom - bounds.top,
        null_mut(),
        null_mut(),
        instance,
        null(),
    );
    if window.is_null() {
        return Err(io::Error::last_os_error());
    }
    SESSION.replace(Some(Selection {
        desktop,
        dimmed,
        back_buffer,
        canvas,
        targets,
        probe: ElementProbe::start(window),
        target: None,
        pointer: None,
        visual_probe: None,
        automatic: restored.is_none(),
        drag: None,
        rect: restored.unwrap_or_default(),
        hint,
        loupe: None,
        pointer_monitor,
        annotations,
        toolbar: None,
        toolbar_hover: None,
        options: None,
        options_hover: None,
        style,
        selected_text: None,
        text_drag: None,
        selected_mark: None,
        mark_drag: None,
        size_input: None,
        element_layers: Vec::new(),
        layer: None,
        picked: Vec::new(),
        pick_drag: None,
        editor: None,
        brush_cursor: None,
        mosaic_cursor: None,
    }));
    if restored.is_some() {
        refresh_toolbar(window);
        refresh_hint(window);
    } else {
        hover(
            window,
            POINT {
                x: cursor.x - bounds.left,
                y: cursor.y - bounds.top,
            },
        );
    }
    ShowWindow(window, SW_SHOW);
    SetForegroundWindow(window);
    SetFocus(window);
    UpdateWindow(window);
    Ok(())
}

fn overlay_rect(target: RECT, desktop: RECT) -> Option<RECT> {
    intersect(target, desktop).map(|clipped| RECT {
        left: clipped.left - desktop.left,
        top: clipped.top - desktop.top,
        right: clipped.right - desktop.left,
        bottom: clipped.bottom - desktop.top,
    })
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_PAINT => paint(window),
        WM_ERASEBKGND => return 1,
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let point = mouse_point(lparam);
            if let Some(options) = SESSION
                .with_borrow(|session| session.as_ref()?.options)
                .filter(|options| contains(options.bounds, point))
            {
                if let Some(choice) = options.hit(point) {
                    change_option(window, choice);
                }
                return 0;
            }
            let toolbar = SESSION.with_borrow(|session| session.as_ref()?.toolbar);
            if let Some(toolbar) = toolbar.filter(|toolbar| contains(toolbar.bounds, point)) {
                if let Some(action) = toolbar.hit(point) {
                    command(window, action);
                }
                return 0;
            }
            if begin_size_input(window, point) {
                return 0;
            }
            if begin_editor_drag(window, point) {
                return 0;
            }
            if let Some(editor) = SESSION.with_borrow(|session| {
                let editor = session.as_ref()?.editor.as_ref()?;
                editor.frame().contains(point).then_some(editor.window)
            }) {
                SendMessageW(
                    editor,
                    TEXT_INPUT_CLICK,
                    ((message as usize) << 16) | (wparam & 0xffff),
                    lparam,
                );
                return 0;
            }
            finish_text(window, true);
            if begin_text_drag(window, point, message == WM_LBUTTONDBLCLK) {
                hide_loupe(window);
                return 0;
            }
            if begin_pick(window, point, wparam & 0x0008 != 0) {
                hide_loupe(window);
                return 0;
            }
            if begin_mark_drag(window, point) {
                hide_loupe(window);
                return 0;
            }
            if begin_annotation(window, point) {
                SetCapture(window);
                hide_loupe(window);
                invalidate_toolbar(window);
                return 0;
            }
            SESSION.with_borrow_mut(|session| {
                if let Some(selection) = session {
                    let point = mouse_point(lparam);
                    if !contains(selection.canvas, point) {
                        return;
                    }
                    selection.drag = Some(Drag {
                        anchor: point,
                        previous: selection.rect,
                        locked: !selection.automatic,
                        kind: if selection.automatic {
                            DragKind::Draw
                        } else {
                            hit_drag(selection.rect, selection.canvas, point)
                        },
                        started: false,
                        copy_on_click: message == WM_LBUTTONDBLCLK,
                    });
                }
            });
            SetCapture(window);
            update_loupe(window, mouse_point(lparam));
        }
        WM_MOUSEMOVE => {
            let point = mouse_point(lparam);
            if update_text_drag(window, point, false)
                || update_pick(window, point, false)
                || update_mark_drag(window, point, false)
                || update_annotation(window, point, false)
            {
                return 0;
            }
            if !update_drag(window, point, false) {
                hover(window, point);
            }
            update_toolbar_hover(window, Some(point));
            update_options_hover(window, Some(point));
            update_cursor(point);
            update_loupe(window, point);
        }
        WM_LBUTTONUP => {
            let point = mouse_point(lparam);
            if update_text_drag(window, point, true)
                || update_pick(window, point, true)
                || update_mark_drag(window, point, true)
                || update_annotation(window, point, true)
            {
                return 0;
            }
            update_drag(window, point, true);
            update_toolbar_hover(window, Some(point));
            update_options_hover(window, Some(point));
            update_cursor(point);
            update_loupe(window, point);
        }
        WM_RBUTTONUP => {
            let editing =
                SESSION.with_borrow(|session| session.as_ref().is_some_and(|s| s.editor.is_some()));
            finish_text(window, false);
            if editing || clear_picks(window) || deselect_mark(window) || leave_tool(window, mouse_point(lparam)) {
                return 0;
            }
            if SESSION.with_borrow(|session| session.as_ref().is_some_and(|s| s.automatic)) {
                DestroyWindow(window);
            } else {
                reset_automatic(window, mouse_point(lparam));
            }
        }
        SIZE_DONE => finish_size_input(window, lparam as HWND, wparam != 0),
        FINISH_TEXT => {
            if SESSION.with_borrow(|session| {
                session
                    .as_ref()
                    .and_then(|selection| selection.editor.as_ref())
                    .is_some_and(|editor| editor.window == lparam as HWND)
            }) {
                finish_text(window, wparam != 0);
            }
        }
        WM_COMMAND if (wparam >> 16) as u32 == EN_CHANGE => {
            invalidate_text_input(window, lparam as HWND);
        }
        WM_TIMER if wparam == TEXT_CARET_TIMER => {
            if let Some(bounds) = SESSION.with_borrow_mut(|session| {
                let editor = session.as_mut()?.editor.as_mut()?;
                editor.caret_visible = !editor.caret_visible;
                Some(editor.frame().bounds())
            }) {
                InvalidateRect(window, &bounds, 0);
            }
        }
        WM_CTLCOLOREDIT => {
            if let Some(color) = SESSION.with_borrow(|session| {
                let editor = session.as_ref()?.editor.as_ref()?;
                (editor.window == lparam as HWND).then_some(editor.text.ink())
            }) {
                SetTextColor(wparam as HDC, color);
                SetBkMode(wparam as HDC, TRANSPARENT as i32);
                SetBrushOrgEx(wparam as HDC, 0, 0, null_mut());
                if let Some(background) = PRINT_BACKGROUND.get() {
                    SetBkColor(wparam as HDC, background);
                    return GetStockObject(if background == 0 {
                        BLACK_BRUSH
                    } else {
                        WHITE_BRUSH
                    }) as LRESULT;
                }
                return GetStockObject(NULL_BRUSH) as LRESULT;
            }
            return DefWindowProcW(window, message, wparam, lparam);
        }
        WM_MOUSEWHEEL => {
            let delta = i32::from((wparam >> 16) as i16);
            // Choosing automatically, the wheel steps through layers like the side buttons.
            if delta.abs() >= 120 && !step_layer(window, delta > 0) {
                change_option(window, Choice::Primary(delta / 120));
            }
        }
        WM_XBUTTONDOWN | WM_XBUTTONDBLCLK => {
            // Back (XBUTTON1) widens to the enclosing layer, forward narrows.
            step_layer(window, (wparam >> 16) & 0xffff == 1);
            return 1;
        }
        // Swallowed so the button never reaches anything as "back".
        WM_XBUTTONUP => return 1,
        WM_MOUSELEAVE => {
            update_toolbar_hover(window, None);
            update_options_hover(window, None);
            if SESSION.with_borrow(|session| {
                session
                    .as_ref()
                    .is_some_and(|selection| selection.drag.is_none())
            }) {
                hide_loupe(window);
            }
        }
        ELEMENT_READY => refine_target(window),
        WM_SETCURSOR => {
            let mut point = POINT::default();
            if GetCursorPos(&mut point) != 0 && ScreenToClient(window, &mut point) != 0 {
                update_cursor(point);
                return 1;
            }
        }
        WM_CAPTURECHANGED => {
            cancel_text_drag(window);
            cancel_mark_drag(window);
            cancel_pick(window);
            let (interrupted, draft) = SESSION.with_borrow_mut(|session| {
                let Some(selection) = session else {
                    return (false, None);
                };
                (
                    selection.drag.take().is_some(),
                    selection.annotations.draft.take().map(|_| selection.rect),
                )
            });
            if interrupted {
                hide_loupe(window);
            }
            if let Some(rect) = draft {
                InvalidateRect(window, &rect, 0);
                invalidate_toolbar(window);
            }
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let key = wparam as u32;
            let modifiers = hotkey::modifiers();
            if key == u32::from(VK_ESCAPE) && modifiers == 0 {
                // A selected shape is let go first; the next Esc leaves.
                if !clear_picks(window) && !deselect_mark(window) {
                    DestroyWindow(window);
                }
            } else if modifiers == 0 && (key == u32::from(VK_DELETE) || key == u32::from(VK_BACK)) {
                if !delete_picks(window) {
                    delete_selected(window);
                }
            } else if modifiers == MOD_ALT
                && (key == u32::from(VK_LEFT) || key == u32::from(VK_RIGHT))
                && step_layer(window, key == u32::from(VK_LEFT))
            {
                // Mouse software often sends the side buttons as Alt+←/→.
            } else if nudge(window, key, modifiers, lparam & (1 << 30) != 0) {
            } else if let Some(action) = hotkey::current()
                .resolve(Shortcut { modifiers, key })
                .and_then(Action::from_binding)
            {
                if lparam & (1 << 30) == 0 {
                    command(window, action);
                }
            } else if modifiers == 0 && key == u32::from(b'C') {
                copy_color(window);
            } else if modifiers == MOD_CONTROL && key == u32::from(b'A') {
                // Fixed shortcut, used only when no binding claims Ctrl+A.
                select_screen(window);
            } else if message == WM_SYSKEYDOWN {
                return DefWindowProcW(window, message, wparam, lparam);
            }
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_CLOSE => {
            DestroyWindow(window);
        }
        WM_DESTROY => {
            if let Some(selection) = SESSION.take() {
                remember(selection);
            }
            // The capture's full-screen buffers are gone; give their pages back.
            crate::capture::release_memory();
        }
        _ => return DefWindowProcW(window, message, wparam, lparam),
    }
    0
}

/// Keeps a locked capture, finished or cancelled, for "找回上次截图".
fn remember(selection: Selection) {
    if selection.automatic || outline_empty(selection.rect) {
        return;
    }
    let bounds = selection.desktop.bounds;
    let full = RECT { right: bounds.right - bounds.left, bottom: bounds.bottom - bounds.top, ..Default::default() };
    // Copy the pixels out so the full-size bitmap is freed right away.
    let Ok(pixels) = selection.desktop.crop(full) else {
        return;
    };
    let mut annotations = selection.annotations;
    annotations.tool = None;
    annotations.draft = None;
    let worker = std::thread::Builder::new().name("snapshot-keep-last".into()).spawn(move || {
        let mut png = Vec::new();
        let packed = match pixels.write_png(&mut png) {
            Ok(()) => Packed::Png(png),
            Err(_) => Packed::Raw(pixels),
        };
        crate::capture::release_memory();
        packed
    });
    let Ok(worker) = worker else {
        return;
    };
    LAST.set(Some(Kept {
        bounds,
        frozen: worker,
        rect: selection.rect,
        annotations,
        style: selection.style,
    }));
}

fn point_param(point: POINT) -> LPARAM {
    (u32::from(point.x as u16) | (u32::from(point.y as u16) << 16)) as LPARAM
}
