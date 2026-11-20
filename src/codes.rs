//! QR codes and barcodes in a capture, found with rxing (a port of ZXing).
//! Runs next to text recognition on the same background thread.

use rxing::{helpers::detect_multiple_in_luma_with_hints, BarcodeFormat, DecodeHints};

use crate::capture::Image;

/// Codes this small are noise from text or textures.
const MIN_SIZE: f32 = 12.0;
/// Barcode readers report a scan line; this much height is added around it.
const BARCODE_HALF_HEIGHT: f32 = 14.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Code {
    pub text: String,
    /// Two-dimensional (QR, Data Matrix…) rather than a one-dimensional barcode.
    pub matrix: bool,
    /// Bounds in image pixels: left, top, right, bottom.
    pub rect: [f32; 4],
}

impl Code {
    pub fn kind(&self) -> &'static str {
        if self.matrix {
            "二维码"
        } else {
            "条形码"
        }
    }

    /// The content as a web address, if it is one.
    pub fn link(&self) -> Option<&str> {
        let text = self.text.trim();
        let lower = text.to_ascii_lowercase();
        (lower.starts_with("https://") || lower.starts_with("http://")).then_some(text)
    }
}

fn is_matrix(format: &BarcodeFormat) -> bool {
    matches!(
        format,
        BarcodeFormat::QR_CODE
            | BarcodeFormat::MICRO_QR_CODE
            | BarcodeFormat::RECTANGULAR_MICRO_QR_CODE
            | BarcodeFormat::DATA_MATRIX
            | BarcodeFormat::AZTEC
            | BarcodeFormat::PDF_417
            | BarcodeFormat::MAXICODE
    )
}

/// Every code in `image`. Finding none is not an error.
pub fn detect(image: &Image) -> Vec<Code> {
    if image.width < MIN_SIZE as i32 || image.height < MIN_SIZE as i32 {
        return Vec::new();
    }
    let luma = image
        .pixels
        .chunks_exact(4)
        .map(|bgra| ((u32::from(bgra[2]) * 77 + u32::from(bgra[1]) * 150 + u32::from(bgra[0]) * 29) >> 8) as u8)
        .collect();
    let mut hints = DecodeHints {
        TryHarder: Some(true),
        // Dark-mode screens often show light codes on dark backgrounds.
        AlsoInverted: Some(true),
        ..Default::default()
    };
    let Ok(results) = detect_multiple_in_luma_with_hints(luma, image.width as u32, image.height as u32, &mut hints)
    else {
        return Vec::new();
    };
    let mut codes: Vec<Code> = Vec::new();
    for result in results {
        let points = result.getPoints();
        if points.is_empty() || result.getText().is_empty() {
            continue;
        }
        let matrix = is_matrix(result.getBarcodeFormat());
        let (mut left, mut top, mut right, mut bottom) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for point in points {
            left = left.min(point.x);
            top = top.min(point.y);
            right = right.max(point.x);
            bottom = bottom.max(point.y);
        }
        if matrix {
            // Finder pattern centers sit inside the corners; pad by about one module.
            let pad = ((right - left).max(bottom - top) * 0.08).max(4.0);
            (left, top, right, bottom) = (left - pad, top - pad, right + pad, bottom + pad);
        } else {
            let middle = (top + bottom) / 2.0;
            (top, bottom) = (middle - BARCODE_HALF_HEIGHT, middle + BARCODE_HALF_HEIGHT);
        }
        let rect = [
            left.max(0.0),
            top.max(0.0),
            right.min(image.width as f32),
            bottom.min(image.height as f32),
        ];
        if rect[2] - rect[0] < MIN_SIZE || rect[3] - rect[1] < MIN_SIZE {
            continue;
        }
        let code = Code { text: result.getText().to_string(), matrix, rect };
        // The multi reader can report one code twice; keep the first.
        if !codes.iter().any(|other| other.text == code.text && overlaps(other.rect, code.rect)) {
            codes.push(code);
        }
    }
    codes.sort_by(|a, b| a.rect[1].total_cmp(&b.rect[1]).then(a.rect[0].total_cmp(&b.rect[0])));
    codes
}

fn overlaps(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rxing::{BarcodeFormat, MultiFormatWriter, Writer};

    /// White image with each code drawn at `(format, text, left, top, module)`.
    pub(crate) fn render(width: i32, height: i32, codes: &[(BarcodeFormat, &str, i32, i32, i32)]) -> Image {
        let mut pixels = vec![255u8; (width * height * 4) as usize];
        for &(format, text, left, top, module) in codes {
            let size = if format == BarcodeFormat::QR_CODE { (0, 0) } else { (220, 60) };
            let matrix = MultiFormatWriter.encode(text, &format, size.0, size.1).unwrap();
            for y in 0..matrix.getHeight() as i32 * module {
                for x in 0..matrix.getWidth() as i32 * module {
                    if matrix.get((x / module) as u32, (y / module) as u32) {
                        let index = (((top + y) * width + left + x) * 4) as usize;
                        pixels[index..index + 3].fill(0);
                    }
                }
            }
        }
        Image { width, height, pixels }
    }

    #[test]
    fn finds_qr_codes_and_barcodes_with_their_bounds() {
        let image = render(
            700,
            320,
            &[
                (BarcodeFormat::QR_CODE, "https://github.com/AllToBeNize/snapshot", 30, 30, 6),
                (BarcodeFormat::CODE_128, "SNAP-2026-1120", 360, 200, 1),
            ],
        );
        let codes = detect(&image);
        assert_eq!(codes.len(), 2, "{codes:?}");
        let qr = &codes[0];
        assert!(qr.matrix);
        assert_eq!(qr.kind(), "二维码");
        assert_eq!(qr.link(), Some("https://github.com/AllToBeNize/snapshot"));
        // The symbol starts after a 4-module quiet zone: 30 + 4 × 6 = 54.
        assert!((30.0..=54.0).contains(&qr.rect[0]) && (30.0..=54.0).contains(&qr.rect[1]), "{:?}", qr.rect);
        assert!(qr.rect[2] > 200.0, "{:?}", qr.rect);
        let bar = &codes[1];
        assert!(!bar.matrix);
        assert_eq!(bar.text, "SNAP-2026-1120");
        assert_eq!(bar.link(), None);
        assert!(bar.rect[0] >= 350.0 && bar.rect[1] >= 180.0, "{:?}", bar.rect);
    }

    #[test]
    fn plain_text_has_no_codes() {
        let image = unsafe { crate::ocr::tests::sample("没有二维码 No codes here 12345", 18, true) };
        assert!(detect(&image).is_empty());
    }

    #[test]
    fn light_codes_on_dark_backgrounds_are_found() {
        let mut image = render(260, 260, &[(BarcodeFormat::QR_CODE, "暗色模式", 20, 20, 6)]);
        for pixel in image.pixels.chunks_exact_mut(4) {
            for channel in &mut pixel[..3] {
                *channel = 255 - *channel;
            }
        }
        let codes = detect(&image);
        assert_eq!(codes.first().map(|code| code.text.as_str()), Some("暗色模式"));
    }
}
