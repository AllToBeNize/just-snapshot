use std::{
    fs::File,
    io::{self, BufWriter, Write},
    mem::size_of,
    path::Path,
    ptr::{null_mut, NonNull},
    slice,
};
use windows_sys::Win32::{
    Foundation::RECT,
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    },
};

pub(crate) const BYTES_PER_PIXEL: usize = 4;

pub struct Desktop {
    pub bounds: RECT,
    pub surface: Surface,
}

pub struct Surface {
    pub dc: HDC,
    bitmap: HBITMAP,
    previous_bitmap: HGDIOBJ,
    pixels: NonNull<u8>,
}

pub struct RegionCapture {
    bounds: RECT,
    surface: Surface,
    len: usize,
}

impl RegionCapture {
    pub fn new(bounds: RECT) -> io::Result<Self> {
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        let len = pixel_len(width, height)?;
        let surface = unsafe { Surface::new(null_mut(), width, height)? };
        Ok(Self {
            bounds,
            surface,
            len,
        })
    }

    pub fn sample(&mut self) -> io::Result<&[u8]> {
        unsafe {
            let screen = GetDC(null_mut());
            if screen.is_null() {
                return Err(io::Error::last_os_error());
            }
            let bounds = self.bounds;
            let copied = BitBlt(
                self.surface.dc,
                0,
                0,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                screen,
                bounds.left,
                bounds.top,
                SRCCOPY | CAPTUREBLT,
            );
            let result = if copied == 0 || GdiFlush() == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(slice::from_raw_parts(
                    self.surface.pixels.as_ptr(),
                    self.len,
                ))
            };
            ReleaseDC(null_mut(), screen);
            result
        }
    }
}

impl Desktop {
    pub fn average_bgr(&self, rect: RECT) -> [u8; 3] {
        let source_width = self.bounds.right - self.bounds.left;
        let source_height = self.bounds.bottom - self.bounds.top;
        assert!(
            rect.left >= 0
                && rect.top >= 0
                && rect.right <= source_width
                && rect.bottom <= source_height
                && rect.left < rect.right
                && rect.top < rect.bottom
        );
        let stride = source_width as usize * BYTES_PER_PIXEL;
        let width = (rect.right - rect.left) as usize;
        let height = (rect.bottom - rect.top) as usize;
        let mut sum = [0u64; 3];
        for y in rect.top..rect.bottom {
            let offset = y as usize * stride + rect.left as usize * BYTES_PER_PIXEL;
            // The source is the frozen capture, and the mosaic tiles are clipped to it.
            let row = unsafe {
                slice::from_raw_parts(
                    self.surface.pixels.as_ptr().add(offset),
                    width * BYTES_PER_PIXEL,
                )
            };
            for pixel in row.chunks_exact(BYTES_PER_PIXEL) {
                for channel in 0..3 {
                    sum[channel] += u64::from(pixel[channel]);
                }
            }
        }
        let count = (width * height) as u64;
        sum.map(|channel| ((channel + count / 2) / count) as u8)
    }

    pub fn visual_panel(
        &self,
        window: RECT,
        point: windows_sys::Win32::Foundation::POINT,
    ) -> Option<RECT> {
        let width = self.bounds.right - self.bounds.left;
        let height = self.bounds.bottom - self.bounds.top;
        let pixels = unsafe {
            // The frozen capture owns this DIB and its only write was flushed at capture time.
            slice::from_raw_parts(self.surface.pixels.as_ptr(), pixel_len(width, height).ok()?)
        };
        crate::panel_edges::find_panel(pixels, width as usize, window, point)
    }

    pub fn capture() -> io::Result<Self> {
        unsafe {
            let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            pixel_len(width, height)?;

            let screen = GetDC(null_mut());
            if screen.is_null() {
                return Err(io::Error::last_os_error());
            }
            let result = Surface::new(screen, width, height).and_then(|surface| {
                if BitBlt(
                    surface.dc,
                    0,
                    0,
                    width,
                    height,
                    screen,
                    left,
                    top,
                    SRCCOPY | CAPTUREBLT,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                GdiFlush();
                Ok(Self {
                    bounds: RECT {
                        left,
                        top,
                        right: left + width,
                        bottom: top + height,
                    },
                    surface,
                })
            });
            ReleaseDC(null_mut(), screen);
            result
        }
    }

    pub fn crop(&self, rect: RECT) -> io::Result<Image> {
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let source_width = self.bounds.right - self.bounds.left;
        let source_height = self.bounds.bottom - self.bounds.top;
        if rect.left < 0 || rect.top < 0 || rect.right > source_width || rect.bottom > source_height
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "选区超出桌面范围",
            ));
        }
        let mut pixels = Vec::with_capacity(pixel_len(width, height)?);
        let source = unsafe {
            // The top-down DIB belongs to this capture and BitBlt was flushed before access.
            slice::from_raw_parts(
                self.surface.pixels.as_ptr(),
                pixel_len(source_width, source_height)?,
            )
        };
        let row_bytes = width as usize * BYTES_PER_PIXEL;
        for y in rect.top..rect.bottom {
            let start = (y as usize * source_width as usize + rect.left as usize) * BYTES_PER_PIXEL;
            pixels.extend_from_slice(&source[start..start + row_bytes]);
        }
        Ok(Image {
            width,
            height,
            pixels,
        })
    }

    pub unsafe fn composite_crop(&self, rect: RECT, render: impl FnOnce(HDC)) -> io::Result<Image> {
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let surface = Surface::new(self.surface.dc, width, height)?;
        if BitBlt(
            surface.dc,
            0,
            0,
            width,
            height,
            self.surface.dc,
            rect.left,
            rect.top,
            SRCCOPY,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        SetViewportOrgEx(surface.dc, -rect.left, -rect.top, null_mut());
        render(surface.dc);
        GdiFlush();
        let bounds = RECT {
            right: width,
            bottom: height,
            ..Default::default()
        };
        Self { bounds, surface }.crop(bounds)
    }
}

impl Surface {
    pub fn bitmap(&self) -> HBITMAP {
        self.bitmap
    }

    /// A copy of the pixels of a `width`×`height` surface.
    pub unsafe fn image(&self, width: i32, height: i32) -> io::Result<Image> {
        let length = pixel_len(width, height)?;
        GdiFlush();
        Ok(Image {
            width,
            height,
            pixels: slice::from_raw_parts(self.pixels.as_ptr(), length).to_vec(),
        })
    }

    // The source buffer must fit the allocated DIB.
    pub unsafe fn write_pixels(&self, source: &[u8]) {
        self.pixels
            .as_ptr()
            .copy_from_nonoverlapping(source.as_ptr(), source.len());
    }

    pub unsafe fn new(reference: HDC, width: i32, height: i32) -> io::Result<Self> {
        let dc = CreateCompatibleDC(reference);
        if dc.is_null() {
            return Err(io::Error::last_os_error());
        }
        let info = BITMAPINFO {
            bmiHeader: bitmap_header(width, height),
            ..Default::default()
        };
        let mut bits = null_mut();
        let bitmap = CreateDIBSection(reference, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let Some(pixels) = NonNull::new(bits.cast()) else {
            let error = io::Error::last_os_error();
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            DeleteDC(dc);
            return Err(error);
        };
        let previous_bitmap = SelectObject(dc, bitmap);
        if previous_bitmap.is_null() || previous_bitmap as isize == -1 {
            let error = io::Error::last_os_error();
            DeleteObject(bitmap);
            DeleteDC(dc);
            return Err(error);
        }
        Ok(Self {
            dc,
            bitmap,
            previous_bitmap,
            pixels,
        })
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous_bitmap);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

#[derive(Clone)]
pub struct Image {
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) pixels: Vec<u8>,
}

impl Image {
    pub unsafe fn blit(&self, dc: HDC, left: i32, top: i32) {
        let info = BITMAPINFO {
            bmiHeader: bitmap_header(self.width, self.height),
            ..Default::default()
        };
        SetDIBitsToDevice(
            dc,
            left,
            top,
            self.width as u32,
            self.height as u32,
            0,
            0,
            0,
            self.height as u32,
            self.pixels.as_ptr().cast(),
            &info,
            DIB_RGB_COLORS,
        );
    }

    pub unsafe fn into_surface(self) -> io::Result<(Surface, i32, i32)> {
        let surface = Surface::new(null_mut(), self.width, self.height)?;
        surface
            .pixels
            .as_ptr()
            .copy_from_nonoverlapping(self.pixels.as_ptr(), self.pixels.len());
        Ok((surface, self.width, self.height))
    }

    pub fn save_png(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let mut file = BufWriter::new(File::create(path)?);
        self.write_png(&mut file)?;
        file.flush()?;
        Ok(())
    }

    /// Saves with the alpha channel kept, for images that have real
    /// transparency (a decorated save with a shadow).
    pub fn save_png_alpha(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let mut file = BufWriter::new(File::create(path)?);
        self.encode_png(&mut file, true)?;
        file.flush()?;
        Ok(())
    }

    /// Opaque PNG: GDI captures leave alpha at 0, so it is written as 255.
    pub fn write_png(&self, output: impl Write) -> Result<(), Box<dyn std::error::Error>> {
        self.encode_png(output, false)
    }

    fn encode_png(&self, output: impl Write, alpha: bool) -> Result<(), Box<dyn std::error::Error>> {
        let mut encoder = png::Encoder::new(output, self.width as u32, self.height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        let mut stream = writer.stream_writer()?;
        let row_bytes = self.width as usize * BYTES_PER_PIXEL;
        let mut rgba = Vec::with_capacity(row_bytes);
        for row in self.pixels.chunks_exact(row_bytes) {
            rgba.clear();
            for &[b, g, r, a] in row.as_chunks::<BYTES_PER_PIXEL>().0 {
                rgba.extend_from_slice(&[r, g, b, if alpha { a } else { 255 }]);
            }
            stream.write_all(&rgba)?;
        }
        stream.finish()?;
        writer.finish()?;
        Ok(())
    }
}

pub(crate) fn bitmap_header(width: i32, height: i32) -> BITMAPINFOHEADER {
    BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..Default::default()
    }
}

fn pixel_len(width: i32, height: i32) -> io::Result<usize> {
    let length = i64::from(width) * i64::from(height);
    if width > 0 && height > 0 && length <= isize::MAX as i64 / BYTES_PER_PIXEL as i64 {
        return Ok(length as usize * BYTES_PER_PIXEL);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "桌面或选区尺寸无效",
    ))
}
