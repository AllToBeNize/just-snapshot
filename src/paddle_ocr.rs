//! Accurate text recognition: PaddleOCR's PP-OCRv5 mobile detector and
//! recognizer on ONNX Runtime, both loaded from the downloaded pack
//! (`ocr_pack`). Pre- and post-processing follow RapidOCR (Apache-2.0):
//! DB text detection, then CTC recognition of every detected line.
//!
//! Screenshot text is upright, so the detector's regions are taken as
//! axis-aligned boxes (RapidOCR fits rotated rectangles) and there is no
//! angle classifier.

use std::{path::Path, sync::OnceLock};

use ort::{environment::Environment, session::Session, value::Tensor};

use crate::{
    capture::Image,
    ocr::{self, Line, Word},
};

/// The detector input's short side is enlarged to at least this.
const DET_SHORT_SIDE: f32 = 736.0;
/// ... and its long side kept within this.
const DET_LONG_SIDE: f32 = 2560.0;
/// Taller images are detected in overlapping strips of this many source rows.
const DET_STRIP: i32 = 2000;
const DET_THRESHOLD: f32 = 0.3;
const BOX_THRESHOLD: f32 = 0.5;
const UNCLIP_RATIO: f32 = 1.6;
const MIN_BOX_SIDE: f32 = 3.0;
const MAX_CANDIDATES: usize = 1000;
const REC_HEIGHT: usize = 48;
/// The narrowest batch the recognizer is fed, as width / height.
const REC_MIN_RATIO: f32 = 320.0 / 48.0;
const REC_BATCH: usize = 6;
/// Lines recognized with less average confidence are dropped.
const TEXT_SCORE: f32 = 0.5;
/// Characters this many output columns apart start a new word (RapidOCR's rule).
const WORD_GAP_COLUMNS: usize = 4;
/// Boxes on the same row closer than this many line heights are one line.
const ROW_GAP: f32 = 1.5;

pub const DETECTOR: &str = "ch_PP-OCRv5_mobile_det.onnx";
pub const RECOGNIZER: &str = "ch_PP-OCRv5_rec_mobile_infer.onnx";
pub const RUNTIME: &str = "onnxruntime.dll";

/// ONNX Runtime can be loaded once per process; the first folder wins.
static ENVIRONMENT: OnceLock<Result<Environment, String>> = OnceLock::new();

fn environment(folder: &Path) -> Result<&'static Environment, String> {
    ENVIRONMENT
        .get_or_init(|| {
            ort::init_from(folder.join(RUNTIME))
                .map_err(|error| format!("无法加载识别运行库 {RUNTIME}。\n{error}"))?
                .build()
                .map_err(|error| format!("无法启动识别运行库。\n{error}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

struct Engine {
    detector: Session,
    recognizer: Session,
    /// Index 0 is the CTC blank, the last is a space.
    characters: Vec<String>,
}

impl Engine {
    fn load(folder: &Path) -> Result<Self, String> {
        let environment = environment(folder)?;
        let session = |name: &str| -> Result<Session, String> {
            Session::builder(environment)
                .and_then(|mut builder| builder.commit_from_file(folder.join(name)))
                .map_err(|error| format!("无法加载识别模型 {name}。\n{error}"))
        };
        let detector = session(DETECTOR)?;
        let recognizer = session(RECOGNIZER)?;
        let list = recognizer
            .metadata()
            .ok()
            .and_then(|metadata| metadata.custom("character"))
            .ok_or_else(|| format!("识别模型 {RECOGNIZER} 里没有字符表。"))?;
        let characters = std::iter::once(String::new())
            .chain(list.lines().map(str::to_string))
            .chain(std::iter::once(" ".to_string()))
            .collect();
        Ok(Self { detector, recognizer, characters })
    }
}

/// Recognizes the text in `image` with the models in `folder`. Blocks; run
/// it off the UI thread. The models load per call, so nothing stays in
/// memory between recognitions.
pub fn recognize(image: &Image, folder: &Path) -> Result<Vec<Line>, String> {
    let trace = std::env::var_os("SNAPSHOT_OCR_TRACE").is_some();
    let start = std::time::Instant::now();
    let mut engine = Engine::load(folder)?;
    if trace {
        eprintln!("load {:?}", start.elapsed());
    }
    let mut boxes = Vec::new();
    for strip in ocr::strips(image.height, DET_STRIP) {
        let rows = Region { left: 0, top: strip.top, right: image.width, bottom: strip.bottom };
        let found = detect(&mut engine.detector, image, rows).map_err(|error| format!("文字检测失败。\n{error}"))?;
        boxes.extend(found.into_iter().filter(|found| strip.owns((found.top + found.bottom) as f32 / 2.0)));
    }
    if trace {
        eprintln!("detect {:?} ({} boxes)", start.elapsed(), boxes.len());
    }
    let mut boxes = join_touching(boxes);
    sort_reading_order(&mut boxes);
    let texts = recognize_boxes(&mut engine, image, &boxes).map_err(|error| format!("文字识别失败。\n{error}"))?;
    if trace {
        eprintln!("recognize {:?}", start.elapsed());
    }
    let pieces: Vec<(Region, Line)> = boxes
        .iter()
        .zip(texts)
        .filter_map(|(region, decoded)| decoded.filter(|decoded| decoded.score >= TEXT_SCORE).map(|decoded| (*region, line(*region, &decoded))))
        .filter(|(_, line)| !line.words.is_empty())
        .collect();
    Ok(merge_rows(pieces))
}

/// Small text gets cut inside identifiers, mostly at underscores; read
/// apart, each piece sees half the character at the cut (`user_` +
/// `_spec`, or `user.` + `_name`). Boxes on one row that touch, at the
/// same text height, are read as one box instead: the recognizer reads the
/// whole run and puts any spaces in itself.
fn join_touching(mut boxes: Vec<Region>) -> Vec<Region> {
    boxes.sort_by_key(|region| (region.left, region.top));
    let mut joined: Vec<Region> = Vec::new();
    for region in boxes {
        let touches = joined.iter().rposition(|row| {
            let (low, high) = (row.height().min(region.height()), row.height().max(region.height()));
            let overlap = row.bottom.min(region.bottom) - row.top.max(region.top);
            let gap = region.left - row.right;
            high * 2 <= low * 3 && overlap * 10 >= low * 7 && gap * 2 <= low && region.right > row.right
        });
        match touches {
            Some(index) => {
                let row = &mut joined[index];
                *row = Region {
                    left: row.left.min(region.left),
                    top: row.top.min(region.top),
                    right: row.right.max(region.right),
                    bottom: row.bottom.max(region.bottom),
                };
            }
            None => joined.push(region),
        }
    }
    joined
}

/// The detector splits a line at wide gaps (log columns, `key: value`);
/// boxes side by side on one row and close enough join into one line, the
/// gap kept as a space. Far apart boxes (table columns) stay separate.
fn merge_rows(pieces: Vec<(Region, Line)>) -> Vec<Line> {
    let mut rows: Vec<(Region, Line)> = Vec::new();
    for (region, mut line) in pieces {
        let joins = rows.iter().rposition(|(row, _)| {
            let overlap = row.bottom.min(region.bottom) - row.top.max(region.top);
            let height = row.height().max(region.height()) as f32;
            let gap = region.left - row.right;
            overlap * 2 >= row.height().min(region.height()) && gap >= -(height as i32) / 2 && (gap as f32) < height * ROW_GAP
        });
        match joins {
            Some(index) => {
                let (row, existing) = &mut rows[index];
                join_words(&mut existing.words, &mut line.words);
                *row = Region {
                    left: row.left.min(region.left),
                    top: row.top.min(region.top),
                    right: row.right.max(region.right),
                    bottom: row.bottom.max(region.bottom),
                };
            }
            None => rows.push((region, line)),
        }
    }
    rows.into_iter().map(|(_, line)| line).collect()
}

/// Appends the words of a box to the row before it. Small text gets split
/// inside identifiers (at underscores mostly), so the boxes only read as
/// apart where their characters leave a gap; where the boxes overlap, a
/// character both of them read is kept once (`user_` + `_name`).
fn join_words(row: &mut Vec<Word>, words: &mut Vec<Word>) {
    let (Some(before), Some(first)) = (row.last(), words.first_mut()) else {
        row.append(words);
        return;
    };
    let height = (first.rect[3] - first.rect[1]).max(before.rect[3] - before.rect[1]);
    let character = (before.rect[2] - before.rect[0]) / before.text.chars().count().max(1) as f32;
    let overlap = before.rect[2] - first.rect[0];
    let repeated = before.text.chars().last().filter(|&last| first.text.starts_with(last));
    // A cut right on an underscore leaves it at the edge of both boxes.
    let twice = |last: char| overlap >= character * 0.3 || (last == '_' && overlap >= -height * 0.2);
    if let Some(last) = repeated.filter(|&last| twice(last)) {
        first.text.drain(..last.len_utf8());
        first.rect[0] = first.rect[0].max(before.rect[2]);
    }
    // Word boxes reach a little past their characters, so a space between
    // words leaves a gap of about a third of the height, a cut inside a word
    // none.
    first.spaced = first.rect[0] - before.rect[2] > height * 0.2;
    if first.text.is_empty() {
        words.remove(0);
    }
    row.append(words);
}

/// Pixel bounds in the source image, right and bottom exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Region {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Region {
    fn width(self) -> i32 {
        self.right - self.left
    }

    fn height(self) -> i32 {
        self.bottom - self.top
    }
}

/// `region` of `image` resized to `width` × `height`, normalized to
/// (x / 255 - 0.5) / 0.5 per channel in B, G, R order (what the models were
/// trained on through OpenCV), laid out channel by channel.
fn normalized(image: &Image, region: Region, width: usize, height: usize, stride: usize, output: &mut [f32]) {
    let plane = height * stride;
    let scale_x = region.width() as f32 / width as f32;
    let scale_y = region.height() as f32 / height as f32;
    let pixel = |x: i32, y: i32, channel: usize| -> f32 {
        let x = (region.left + x).clamp(0, image.width - 1);
        let y = (region.top + y).clamp(0, image.height - 1);
        f32::from(image.pixels[((y * image.width + x) * 4) as usize + channel])
    };
    // Bilinear with pixel centers aligned, like cv2.resize.
    for row in 0..height {
        let source_y = ((row as f32 + 0.5) * scale_y - 0.5).max(0.0);
        let y0 = source_y.floor() as i32;
        let y1 = (y0 + 1).min(region.height() - 1);
        let fy = source_y - y0 as f32;
        for column in 0..width {
            let source_x = ((column as f32 + 0.5) * scale_x - 0.5).max(0.0);
            let x0 = source_x.floor() as i32;
            let x1 = (x0 + 1).min(region.width() - 1);
            let fx = source_x - x0 as f32;
            for channel in 0..3 {
                let top = pixel(x0, y0, channel) * (1.0 - fx) + pixel(x1, y0, channel) * fx;
                let bottom = pixel(x0, y1, channel) * (1.0 - fx) + pixel(x1, y1, channel) * fx;
                let value = top * (1.0 - fy) + bottom * fy;
                output[channel * plane + row * stride + column] = value / 127.5 - 1.0;
            }
        }
    }
}

/// The detector's input size for a `width` × `height` region: the short
/// side enlarged to `DET_SHORT_SIDE`, the long side kept within
/// `DET_LONG_SIDE`, both rounded to a multiple of 32.
fn detector_size(width: i32, height: i32) -> (usize, usize) {
    let (short, long) = (width.min(height) as f32, width.max(height) as f32);
    let ratio = (DET_SHORT_SIDE / short).max(1.0).min(DET_LONG_SIDE / long);
    let round = |side: i32| (((side as f32 * ratio) / 32.0).round() as usize * 32).max(32);
    (round(width), round(height))
}

fn detect(detector: &mut Session, image: &Image, region: Region) -> Result<Vec<Region>, ort::Error> {
    let (width, height) = detector_size(region.width(), region.height());
    let mut input = vec![0f32; 3 * width * height];
    normalized(image, region, width, height, width, &mut input);
    let name = output_name(detector)?;
    let outputs = detector.run(ort::inputs![Tensor::from_array(([1usize, 3, height, width], input))?])?;
    let (_, probabilities) = outputs[name.as_str()].try_extract_tensor::<f32>()?;
    let scale_x = region.width() as f32 / width as f32;
    let scale_y = region.height() as f32 / height as f32;
    Ok(boxes_from_map(probabilities, width, height)
        .into_iter()
        .map(|[left, top, right, bottom]| Region {
            left: (region.left + (left * scale_x).round() as i32).clamp(0, image.width),
            top: (region.top + (top * scale_y).round() as i32).clamp(0, image.height),
            right: (region.left + (right * scale_x).round() as i32).clamp(0, image.width),
            bottom: (region.top + (bottom * scale_y).round() as i32).clamp(0, image.height),
        })
        .filter(|region| region.width() > 3 && region.height() > 3)
        .collect())
}

/// The model's only output's name.
fn output_name(session: &Session) -> Result<String, ort::Error> {
    session.outputs().first().map(|output| output.name().to_string()).ok_or_else(|| ort::Error::new("模型没有输出"))
}

/// DB post-processing on the probability map: threshold, dilate 2×2, take
/// each connected region's box, keep confident ones and grow them back out
/// by area × ratio / perimeter (the detector predicts shrunken text).
/// Returns `[left, top, right, bottom]` in map pixels.
fn boxes_from_map(probabilities: &[f32], width: usize, height: usize) -> Vec<[f32; 4]> {
    let above: Vec<bool> = probabilities.iter().map(|&p| p > DET_THRESHOLD).collect();
    let mut mask = vec![false; width * height];
    for y in 0..height {
        for x in 0..width {
            let at = |dx: usize, dy: usize| x >= dx && y >= dy && above[(y - dy) * width + x - dx];
            mask[y * width + x] = at(0, 0) || at(1, 0) || at(0, 1) || at(1, 1);
        }
    }
    let mut seen = vec![false; width * height];
    let mut stack = Vec::new();
    let mut boxes = Vec::new();
    let mut candidates = 0;
    for start in 0..width * height {
        if !mask[start] || seen[start] {
            continue;
        }
        candidates += 1;
        if candidates > MAX_CANDIDATES {
            break;
        }
        let (mut left, mut top, mut right, mut bottom) = (usize::MAX, usize::MAX, 0, 0);
        seen[start] = true;
        stack.push(start);
        while let Some(index) = stack.pop() {
            let (x, y) = (index % width, index / width);
            (left, top, right, bottom) = (left.min(x), top.min(y), right.max(x), bottom.max(y));
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if nx < 0 || ny < 0 || nx >= width as i32 || ny >= height as i32 {
                        continue;
                    }
                    let next = ny as usize * width + nx as usize;
                    if mask[next] && !seen[next] {
                        seen[next] = true;
                        stack.push(next);
                    }
                }
            }
        }
        let (box_width, box_height) = ((right - left) as f32, (bottom - top) as f32);
        if box_width.min(box_height) < MIN_BOX_SIDE {
            continue;
        }
        let mut sum = 0.0;
        for y in top..=bottom {
            sum += probabilities[y * width + left..=y * width + right].iter().sum::<f32>();
        }
        let score = sum / ((right - left + 1) * (bottom - top + 1)) as f32;
        if score < BOX_THRESHOLD {
            continue;
        }
        let distance = box_width * box_height * UNCLIP_RATIO / (2.0 * (box_width + box_height));
        if box_width.min(box_height) + distance * 2.0 < MIN_BOX_SIDE + 2.0 {
            continue;
        }
        boxes.push([
            (left as f32 - distance).max(0.0),
            (top as f32 - distance).max(0.0),
            (right as f32 + distance).min(width as f32),
            (bottom as f32 + distance).min(height as f32),
        ]);
    }
    boxes
}

/// Top to bottom; boxes within 10 px of each other's top read left to right.
fn sort_reading_order(boxes: &mut [Region]) {
    boxes.sort_by_key(|region| (region.top, region.left));
    // RapidOCR's pass: a box barely lower but further left comes first.
    for end in 1..boxes.len() {
        let mut index = end;
        while index > 0 && (boxes[index].top - boxes[index - 1].top).abs() < 10 && boxes[index].left < boxes[index - 1].left {
            boxes.swap(index, index - 1);
            index -= 1;
        }
    }
}

/// A recognized line before it is split into words.
struct Decoded {
    /// Each character with the output column it was read at.
    characters: Vec<(String, usize)>,
    score: f32,
    /// Output columns that cover the line itself, not the batch padding.
    columns: f32,
}

fn recognize_boxes(engine: &mut Engine, image: &Image, boxes: &[Region]) -> Result<Vec<Option<Decoded>>, ort::Error> {
    let ratio = |region: &Region| region.width() as f32 / region.height().max(1) as f32;
    let mut order: Vec<usize> = (0..boxes.len()).collect();
    order.sort_by(|&a, &b| ratio(&boxes[a]).total_cmp(&ratio(&boxes[b])));
    let mut results: Vec<Option<Decoded>> = (0..boxes.len()).map(|_| None).collect();
    for batch in order.chunks(REC_BATCH) {
        let widest = batch.iter().map(|&index| ratio(&boxes[index])).fold(REC_MIN_RATIO, f32::max);
        let width = (REC_HEIGHT as f32 * widest) as usize;
        let mut input = vec![0f32; batch.len() * 3 * REC_HEIGHT * width];
        for (slot, &index) in batch.iter().enumerate() {
            let resized = ((REC_HEIGHT as f32 * ratio(&boxes[index])).ceil() as usize).clamp(1, width);
            let start = slot * 3 * REC_HEIGHT * width;
            normalized(image, boxes[index], resized, REC_HEIGHT, width, &mut input[start..start + 3 * REC_HEIGHT * width]);
        }
        let name = output_name(&engine.recognizer)?;
        let outputs = engine.recognizer.run(ort::inputs![Tensor::from_array(([batch.len(), 3, REC_HEIGHT, width], input))?])?;
        let (shape, scores) = outputs[name.as_str()].try_extract_tensor::<f32>()?;
        let (steps, classes) = (shape[1] as usize, shape[2] as usize);
        for (slot, &index) in batch.iter().enumerate() {
            let rows = &scores[slot * steps * classes..(slot + 1) * steps * classes];
            let mut decoded = decode(rows, steps, classes, &engine.characters);
            decoded.columns = steps as f32 * (ratio(&boxes[index]) / widest).min(1.0);
            results[index] = Some(decoded);
        }
    }
    Ok(results)
}

/// Greedy CTC: the best class per column, repeats merged, blanks dropped.
fn decode(rows: &[f32], steps: usize, classes: usize, characters: &[String]) -> Decoded {
    let mut picked = Vec::new();
    let mut confidence = Vec::new();
    let mut previous = 0;
    for step in 0..steps {
        let row = &rows[step * classes..(step + 1) * classes];
        let (best, probability) = row
            .iter()
            .enumerate()
            .fold((0, f32::MIN), |best, (class, &p)| if p > best.1 { (class, p) } else { best });
        if best != 0 && best != previous {
            if let Some(character) = characters.get(best) {
                picked.push((character.clone(), step));
                confidence.push(probability);
            }
        }
        previous = best;
    }
    let score = if confidence.is_empty() { 0.0 } else { confidence.iter().sum::<f32>() / confidence.len() as f32 };
    Decoded { characters: picked, score, columns: steps as f32 }
}

/// CJK characters and full-width punctuation stand alone as words.
fn is_cjk(text: &str) -> bool {
    text.chars().next().is_some_and(|c| {
        ('\u{2e80}'..='\u{9fff}').contains(&c)
            || ('\u{f900}'..='\u{faff}').contains(&c)
            || ('\u{ff00}'..='\u{ffef}').contains(&c)
            || ('\u{3000}'..='\u{303f}').contains(&c)
    })
}

/// Splits a recognized line into words the way the Windows recognizer
/// returns them: each CJK character on its own, Latin runs at spaces. Each
/// word's box spans its characters' columns.
fn line(region: Region, decoded: &Decoded) -> Line {
    let characters = &decoded.characters;
    let column_width = region.width() as f32 / decoded.columns.max(1.0);
    // A character reaches halfway to its neighbors' columns, at most a
    // column and a half.
    let edges: Vec<(f32, f32)> = (0..characters.len())
        .map(|index| {
            let column = characters[index].1 as f32;
            let before = if index > 0 { (column - characters[index - 1].1 as f32) / 2.0 } else { 1.5 };
            let after = if index + 1 < characters.len() { (characters[index + 1].1 as f32 - column) / 2.0 } else { 1.5 };
            let left = region.left as f32 + ((column - before.min(1.5)) * column_width).max(0.0);
            let right = region.left as f32 + ((column + 1.0 + after.min(1.5)) * column_width).min(region.width() as f32);
            (left, right)
        })
        .collect();
    // (text, left, right, spaced)
    let mut words = Vec::new();
    let mut current: Option<(String, f32, f32, bool)> = None;
    let mut pending_space = false;
    let finish = |current: &mut Option<(String, f32, f32, bool)>, words: &mut Vec<Word>| {
        if let Some((text, left, right, spaced)) = current.take() {
            words.push(Word { text, rect: [left, region.top as f32, right, region.bottom as f32], spaced });
        }
    };
    for (index, ((text, column), &(left, right))) in characters.iter().zip(&edges).enumerate() {
        let gap = index > 0 && column - characters[index - 1].1 > WORD_GAP_COLUMNS;
        if text.trim().is_empty() {
            finish(&mut current, &mut words);
            pending_space = true;
            continue;
        }
        if gap {
            finish(&mut current, &mut words);
            // A wide gap between Latin text reads as a space; between CJK it is just spacing.
            pending_space |= !is_cjk(text) && !words.last().is_some_and(|word: &Word| is_cjk(&word.text));
        }
        let spaced = std::mem::take(&mut pending_space) && !words.is_empty();
        if is_cjk(text) {
            finish(&mut current, &mut words);
            current = Some((text.clone(), left, right, spaced));
            finish(&mut current, &mut words);
        } else {
            match &mut current {
                Some((word, _, end, _)) => {
                    word.push_str(text);
                    *end = right;
                }
                None => current = Some((text.clone(), left, right, spaced)),
            }
        }
    }
    finish(&mut current, &mut words);
    Line { words }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_input_keeps_text_readable_and_sizes_multiple_of_32() {
        // A small capture is enlarged so its short side reaches 736.
        assert_eq!(detector_size(400, 200), (1472, 736));
        // A wide single line is not blown up past the long-side limit.
        assert_eq!(detector_size(1500, 30), (2560, 64));
        // A screen is left about as it is.
        assert_eq!(detector_size(1920, 1080), (1920, 1088));
    }

    #[test]
    fn map_regions_become_grown_boxes_and_weak_ones_drop() {
        let (width, height) = (40, 20);
        let mut map = vec![0f32; width * height];
        // A confident word at x 4..=15, y 5..=10; a faint smudge at x 25..=30.
        for y in 5..=10 {
            for x in 4..=15 {
                map[y * width + x] = 0.9;
            }
            for x in 25..=30 {
                map[y * width + x] = 0.35;
            }
        }
        let boxes = boxes_from_map(&map, width, height);
        assert_eq!(boxes.len(), 1, "{boxes:?}");
        let [left, top, right, bottom] = boxes[0];
        assert!(left < 4.0 && top < 5.0 && right > 16.0 && bottom > 11.0, "grown: {:?}", boxes[0]);
    }

    #[test]
    fn ctc_merges_repeats_drops_blanks_and_words_split_like_windows() {
        let characters: Vec<String> = ["", "B", "P", "_", "O", "x", "中", "文", " "].iter().map(|c| c.to_string()).collect();
        // Columns: B B _blank P _ O x blank space 中 文
        let path = [1, 1, 0, 2, 3, 4, 5, 0, 8, 6, 7];
        let classes = characters.len();
        let mut rows = vec![0f32; path.len() * classes];
        for (step, &class) in path.iter().enumerate() {
            rows[step * classes + class] = 0.9;
        }
        let decoded = decode(&rows, path.len(), classes, &characters);
        let text: String = decoded.characters.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(text, "BP_Ox 中文");
        assert!((decoded.score - 0.9).abs() < 1e-6);
        let line = line(Region { left: 100, top: 10, right: 210, bottom: 30 }, &decoded);
        let words: Vec<&str> = line.words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(words, ["BP_Ox", "中", "文"]);
        assert_eq!(line.text(), "BP_Ox 中文", "the recognized space stays");
        let [left, top, right, bottom] = line.words[0].rect;
        assert!(left >= 100.0 && right < line.words[1].rect[0] + 1.0 && top == 10.0 && bottom == 30.0);
    }

    /// Recognizes with the real pack: `SNAPSHOT_OCR_PACK` (or the installed
    /// pack) and, if set, every PNG in `SNAPSHOT_OCR_IMAGES`.
    /// Run with `<test binary> paddle_ocr::tests::pack_live --ignored --nocapture`.
    #[test]
    #[ignore]
    fn pack_live() {
        let folder = std::env::var_os("SNAPSHOT_OCR_PACK")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| crate::ocr_pack::folder().unwrap());
        let mut samples = vec![("rendered".to_string(), unsafe {
            crate::ocr::tests::sample("识别文字 Snapshot 保存到 C:\\Users\\eric\\Pictures", 16, true)
        })];
        if let Some(images) = std::env::var_os("SNAPSHOT_OCR_IMAGES") {
            for entry in std::fs::read_dir(images).unwrap().flatten() {
                if entry.path().extension().is_some_and(|extension| extension == "png") {
                    let image = crate::app_icon::decode(&std::fs::read(entry.path()).unwrap()).unwrap();
                    samples.push((entry.file_name().to_string_lossy().into_owned(), image));
                }
            }
        }
        for (name, image) in samples {
            let start = std::time::Instant::now();
            let lines = recognize(&image, &folder).unwrap();
            println!("== {name}: {} lines in {:?}", lines.len(), start.elapsed());
            for line in &lines {
                println!("  {}", line.text());
                if std::env::var_os("SNAPSHOT_OCR_WORDS").is_some() {
                    for word in &line.words {
                        println!("    {:?} {:?} spaced={}", word.text, word.rect, word.spaced);
                    }
                }
            }
            if name == "rendered" {
                let text = ocr::text(&lines);
                for expected in ["识别文字", "Snapshot", "保存到", "Pictures"] {
                    assert!(text.contains(expected), "{text}");
                }
            }
        }
    }

    #[test]
    fn boxes_side_by_side_join_into_one_line_but_columns_stay_apart() {
        let piece = |left: i32, top: i32, width: i32, text: &str| {
            let region = Region { left, top, right: left + width, bottom: top + 20 };
            let word = Word { text: text.into(), rect: [left as f32, top as f32, (left + width) as f32, (top + 20) as f32], spaced: false };
            (region, Line { words: vec![word] })
        };
        let lines = merge_rows(vec![
            piece(0, 0, 30, "PY:"),
            piece(40, 1, 80, "Warning:"),
            piece(125, 0, 20, "QQ"),
            piece(400, 0, 60, "Actor"),
            piece(0, 30, 60, "next"),
        ]);
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        assert_eq!(texts, ["PY: Warning: QQ", "Actor", "next"]);
    }

    #[test]
    fn boxes_touching_on_a_row_are_read_as_one() {
        let at = |left, top, right, bottom| Region { left, top, right, bottom };
        let joined = join_touching(vec![
            at(46, 251, 155, 275),  // self.user.
            at(150, 253, 195, 273), // _name_
            at(193, 253, 243, 275), // list
            at(258, 252, 300, 275), // apart by a space and more: its own box
            at(150, 230, 200, 252), // the row above
            at(300, 240, 330, 300), // a tall box beside: another size of text
        ]);
        assert!(joined.contains(&at(46, 251, 243, 275)), "{joined:?}");
        assert_eq!(joined.len(), 4, "{joined:?}");
    }

    #[test]
    fn identifiers_cut_into_boxes_join_back_without_spaces() {
        let piece = |left: f32, right: f32, text: &str| {
            let region = Region { left: left as i32, top: 0, right: right as i32, bottom: 18 };
            (region, Line { words: vec![Word { text: text.into(), rect: [left, 0.0, right, 18.0], spaced: false }] })
        };
        // Read from a pin: the underscore at the cut came back in both boxes.
        let lines = merge_rows(vec![
            piece(15.0, 32.5, "if"),
            piece(39.5, 108.5, "user_"),
            piece(103.0, 141.7, "_name"),
            piece(147.2, 163.5, "is"),
            piece(170.3, 197.0, "not"),
            piece(197.5, 230.0, "None:"),
        ]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text(), "if user_name is not None:");
        assert!(lines[0].attached(2), "double-click crosses the cut");
        // Boxes that only touch at the cut.
        let lines = merge_rows(vec![piece(38.3, 86.0, "u_cfg_"), piece(86.0, 132.5, "_out_"), piece(128.6, 172.9, "proxy")]);
        assert_eq!(lines[0].text(), "u_cfg_out_path");
    }

    #[test]
    fn reading_order_goes_left_to_right_within_a_row() {
        let at = |left, top| Region { left, top, right: left + 10, bottom: top + 10 };
        let mut boxes = vec![at(50, 3), at(0, 40), at(0, 6)];
        sort_reading_order(&mut boxes);
        assert_eq!(boxes, vec![at(0, 6), at(50, 3), at(0, 40)]);
    }
}
