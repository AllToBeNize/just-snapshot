//! The single exit for finished captures: clipboard, PNG file or pinned window.
//! Normal and long captures both go through [`deliver`], so a fix here applies
//! to every copy and save trigger.

use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    mem::size_of,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::null_mut,
    slice,
    sync::Mutex,
    thread::{self, JoinHandle},
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{GlobalFree, HWND, RECT},
        Graphics::Gdi::BITMAPINFOHEADER,
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW,
                SetClipboardData,
            },
            Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
        },
        UI::Controls::Dialogs::{
            CommDlgExtendedError, GetSaveFileNameW, OFN_EXPLORER, OFN_NOCHANGEDIR,
            OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
        },
    },
};

use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};

use crate::{
    capture::{bitmap_header, Image, BYTES_PER_PIXEL},
    dialog,
    output_settings::{self, OutputConfig},
    pinned, theme,
};

/// PNG files still being written. Encoding a long capture takes seconds, so
/// files are written in the background and the capture closes at once; the
/// app waits for them before it exits.
static WRITERS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;

#[derive(Clone, Copy)]
pub enum Target {
    Copy,
    /// Save as PNG: straight into the save folder when auto-save is on,
    /// otherwise through the file dialog, named by the template.
    Save,
    /// Pin on screen at the capture's screen position.
    Pin(RECT),
    /// Pin, then recognize its text so it can be selected and copied.
    TextPin(RECT),
    /// Pin, recognize its text and show a translation beside it.
    TranslatePin(RECT),
}

/// Delivers a capture. `Ok(true)` means it left Snapshot and the capture UI
/// can close; `Ok(false)` means the user cancelled the save dialog.
pub unsafe fn deliver(window: HWND, image: &Image, target: Target) -> Result<bool, String> {
    // A broken settings file must not stop a copy; it falls back to defaults.
    let config = OutputConfig::load().unwrap_or_default();
    let result = match target {
        Target::Copy => copy_image(window, image).map(|()| true),
        Target::Save if config.auto_save => {
            save_to_folder(image, &config, "无法保存图片。").map(|_| true)
        }
        Target::Save => {
            let name = config.file_name(image.width, image.height);
            match save_path(window, &name, &config.folder())? {
                Some(path) => {
                    save_later(image, &config, path, "无法保存图片。");
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        Target::Pin(screen) | Target::TextPin(screen) | Target::TranslatePin(screen) => {
            let mode = match target {
                Target::TextPin(_) => pinned::Mode::Text,
                Target::TranslatePin(_) => pinned::Mode::Translate,
                _ => pinned::Mode::Plain,
            };
            pinned::open(image.clone(), screen, mode)
                .map(|()| true)
                .map_err(|error| error.to_string())
        }
    };
    let delivered = result.map_err(|error| match target {
        Target::Copy => format!("无法复制到剪贴板。\n{error}"),
        Target::Save => format!("无法保存图片。\n{error}"),
        Target::Pin(_) | Target::TextPin(_) | Target::TranslatePin(_) => format!("无法贴图。\n{error}"),
    })?;
    if delivered && config.auto_save && !matches!(target, Target::Save) {
        let failure = if matches!(target, Target::Copy) {
            "截图已经复制，但没能自动保存。"
        } else {
            "截图已经贴到屏幕上，但没能自动保存。"
        };
        save_to_folder(image, &config, failure).map_err(|error| format!("{failure}\n{error}"))?;
    }
    Ok(delivered)
}

/// Saves the capture into the save folder under a free name, in the
/// background. The name is taken right away by creating the empty file, so
/// two quick saves never pick the same one.
fn save_to_folder(image: &Image, config: &OutputConfig, failure: &'static str) -> Result<PathBuf, String> {
    let folder = config.folder();
    fs::create_dir_all(&folder).map_err(|error| format!("无法创建保存位置 {}。\n{error}", folder.display()))?;
    let name = config.file_name(image.width, image.height);
    let path = loop {
        let path = output_settings::unique_path(&folder, &name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("{}\n{error}", path.display())),
        }
    };
    save_later(image, config, path.clone(), failure);
    Ok(path)
}

/// Encodes and writes `path` on a worker thread; a failure removes the
/// unfinished file and shows `failure` with the reason.
fn save_later(image: &Image, config: &OutputConfig, path: PathBuf, failure: &'static str) {
    let image = image.clone();
    let config = config.clone();
    let theme = theme::current();
    let work = move || {
        if let Err(error) = save_file(&image, &config, &path) {
            let _ = fs::remove_file(&path);
            theme::set(theme);
            unsafe { dialog::show(null_mut(), dialog::Kind::Error, &format!("{failure}\n{error}")) };
        }
    };
    let mut writers = WRITERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    writers.retain(|writer| !writer.is_finished());
    match thread::Builder::new().name("snapshot-save".into()).spawn(work) {
        Ok(writer) => writers.push(writer),
        Err(error) => {
            drop(writers);
            unsafe { dialog::show(null_mut(), dialog::Kind::Error, &format!("{failure}\n{error}")) };
        }
    }
}

/// Blocks until every background save has finished; called before exiting.
pub fn wait_for_saves() {
    let writers = std::mem::take(&mut *WRITERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    for writer in writers {
        let _ = writer.join();
    }
}

/// Saves as PNG, with the shadow and border the settings ask for.
fn save_file(image: &Image, config: &OutputConfig, path: &Path) -> Result<(), String> {
    let result = if config.decorates() {
        output_settings::decorate(image, config.shadow, config.border).save_png_alpha(path)
    } else {
        image.save_png(path)
    };
    result.map_err(|error| format!("{}\n{error}", path.display()))
}

/// Puts the image on the clipboard as an opaque bottom-up CF_DIB plus PNG.
///
/// GDI captures leave alpha at 0, and apps that honor alpha in a 32-bit DIB
/// (Feishu, browsers) would paste a fully transparent image. Many apps also
/// mishandle top-down DIBs, so rows are flipped. PNG is what Chromium-based
/// apps and Office prefer; it is added next to the DIB when encoding works.
///
/// The PNG also goes on as a virtual file (`FileGroupDescriptorW` plus
/// `FileContents`), so pasting in Explorer or on the desktop creates a PNG
/// file. Chat apps and editors keep pasting the image itself: they look for
/// the bitmap formats, not virtual files.
pub fn copy_image(owner: HWND, image: &Image) -> Result<(), String> {
    let mut png = Vec::new();
    let png = image.write_png_fast(&mut png).ok().map(|()| png);
    let format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    let mut items = vec![(CF_DIB, opaque_dib(image))];
    if let (Some(png), true) = (png, format != 0) {
        let (descriptor, contents) = unsafe {
            (
                RegisterClipboardFormatW(w!("FileGroupDescriptorW")),
                RegisterClipboardFormatW(w!("FileContents")),
            )
        };
        items.push((format, png.clone()));
        if descriptor != 0 && contents != 0 {
            let mut with_file = items.clone();
            with_file.push((descriptor, file_descriptor(&file_name(), png.len())));
            with_file.push((contents, png));
            if crate::clipboard_data::set(with_file, contents).is_ok() {
                return Ok(());
            }
            // Without OLE the image still copies; only pasting as a file is lost.
        }
    }
    set_clipboard(owner, items)
}

/// "just-snapshot 2026-11-20 114500.png" in local time.
fn file_name() -> String {
    let mut time = SYSTEMTIME::default();
    unsafe { GetLocalTime(&mut time) };
    format!(
        "just-snapshot {:04}-{:02}-{:02} {:02}{:02}{:02}.png",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
    )
}

/// A `FILEGROUPDESCRIPTORW` with one file of `size` bytes.
fn file_descriptor(name: &str, size: usize) -> Vec<u8> {
    const FD_FILESIZE: u32 = 0x40;
    const FD_PROGRESSUI: u32 = 0x4000;
    // FILEDESCRIPTORW: flags, CLSID, SIZEL, POINTL, attributes, three
    // FILETIMEs, size high and low, then a 260-character name.
    let mut data = Vec::with_capacity(4 + 592);
    data.extend_from_slice(&1u32.to_le_bytes());
    data.extend_from_slice(&(FD_FILESIZE | FD_PROGRESSUI).to_le_bytes());
    data.extend_from_slice(&[0; 16 + 8 + 8 + 4 + 24]);
    data.extend_from_slice(&((size as u64 >> 32) as u32).to_le_bytes());
    data.extend_from_slice(&(size as u32).to_le_bytes());
    let mut units: Vec<u16> = name.encode_utf16().take(259).collect();
    units.resize(260, 0);
    data.extend(units.into_iter().flat_map(u16::to_le_bytes));
    debug_assert_eq!(data.len(), 4 + 592);
    data
}

pub fn copy_text(owner: HWND, text: &str) -> Result<(), String> {
    let bytes = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    set_clipboard(owner, vec![(CF_UNICODETEXT, bytes)])
}

/// Replaces the clipboard with `items`. The first item is required; later
/// ones are best-effort extra formats.
fn set_clipboard(owner: HWND, items: Vec<(u32, Vec<u8>)>) -> Result<(), String> {
    let error = || std::io::Error::last_os_error().to_string();
    unsafe {
        if OpenClipboard(owner) == 0 {
            return Err(error());
        }
        if EmptyClipboard() == 0 {
            let error = error();
            CloseClipboard();
            return Err(error);
        }
        for (index, (format, bytes)) in items.iter().enumerate() {
            let memory = match global_copy(bytes) {
                Ok(memory) => memory,
                Err(message) if index == 0 => {
                    CloseClipboard();
                    return Err(message);
                }
                Err(_) => continue,
            };
            // On success the clipboard owns the memory.
            if SetClipboardData(*format, memory).is_null() {
                let message = error();
                GlobalFree(memory);
                if index == 0 {
                    CloseClipboard();
                    return Err(message);
                }
            }
        }
        CloseClipboard();
    }
    Ok(())
}

/// BITMAPINFOHEADER followed by bottom-up rows with alpha forced to 255.
fn opaque_dib(image: &Image) -> Vec<u8> {
    let mut header = bitmap_header(image.width, image.height);
    header.biHeight = image.height;
    let header_size = size_of::<BITMAPINFOHEADER>();
    let mut data = Vec::with_capacity(header_size + image.pixels.len());
    // SAFETY: BITMAPINFOHEADER is a plain C struct.
    data.extend_from_slice(unsafe {
        slice::from_raw_parts((&header as *const BITMAPINFOHEADER).cast::<u8>(), header_size)
    });
    let row_bytes = image.width as usize * BYTES_PER_PIXEL;
    for row in image.pixels.chunks_exact(row_bytes).rev() {
        for pixel in row.chunks_exact(BYTES_PER_PIXEL) {
            data.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
    }
    data
}

/// Copies bytes into a movable global block for SetClipboardData.
unsafe fn global_copy(bytes: &[u8]) -> Result<*mut core::ffi::c_void, String> {
    let error = || std::io::Error::last_os_error().to_string();
    let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
    if memory.is_null() {
        return Err(error());
    }
    let target = GlobalLock(memory).cast::<u8>();
    if target.is_null() {
        let message = error();
        GlobalFree(memory);
        return Err(message);
    }
    target.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len());
    GlobalUnlock(memory);
    Ok(memory)
}

/// Asks where to save, starting in `folder`. `Ok(None)` when the user cancels.
unsafe fn save_path(window: HWND, default_name: &str, folder: &std::path::Path) -> Result<Option<PathBuf>, String> {
    let folder: Vec<u16> = folder.as_os_str().encode_wide().chain([0]).collect();
    let mut filename = [0u16; 1024];
    let default_name: Vec<u16> = default_name.encode_utf16().take(filename.len() - 1).collect();
    filename[..default_name.len()].copy_from_slice(&default_name);
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: window,
        lpstrFilter: w!("PNG 图片 (*.png)\0*.png\0"),
        lpstrFile: filename.as_mut_ptr(),
        nMaxFile: filename.len() as u32,
        Flags: OFN_EXPLORER | OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        lpstrDefExt: w!("png"),
        lpstrInitialDir: folder.as_ptr(),
        ..Default::default()
    };
    if GetSaveFileNameW(&mut dialog) == 0 {
        let error = CommDlgExtendedError();
        return if error == 0 {
            Ok(None)
        } else {
            Err(format!("保存对话框错误 {error:#x}"))
        };
    }
    let end = filename
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(filename.len());
    Ok(Some(OsString::from_wide(&filename[..end]).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where a long-capture copy spends its time:
    /// `SNAPSHOT_PROFILE_IMAGE=<png> <test binary> profile_copy --ignored --nocapture`.
    #[test]
    #[ignore]
    fn profile_copy() {
        use std::time::Instant;
        let path = std::env::var("SNAPSHOT_PROFILE_IMAGE").unwrap();
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut rgba = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut rgba).unwrap();
        let pixels: Vec<u8> = rgba[..info.buffer_size()]
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], 0])
            .collect();
        for repeat in [1, 4] {
            let image = Image {
                width: info.width as i32,
                height: info.height as i32 * repeat,
                pixels: pixels.repeat(repeat as usize),
            };
            let started = Instant::now();
            let mut encoded = Vec::new();
            image.write_png(&mut encoded).unwrap();
            let png_time = started.elapsed();
            for compression in [png::Compression::Balanced, png::Compression::Fast, png::Compression::Fastest] {
                let started = Instant::now();
                let mut out = Vec::new();
                let mut encoder = png::Encoder::new(&mut out, image.width as u32, image.height as u32);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_compression(compression);
                let mut writer = encoder.write_header().unwrap();
                let rgba: Vec<u8> = image.pixels.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
                writer.write_image_data(&rgba).unwrap();
                writer.finish().unwrap();
                println!("    {compression:?}: {:?}, {} KB", started.elapsed(), out.len() / 1024);
            }
            let started = Instant::now();
            let dib = opaque_dib(&image);
            let dib_time = started.elapsed();
            let started = Instant::now();
            let copied = copy_image(std::ptr::null_mut(), &image);
            println!(
                "{}x{}: png {png_time:?} ({} KB), dib {dib_time:?} ({} KB), copy_image {:?} {:?}",
                image.width,
                image.height,
                encoded.len() / 1024,
                dib.len() / 1024,
                started.elapsed(),
                copied.is_ok()
            );
        }
    }

    #[test]
    fn clipboard_dib_is_opaque_and_bottom_up() {
        // A 1×2 GDI capture: alpha is 0, rows are top-down.
        let image = Image {
            width: 1,
            height: 2,
            pixels: vec![10, 20, 30, 0, 40, 50, 60, 0],
        };
        let dib = opaque_dib(&image);
        let header_size = size_of::<BITMAPINFOHEADER>();
        let header = unsafe { dib.as_ptr().cast::<BITMAPINFOHEADER>().read_unaligned() };
        assert_eq!(header.biHeight, 2, "positive height means bottom-up");
        assert_eq!(&dib[header_size..], &[40, 50, 60, 255, 10, 20, 30, 255]);
        let mut png = Vec::new();
        image.write_png(&mut png).unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn quick_saves_get_their_own_files_and_finish_in_the_background() {
        let folder = std::env::temp_dir().join(format!("snapshot-save-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        let config = OutputConfig {
            folder: folder.display().to_string(),
            template: "same name".into(),
            ..OutputConfig::default()
        };
        let image = Image {
            width: 64,
            height: 2000,
            pixels: (0..64 * 2000 * 4).map(|i| (i * 7 % 251) as u8).collect(),
        };
        let first = save_to_folder(&image, &config, "无法保存图片。").unwrap();
        let second = save_to_folder(&image, &config, "无法保存图片。").unwrap();
        assert_ne!(first, second, "the second save must not reuse the reserved name");
        wait_for_saves();
        for path in [&first, &second] {
            let decoder = png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()));
            let info = decoder.read_info().unwrap();
            assert_eq!((info.info().width, info.info().height), (64, 2000));
        }
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn the_virtual_file_names_a_png_of_the_right_size() {
        let descriptor = file_descriptor("截图.png", 0x1_0000_0002);
        assert_eq!(descriptor.len(), 596);
        assert_eq!(&descriptor[..4], &1u32.to_le_bytes(), "one file");
        assert_eq!(&descriptor[68..72], &1u32.to_le_bytes(), "size high");
        assert_eq!(&descriptor[72..76], &2u32.to_le_bytes(), "size low");
        let name: Vec<u16> = descriptor[76..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|&unit| unit != 0)
            .collect();
        assert_eq!(String::from_utf16(&name).unwrap(), "截图.png");
        assert!(file_name().starts_with("just-snapshot 20") && file_name().ends_with(".png"));
    }

    /// Reads the clipboard back the way Explorer does, through OLE with
    /// `lindex` 0. Run with `<test binary> paste_as_file --ignored`.
    #[test]
    #[ignore]
    fn explorer_can_paste_the_copy_as_a_file() {
        use windows::Win32::System::{
            Com::{FORMATETC, DVASPECT_CONTENT, TYMED_HGLOBAL},
            Ole::{OleGetClipboard, OleInitialize, ReleaseStgMedium},
        };
        let image = Image { width: 3, height: 2, pixels: vec![200; 24] };
        copy_image(std::ptr::null_mut(), &image).unwrap();
        unsafe {
            let _ = OleInitialize(None);
            let data = OleGetClipboard().unwrap();
            let read = |name: *const u16, index: i32| {
                let format = FORMATETC {
                    cfFormat: RegisterClipboardFormatW(name) as u16,
                    ptd: std::ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: index,
                    tymed: TYMED_HGLOBAL.0 as u32,
                };
                let mut medium = data.GetData(&format).unwrap_or_else(|error| panic!("lindex {index}: {error}"));
                let global = medium.u.hGlobal;
                let size = windows::Win32::System::Memory::GlobalSize(global);
                let pointer = windows::Win32::System::Memory::GlobalLock(global) as *const u8;
                let bytes = std::slice::from_raw_parts(pointer, size).to_vec();
                let _ = windows::Win32::System::Memory::GlobalUnlock(global);
                ReleaseStgMedium(&mut medium);
                bytes
            };
            let descriptor = read(w!("FileGroupDescriptorW"), -1);
            let size = u32::from_le_bytes(descriptor[72..76].try_into().unwrap()) as usize;
            let contents = read(w!("FileContents"), 0);
            assert_eq!(&contents[1..4], b"PNG");
            assert!(contents.len() >= size, "the file has the announced size");
        }
    }
}
