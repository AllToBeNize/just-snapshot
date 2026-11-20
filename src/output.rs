//! The single exit for finished captures: clipboard, PNG file or pinned window.
//! Normal and long captures both go through [`deliver`], so a fix here applies
//! to every copy and save trigger.

use std::{
    ffi::OsString,
    mem::size_of,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
    slice,
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

use crate::{
    capture::{bitmap_header, Image, BYTES_PER_PIXEL},
    pinned,
};

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;

#[derive(Clone, Copy)]
pub enum Target {
    Copy,
    /// Save as PNG, suggesting `name` in the file dialog.
    Save(&'static str),
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
    let result = match target {
        Target::Copy => copy_image(window, image).map(|()| true),
        Target::Save(name) => match save_path(window, name)? {
            Some(path) => image
                .save_png(&path)
                .map(|()| true)
                .map_err(|error| error.to_string()),
            None => Ok(false),
        },
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
    result.map_err(|error| match target {
        Target::Copy => format!("无法复制到剪贴板。\n{error}"),
        Target::Save(_) => format!("无法保存图片。\n{error}"),
        Target::Pin(_) | Target::TextPin(_) | Target::TranslatePin(_) => format!("无法贴图。\n{error}"),
    })
}

/// Puts the image on the clipboard as an opaque bottom-up CF_DIB plus PNG.
///
/// GDI captures leave alpha at 0, and apps that honor alpha in a 32-bit DIB
/// (Feishu, browsers) would paste a fully transparent image. Many apps also
/// mishandle top-down DIBs, so rows are flipped. PNG is what Chromium-based
/// apps and Office prefer; it is added next to the DIB when encoding works.
pub fn copy_image(owner: HWND, image: &Image) -> Result<(), String> {
    let mut png = Vec::new();
    let png = image.write_png(&mut png).ok().map(|()| png);
    let format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    let mut items = vec![(CF_DIB, opaque_dib(image))];
    if let (Some(png), true) = (png, format != 0) {
        items.push((format, png));
    }
    set_clipboard(owner, items)
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

/// Asks where to save. `Ok(None)` when the user cancels.
unsafe fn save_path(window: HWND, default_name: &str) -> Result<Option<PathBuf>, String> {
    let mut filename = [0u16; 1024];
    let default_name: Vec<u16> = default_name.encode_utf16().collect();
    filename[..default_name.len()].copy_from_slice(&default_name);
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: window,
        lpstrFilter: w!("PNG 图片 (*.png)\0*.png\0"),
        lpstrFile: filename.as_mut_ptr(),
        nMaxFile: filename.len() as u32,
        Flags: OFN_EXPLORER | OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        lpstrDefExt: w!("png"),
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
}
