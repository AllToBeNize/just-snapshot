//! Draws a translation over the original text of a pin, like a translated
//! photo: each recognized line is covered with its background color and its
//! translation is written in the text color, shrunk to fit.

use std::io;
use windows_sys::{
    core::w,
    Win32::{Foundation::RECT, Graphics::Gdi::*},
};

use crate::{capture::Image, ocr::Line, text_mark::contrast};

/// Smallest font tried before the text is cut with an ellipsis.
const MIN_FONT: i32 = 9;

/// A copy of `source` with `translation` over `lines`. When the translation
/// has one line per recognized line they are matched up; otherwise the whole
/// text area becomes one wrapped block.
pub unsafe fn render(source: &Image, lines: &[Line], translation: &str) -> io::Result<Image> {
    let (surface, width, height) = source.clone().into_surface()?;
    let parts: Vec<&str> = translation.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    let blocks: Vec<([f32; 4], String, bool)> = if parts.len() == lines.len() {
        lines.iter().zip(&parts).map(|(line, text)| (line.bounds(), text.to_string(), false)).collect()
    } else if let Some(first) = lines.first() {
        let union = lines.iter().fold(first.bounds(), |[l, t, r, b], line| {
            let [l2, t2, r2, b2] = line.bounds();
            [l.min(l2), t.min(t2), r.max(r2), b.max(b2)]
        });
        vec![(union, parts.join("\n"), true)]
    } else {
        Vec::new()
    };
    SetBkMode(surface.dc, TRANSPARENT as i32);
    for (bounds, text, wrap) in blocks {
        let rect = RECT {
            left: (bounds[0].floor() as i32 - 2).max(0),
            top: (bounds[1].floor() as i32 - 2).max(0),
            right: (bounds[2].ceil() as i32 + 2).min(width),
            bottom: (bounds[3].ceil() as i32 + 2).min(height),
        };
        if rect.left >= rect.right || rect.top >= rect.bottom {
            continue;
        }
        let background = background(source, rect);
        let ink = ink(source, rect, background);
        SetDCBrushColor(surface.dc, background);
        FillRect(surface.dc, &rect, GetStockObject(DC_BRUSH));
        draw_fitted(surface.dc, rect, &text, ink, wrap);
    }
    GdiFlush();
    surface.image(width, height)
}

fn pixel(image: &Image, x: i32, y: i32) -> [u8; 3] {
    let index = ((y * image.width + x) * 4) as usize;
    [image.pixels[index], image.pixels[index + 1], image.pixels[index + 2]]
}

fn colorref([b, g, r]: [u8; 3]) -> u32 {
    u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16)
}

/// The median color of the ring just outside `rect`: what the text sits on.
fn background(image: &Image, rect: RECT) -> u32 {
    let mut ring = Vec::new();
    let (left, top) = ((rect.left - 1).max(0), (rect.top - 1).max(0));
    let (right, bottom) = (rect.right.min(image.width - 1), rect.bottom.min(image.height - 1));
    for x in left..=right {
        ring.push(pixel(image, x, top));
        ring.push(pixel(image, x, bottom));
    }
    for y in top..=bottom {
        ring.push(pixel(image, left, y));
        ring.push(pixel(image, right, y));
    }
    let median = |channel: usize| {
        let mut values: Vec<u8> = ring.iter().map(|color| color[channel]).collect();
        values.sort_unstable();
        values[values.len() / 2]
    };
    colorref([median(0), median(1), median(2)])
}

/// The text color: the pixel inside `rect` furthest from the background,
/// or plain contrast when the text is too faint to tell.
fn ink(image: &Image, rect: RECT, background: u32) -> u32 {
    let bg = [(background >> 16) as u8, (background >> 8) as u8, background as u8];
    let distance = |color: [u8; 3]| {
        color.iter().zip(bg).map(|(a, b)| (i32::from(*a) - i32::from(b)).abs()).sum::<i32>()
    };
    let mut best = (0, bg);
    for y in rect.top..rect.bottom {
        for x in rect.left..rect.right {
            let color = pixel(image, x, y);
            let far = distance(color);
            if far > best.0 {
                best = (far, color);
            }
        }
    }
    if best.0 < 90 {
        contrast(background)
    } else {
        colorref(best.1)
    }
}

unsafe fn font(size: i32) -> HFONT {
    CreateFontW(
        -size,
        0,
        0,
        0,
        FW_NORMAL as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        ANTIALIASED_QUALITY as u32,
        DEFAULT_PITCH as u32,
        w!("Microsoft YaHei UI"),
    )
}

/// Writes `text` as large as fits in `rect`: one line, or wrapped for a block.
unsafe fn draw_fitted(dc: HDC, rect: RECT, text: &str, color: u32, wrap: bool) {
    let units: Vec<u16> = text.encode_utf16().collect();
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    let layout = if wrap { DT_WORDBREAK | DT_EDITCONTROL } else { DT_SINGLELINE };
    // Recognized boxes hug the glyphs, so a line's font is about its full height.
    let start = if wrap { (height / 2).clamp(MIN_FONT, 28) } else { (height * 19 / 20).max(MIN_FONT) };
    let mut size = start;
    let mut chosen = font(size);
    loop {
        let previous = SelectObject(dc, chosen);
        let mut measure = RECT { right: width, ..Default::default() };
        DrawTextW(dc, units.as_ptr(), units.len() as i32, &mut measure, layout | DT_CALCRECT | DT_NOPREFIX);
        SelectObject(dc, previous);
        let fits = measure.right <= width && measure.bottom <= height;
        if fits || size <= MIN_FONT {
            break;
        }
        DeleteObject(chosen);
        size = (size - (size / 10).max(1)).max(MIN_FONT);
        chosen = font(size);
    }
    let previous = SelectObject(dc, chosen);
    SetTextColor(dc, color);
    let mut target = rect;
    let vertical = if wrap { 0 } else { DT_VCENTER };
    DrawTextW(dc, units.as_ptr(), units.len() as i32, &mut target, layout | vertical | DT_LEFT | DT_NOPREFIX | DT_END_ELLIPSIS);
    SelectObject(dc, previous);
    DeleteObject(chosen);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::Word;

    fn line(rect: [f32; 4]) -> Line {
        Line { words: vec![Word { text: "x".into(), rect }] }
    }

    /// A light page with two dark "text" bars.
    fn page() -> Image {
        let (width, height) = (300, 120);
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let index = ((y * width + x) * 4) as usize;
                let text = (20..200).contains(&x) && ((20..36).contains(&y) || (60..76).contains(&y)) && x % 3 != 0;
                let color = if text { [40, 30, 20] } else { [240, 245, 250] };
                pixels[index..index + 3].copy_from_slice(&color);
            }
        }
        Image { width, height, pixels }
    }

    #[test]
    fn each_line_is_covered_and_rewritten_in_the_text_color() {
        unsafe {
            let source = page();
            let lines = [line([20.0, 20.0, 200.0, 36.0]), line([20.0, 60.0, 200.0, 76.0])];
            let image = render(&source, &lines, "第一行\n第二行").unwrap();
            let at = |x: i32, y: i32| pixel(&image, x, y);
            // The old bars are gone where no new letters land.
            assert_eq!(at(190, 28), [240, 245, 250], "covered with the page color");
            // New letters appear in a dark color near the original ink.
            let dark = (20..80).flat_map(|x| (20..36).map(move |y| (x, y))).filter(|&(x, y)| at(x, y)[0] < 120).count();
            assert!(dark > 20, "translated text is drawn: {dark}");
            assert_eq!(at(250, 100), [240, 245, 250], "the rest is untouched");
        }
    }

    /// Real recognition, then a translation drawn over it. Run with
    /// `<test binary> translate_overlay --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        unsafe {
            let source = crate::ocr::tests::sample("Save the screenshot to a folder", 20, true);
            let lines = crate::ocr::recognize(&source).unwrap();
            let translation: Vec<&str> = ["把截图保存到文件夹"].into_iter().cycle().take(lines.len()).collect();
            let image = render(&source, &lines, &translation.join("\n")).unwrap();
            let folder = std::env::temp_dir().join("snapshot-preview");
            std::fs::create_dir_all(&folder).unwrap();
            let opaque = |image: &Image| {
                let mut image = image.clone();
                for pixel in image.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                image
            };
            opaque(&source).save_png(&folder.join("overlay-before.png")).unwrap();
            opaque(&image).save_png(&folder.join("overlay-after.png")).unwrap();
        }
    }

    #[test]
    fn mismatched_lines_become_one_block() {
        unsafe {
            let source = page();
            let lines = [line([20.0, 20.0, 200.0, 36.0]), line([20.0, 60.0, 200.0, 76.0])];
            let image = render(&source, &lines, "One long sentence that the service did not split.").unwrap();
            // The gap between the two lines is covered too.
            assert_eq!(pixel(&image, 195, 50), [240, 245, 250]);
            assert!(background(&source, RECT { left: 18, top: 18, right: 202, bottom: 78 }) == colorref([240, 245, 250]));
        }
    }
}
