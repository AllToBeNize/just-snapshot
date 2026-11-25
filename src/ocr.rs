//! Text recognition. `recognize` uses the engine chosen in the settings:
//! 快速 is the OCR engine built into Windows (`Windows.Media.Ocr`, below):
//! offline, no setup, recognizer languages follow the user's Windows
//! languages. 准确 is PaddleOCR from the downloaded pack (`paddle_ocr`).
//!
//! The engine reads text about 18 px and taller well but garbles smaller UI
//! text, so small text is recognized again on a smoothly enlarged copy.
//! Images taller than the engine accepts are recognized in overlapping strips.

use windows::{
    Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Storage::Streams::DataWriter,
    Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED},
};

use crate::capture::Image;

/// Text shorter than this (median word height, px) is recognized again enlarged.
const SMALL_TEXT: f32 = 18.0;
/// Enlargement for small text; more blurs large text and gains nothing on small.
const UPSCALE: i32 = 2;
/// Overlap between strips, in source pixels, so no line is cut in half.
const STRIP_OVERLAP: i32 = 160;

/// A word and its box in source image pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    pub rect: [f32; 4],
    /// The source had a gap before this word, so the text gets a space here
    /// even where the characters alone would not call for one.
    pub spaced: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub words: Vec<Word>,
}

impl Line {
    pub fn bounds(&self) -> [f32; 4] {
        self.words.iter().fold(
            [f32::MAX, f32::MAX, f32::MIN, f32::MIN],
            |[left, top, right, bottom], word| {
                [
                    left.min(word.rect[0]),
                    top.min(word.rect[1]),
                    right.max(word.rect[2]),
                    bottom.max(word.rect[3]),
                ]
            },
        )
    }

    /// The text of `words[from..to]`, spaced the way it reads.
    pub fn text_of(&self, from: usize, to: usize) -> String {
        let mut text = String::new();
        for (index, word) in self.words[from..to].iter().enumerate() {
            if index > 0 && (word.spaced || needs_space(&text, &word.text)) {
                text.push(' ');
            }
            text.push_str(&word.text);
        }
        text
    }

    pub fn text(&self) -> String {
        self.text_of(0, self.words.len())
    }
}

/// A space goes only between two words that both border on Latin letters
/// or digits. The Chinese recognizer returns every character as its own
/// word, and those must not be spaced apart.
fn needs_space(before: &str, word: &str) -> bool {
    before.chars().next_back().is_some_and(is_latin) && word.chars().next().is_some_and(is_latin)
}

fn is_latin(c: char) -> bool {
    c.is_ascii_alphanumeric() || (c.is_alphabetic() && (c as u32) < 0x2e80)
}

/// All recognized text, one line per line.
pub fn text(lines: &[Line]) -> String {
    lines.iter().map(Line::text).collect::<Vec<_>>().join("\r\n")
}

/// Recognizes the text in `image` with the chosen engine. Blocks; run it
/// off the UI thread.
pub fn recognize(image: &Image) -> Result<Vec<Line>, String> {
    match crate::ocr_pack::engine() {
        crate::ocr_pack::Engine::Fast => recognize_windows(image),
        crate::ocr_pack::Engine::Accurate if !crate::ocr_pack::installed() => Err(
            "「准确」档的识别包还没下载完。\n请在设置里「提取文本」旁的齿轮中下载，或者切回「快速」档。".into(),
        ),
        crate::ocr_pack::Engine::Accurate => crate::paddle_ocr::recognize(image, &crate::ocr_pack::folder()?),
    }
}

/// Recognizes with the Windows engine.
pub fn recognize_windows(image: &Image) -> Result<Vec<Line>, String> {
    let lines = recognize_at(image, 1)?;
    match median_height(&lines) {
        Some(height) if height >= SMALL_TEXT => Ok(lines),
        _ => recognize_at(image, UPSCALE),
    }
}

fn median_height(lines: &[Line]) -> Option<f32> {
    let mut heights: Vec<f32> = lines
        .iter()
        .flat_map(|line| &line.words)
        .map(|word| word.rect[3] - word.rect[1])
        .collect();
    heights.sort_by(f32::total_cmp);
    heights.get(heights.len() / 2).copied()
}

fn recognize_at(image: &Image, upscale: i32) -> Result<Vec<Line>, String> {
    unsafe {
        // Already initialized (S_FALSE) or a different model is fine for WinRT calls.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .map_err(|error| format!("无法启动 Windows 文字识别。\n{}", error.message()))?;
    let max = OcrEngine::MaxImageDimension().unwrap_or(10_000) as i32;
    if image.width * upscale > max {
        return Err(format!(
            "图片太宽，无法识别文字。\n宽度 {} 像素，最多支持 {} 像素。",
            image.width,
            max / upscale
        ));
    }
    let mut lines = Vec::new();
    for strip in strips(image.height, max / upscale) {
        let found = recognize_strip(&engine, image, upscale, strip.top, strip.bottom)
            .map_err(|error| format!("文字识别失败。\n{}", error.message()))?;
        lines.extend(found.into_iter().filter(|line| {
            let [_, top, _, bottom] = line.bounds();
            strip.owns((top + bottom) / 2.0)
        }));
    }
    Ok(lines)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Strip {
    pub(crate) top: i32,
    pub(crate) bottom: i32,
    /// The part whose lines this strip keeps; neighbors keep the rest of the overlap.
    keep_top: i32,
    keep_bottom: i32,
}

impl Strip {
    pub(crate) fn owns(self, y: f32) -> bool {
        y >= self.keep_top as f32 && y < self.keep_bottom as f32
    }
}

/// Splits `height` into strips no taller than `limit`, overlapping by
/// `STRIP_OVERLAP`; each line is kept by the strip that owns its center.
pub(crate) fn strips(height: i32, limit: i32) -> Vec<Strip> {
    if height <= limit {
        return vec![Strip {
            top: 0,
            bottom: height,
            keep_top: 0,
            keep_bottom: height,
        }];
    }
    let step = limit - STRIP_OVERLAP;
    let mut strips = Vec::new();
    let mut top = 0;
    loop {
        let bottom = (top + limit).min(height);
        let keep_top = if top == 0 { 0 } else { top + STRIP_OVERLAP / 2 };
        let last = bottom == height;
        let keep_bottom = if last { height } else { top + step + STRIP_OVERLAP / 2 };
        strips.push(Strip {
            top,
            bottom,
            keep_top,
            keep_bottom,
        });
        if last {
            return strips;
        }
        top += step;
    }
}

fn recognize_strip(
    engine: &OcrEngine,
    image: &Image,
    upscale: i32,
    top: i32,
    bottom: i32,
) -> windows::core::Result<Vec<Line>> {
    let width = image.width * upscale;
    let height = (bottom - top) * upscale;
    let pixels = enlarge(image, upscale, top, bottom);
    let writer = DataWriter::new()?;
    writer.WriteBytes(&pixels)?;
    let bitmap =
        SoftwareBitmap::CreateCopyFromBuffer(&writer.DetachBuffer()?, BitmapPixelFormat::Bgra8, width, height)?;
    let result = engine.RecognizeAsync(&bitmap)?.join()?;
    let scale = upscale as f32;
    let found = result.Lines()?;
    let mut lines = Vec::with_capacity(found.Size()? as usize);
    for index in 0..found.Size()? {
        let words = found.GetAt(index)?.Words()?;
        let mut line = Line {
            words: Vec::with_capacity(words.Size()? as usize),
        };
        for index in 0..words.Size()? {
            let word = words.GetAt(index)?;
            let rect = word.BoundingRect()?;
            line.words.push(Word {
                text: word.Text()?.to_string_lossy(),
                spaced: false,
                rect: [
                    rect.X / scale,
                    rect.Y / scale + top as f32,
                    (rect.X + rect.Width) / scale,
                    (rect.Y + rect.Height) / scale + top as f32,
                ],
            });
        }
        if !line.words.is_empty() {
            lines.push(line);
        }
    }
    Ok(lines)
}

/// Rows `top..bottom` of `image`, enlarged `upscale`× with Catmull-Rom
/// interpolation: smooth edges read far better than blocky repeated pixels.
fn enlarge(image: &Image, upscale: i32, top: i32, bottom: i32) -> Vec<u8> {
    let width = image.width as usize;
    let height = (bottom - top) as usize;
    let gray: Vec<f32> = image.pixels[top as usize * width * 4..bottom as usize * width * 4]
        .chunks_exact(4)
        .map(|p| 0.114 * p[0] as f32 + 0.587 * p[1] as f32 + 0.299 * p[2] as f32)
        .collect();
    if upscale == 1 {
        return gray.iter().flat_map(|&v| [v as u8, v as u8, v as u8, 255]).collect();
    }
    let n = upscale as usize;
    let taps = weights(n);
    // Horizontal pass, then vertical.
    let wide_w = width * n;
    let mut wide = vec![0.0f32; wide_w * height];
    for y in 0..height {
        let row = &gray[y * width..(y + 1) * width];
        for x in 0..wide_w {
            let (base, w) = &taps[x % n];
            let center = (x / n) as isize + base;
            wide[y * wide_w + x] = (0..4)
                .map(|k| w[k] * row[(center + k as isize).clamp(0, width as isize - 1) as usize])
                .sum();
        }
    }
    let mut pixels = Vec::with_capacity(wide_w * height * n * 4);
    for y in 0..height * n {
        let (base, w) = &taps[y % n];
        let center = (y / n) as isize + base;
        for x in 0..wide_w {
            let value: f32 = (0..4)
                .map(|k| {
                    let row = (center + k as isize).clamp(0, height as isize - 1) as usize;
                    w[k] * wide[row * wide_w + x]
                })
                .sum();
            let value = value.round().clamp(0.0, 255.0) as u8;
            pixels.extend_from_slice(&[value, value, value, 255]);
        }
    }
    pixels
}

/// For each output phase of an `n`× enlargement: the offset of the first of
/// four source taps, and their Catmull-Rom weights.
fn weights(n: usize) -> Vec<(isize, [f32; 4])> {
    let cubic = |t: f32| {
        let t = t.abs();
        if t < 1.0 {
            1.5 * t * t * t - 2.5 * t * t + 1.0
        } else if t < 2.0 {
            -0.5 * t * t * t + 2.5 * t * t - 4.0 * t + 2.0
        } else {
            0.0
        }
    };
    (0..n)
        .map(|phase| {
            // Output pixel center mapped back into source pixel space.
            let source = (phase as f32 + 0.5) / n as f32 - 0.5;
            let first = source.floor() as isize - 1;
            let mut w = [0.0; 4];
            for (k, weight) in w.iter_mut().enumerate() {
                *weight = cubic(source - (first + k as isize) as f32);
            }
            let total: f32 = w.iter().sum();
            (first, w.map(|weight| weight / total))
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn word(text: &str, left: f32) -> Word {
        Word {
            text: text.into(),
            rect: [left, 0.0, left + 10.0, 10.0],
            spaced: false,
        }
    }

    fn join(words: &[&str]) -> String {
        let line = Line { words: words.iter().map(|text| word(text, 0.0)).collect() };
        line.text()
    }

    #[test]
    fn chinese_characters_join_without_spaces_and_latin_words_with() {
        assert_eq!(join(&["识", "别", "文", "字"]), "识别文字");
        assert_eq!(join(&["Hello", "world"]), "Hello world");
        assert_eq!(join(&["打开", "Snapshot", "设置", "v0.1"]), "打开Snapshot设置v0.1");
        assert_eq!(join(&["Ctrl+C", "复制"]), "Ctrl+C复制");
        let mut gap = Line { words: vec![word("PY:", 0.0), word("Warning:", 30.0)] };
        assert_eq!(gap.text(), "PY:Warning:");
        gap.words[1].spaced = true;
        assert_eq!(gap.text(), "PY: Warning:", "a gap in the source keeps its space");
        let line = Line {
            words: vec![word("快捷键", 0.0), word("Ctrl", 20.0), word("8", 40.0)],
        };
        assert_eq!(line.text_of(1, 3), "Ctrl 8");
        assert_eq!(line.bounds(), [0.0, 0.0, 50.0, 10.0]);
    }

    #[test]
    fn strips_overlap_and_every_row_has_one_owner() {
        assert_eq!(strips(300, 5000).len(), 1);
        let strips = strips(12_000, 5000);
        assert_eq!(strips.first().unwrap().top, 0);
        assert_eq!(strips.last().unwrap().bottom, 12_000);
        for pair in strips.windows(2) {
            assert!(pair[1].top < pair[0].bottom, "neighbors overlap");
            assert_eq!(pair[0].keep_bottom, pair[1].keep_top, "no gap, no double");
        }
        for y in [0.0, 4_900.0, 4_920.0, 9_700.0, 11_999.0] {
            assert_eq!(strips.iter().filter(|strip| strip.owns(y)).count(), 1, "row {y}");
        }
        assert!(strips.iter().all(|strip| strip.bottom - strip.top <= 5000));
    }

    #[test]
    fn median_height_picks_the_middle_word() {
        let mut line = Line { words: vec![word("a", 0.0), word("b", 20.0), word("c", 40.0)] };
        line.words[0].rect[3] = 30.0;
        assert_eq!(median_height(&[line]), Some(10.0));
        assert_eq!(median_height(&[]), None);
    }

    #[test]
    fn enlarging_is_smooth_gray_and_opaque() {
        let image = Image {
            width: 2,
            height: 2,
            pixels: vec![1, 1, 1, 0, 2, 2, 2, 0, 3, 3, 3, 0, 4, 4, 4, 0],
        };
        let big = enlarge(&image, 2, 1, 2);
        assert_eq!(big.len(), 4 * 2 * 4);
        // Gray, opaque, and between the two source values of that row.
        for pixel in big.chunks_exact(4) {
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[3], 255);
            assert!((3..=4).contains(&pixel[0]), "{pixel:?}");
        }
        // A flat image stays flat.
        let flat = Image { width: 3, height: 3, pixels: [200, 200, 200, 0].repeat(9) };
        assert!(enlarge(&flat, 3, 0, 3).chunks_exact(4).all(|p| p[0] == 200));
    }

    /// Recognizes a rendered sample with the real Windows engine.
    /// Run with `<test binary> ocr --ignored --nocapture`.
    #[test]
    #[ignore]
    fn recognizes_rendered_text() {
        // The engine's floor: text from about 16 px reads right; 12–14 px is
        // printed for reference (enlarging helps, but some letters still slip).
        for px in [12, 13, 14, 16, 18, 20, 24] {
            let image = unsafe { sample("识别文字 Snapshot 保存到 C:\\Users\\eric\\Pictures", px, true) };
            let found = text(&recognize_windows(&image).unwrap());
            println!("{px}px: {found}");
            if px >= 16 {
                for expected in ["识别文字", "保存到", "Users", "Pictures"] {
                    assert!(found.contains(expected), "{px}px: {found}");
                }
            }
        }
    }

    /// `value` in Microsoft YaHei at `px` on white, like a screenshot.
    pub(crate) unsafe fn sample(value: &str, px: i32, cleartype: bool) -> Image {
        use crate::{capture::{Desktop, Surface}, ui};
        use windows_sys::{core::w, Win32::{Foundation::RECT, Graphics::Gdi::*}};
        let bounds = RECT { left: 0, top: 0, right: px * 30, bottom: px * 3 };
        let surface = Surface::new(std::ptr::null_mut(), bounds.right, bounds.bottom).unwrap();
        ui::fill(surface.dc, bounds, 0x00ff_ffff);
        let quality = if cleartype { CLEARTYPE_QUALITY } else { ANTIALIASED_QUALITY };
        let font = CreateFontW(-px, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET as u32, 0, 0, quality as u32, 0, w!("Microsoft YaHei UI"));
        ui::text(surface.dc, bounds, value, font, 0, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        GdiFlush();
        DeleteObject(font);
        Desktop { bounds, surface }.crop(bounds).unwrap()
    }
}
