use std::ops::Range;

use crate::capture::Image;

pub const MIN_HEIGHT: i32 = 64;
const MAX_HEIGHT: usize = 32_768;
const MAX_BYTES: usize = 128 * 1024 * 1024;
/// Rows with visible content (not a flat color) the overlap must contain.
const MIN_ANCHORS: usize = 6;
const MIN_RUN: usize = 8;
/// Horizontal cells per row fingerprint, each the mean gray of its pixels.
const CELLS: usize = 96;
/// Half the gray step between neighboring cells that counts as an edge, and
/// as a disagreement between two rows. Content scrolls over fixed gradients
/// and through translucent panels, so the same text comes back a few levels
/// lighter or darker, never equal; the steps between cells barely change.
const TOLERANCE: u32 = 6;
/// Coarse offsets checked in full after the quick scan.
const CANDIDATES: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Update {
    Ready,
    NoMatch,
    Limit,
}

pub struct Stitcher {
    pub image: Image,
    rows: Fingerprints,
    next_rows: Fingerprints,
    rejected_rows: Vec<u64>,
    overlay: Overlay,
    viewport_top: i32,
}

impl Stitcher {
    pub fn new(image: Image) -> Option<Self> {
        if image.height as usize > MAX_HEIGHT || image.pixels.len() > MAX_BYTES {
            return None;
        }
        let mut rows = Fingerprints::default();
        rows.update(&image.pixels, image.width as usize);
        Some(Self {
            next_rows: Fingerprints::default(),
            rejected_rows: Vec::new(),
            overlay: Overlay::new(&image),
            rows,
            image,
            viewport_top: 0,
        })
    }

    pub fn observe(&mut self, frame: &[u8]) -> Update {
        let width = self.image.width as usize;
        self.next_rows.update(frame, width);
        if self.next_rows.exact == self.rows.exact {
            return Update::Ready;
        }
        if self.next_rows.exact == self.rejected_rows {
            return Update::NoMatch;
        }
        let Some((shift, seam, pinned)) = align(&self.rows, &self.next_rows) else {
            let noise_limit = (self.rows.len() / 100).max(2);
            if (0..self.rows.len())
                .filter(|&y| !self.rows.same(y, &self.next_rows, y))
                .take(noise_limit + 1)
                .count()
                <= noise_limit
            {
                // A blinking caret or a few animated rows don't move the viewport.
                return Update::Ready;
            }
            std::mem::swap(&mut self.rejected_rows, &mut self.next_rows.exact);
            return Update::NoMatch;
        };
        let top = self.viewport_top + shift;
        let bottom = top + self.rows.len() as i32;
        let height = (self.image.height.max(bottom) - top.min(0)) as usize;
        let row_bytes = width * 4;
        let Some(bytes) = height.checked_mul(row_bytes) else {
            return Update::Limit;
        };
        if height > MAX_HEIGHT || bytes > MAX_BYTES {
            return Update::Limit;
        }
        let pixels = &mut self.image.pixels;
        if bytes > pixels.capacity() {
            let capacity = (pixels.capacity() * 2).clamp(bytes, MAX_BYTES);
            if pixels.try_reserve_exact(capacity - pixels.len()).is_err() {
                return Update::Limit;
            }
        }

        self.overlay.learn(frame, shift, pinned);

        // Replace the edge through the seam, keeping fixed chrome only at the extremes.
        let frame_seam = seam * row_bytes;
        let kept = (top + seam as i32) as usize;
        let image_seam = kept * row_bytes;
        if top < 0 {
            let old_bytes = pixels.len();
            pixels.resize(bytes, 0);
            pixels.copy_within(image_seam..old_bytes, frame_seam);
            pixels[..frame_seam].copy_from_slice(&frame[..frame_seam]);
            self.overlay.prepend(kept, seam);
        } else if bottom > self.image.height {
            pixels.truncate(image_seam);
            pixels.extend_from_slice(&frame[frame_seam..]);
            self.overlay.append(kept, seam, self.rows.len());
        }
        self.viewport_top = top.max(0);
        self.overlay
            .repair(pixels, frame, self.viewport_top as usize);
        self.image.height = height as i32;
        self.rejected_rows.clear();
        std::mem::swap(&mut self.rows, &mut self.next_rows);
        Update::Ready
    }
}

/// Side length of the squares in which fixed overlays are detected.
const BLOCK: usize = 8;
/// A pixel votes "overlay" when it kept its screen color within `STILL`
/// although the content that scrolled to it differs by `STANDS_OUT` or more:
/// a gradient fixed behind translucent content never differs that much.
const STILL: i32 = 4;
const STANDS_OUT: i32 = 32;
/// How much closer a pixel must be to the scrolled content than to its old
/// screen color to vote "content".
const EVIDENCE: i32 = 8;
/// Blocks added around a detected overlay.
const GROW: usize = 2;
/// Pixels beside a patch that set its shading.
const RING: usize = 8;

/// Floating buttons and toolbars that stay put on screen while the content
/// scrolls beneath them. Without this every stitched screen would bring its
/// own copy. Each image row remembers which screen row it came from; image
/// pixels that were under an overlay there are replaced from a later frame
/// that shows the same content away from it, so only the last screen keeps
/// the overlay, as it keeps fixed headers and footers.
struct Overlay {
    width: usize,
    height: usize,
    columns: usize,
    /// Per block: pixels that stayed on screen, pixels that moved along.
    votes: Vec<(u16, u16)>,
    mask: Vec<bool>,
    /// Screen rows of headers, footers and full-width bars: never content.
    pinned: Vec<bool>,
    /// Per image row, the screen row it was copied from.
    sources: Vec<u16>,
    /// Per image pixel, set once it holds content that no overlay covered.
    repaired: Vec<u64>,
    words: usize,
    last: Vec<u8>,
}

impl Overlay {
    fn new(first: &Image) -> Self {
        let width = first.width as usize;
        let height = first.height as usize;
        let columns = width.div_ceil(BLOCK);
        let blocks = columns * height.div_ceil(BLOCK);
        let words = width.div_ceil(64);
        Self {
            width,
            height,
            columns,
            votes: vec![(0, 0); blocks],
            mask: vec![false; blocks],
            pinned: vec![false; height],
            sources: (0..height as u16).collect(),
            repaired: vec![0; height * words],
            words,
            last: first.pixels.clone(),
        }
    }

    fn masked(&self, x: usize, y: usize) -> bool {
        self.mask[y / BLOCK * self.columns + x / BLOCK]
    }

    /// Votes on every screen pixel by comparing the previous accepted frame
    /// with `frame`, whose content moved by `shift`, then rebuilds the mask.
    /// `pinned` counts the rows that stayed put at the top and the bottom.
    fn learn(&mut self, frame: &[u8], shift: i32, pinned: (usize, usize)) {
        let row_bytes = self.width * 4;
        for y in 0..self.height {
            let Ok(moved_from) = usize::try_from(y as i32 + shift) else {
                continue;
            };
            if moved_from >= self.height {
                continue;
            }
            let now = &frame[y * row_bytes..][..row_bytes];
            let before = &self.last[y * row_bytes..][..row_bytes];
            let carried = &self.last[moved_from * row_bytes..][..row_bytes];
            let votes = &mut self.votes[y / BLOCK * self.columns..][..self.columns];
            for x in 0..self.width {
                let pixel = &now[x * 4..x * 4 + 3];
                let stayed = difference(pixel, &before[x * 4..x * 4 + 3]);
                let moved = difference(pixel, &carried[x * 4..x * 4 + 3]);
                let vote = &mut votes[x / BLOCK];
                if stayed <= STILL && moved >= STANDS_OUT {
                    vote.0 = vote.0.saturating_add(1);
                } else if moved + EVIDENCE < stayed {
                    vote.1 = vote.1.saturating_add(1);
                }
            }
        }
        let fixed: Vec<bool> = self
            .votes
            .iter()
            // A button may fill only part of a block whose rest scrolls.
            .map(|&(stayed, moved)| stayed >= 6 && stayed * 4 >= moved)
            .collect();
        // Rows fixed almost across the whole width are headers and footers,
        // which the seams already keep only at the extremes.
        let mut core = fixed;
        for (y, is_pinned) in self.pinned.iter_mut().enumerate() {
            *is_pinned = y < pinned.0 || y >= self.height - pinned.1;
        }
        for (by, row) in core.chunks_exact_mut(self.columns).enumerate() {
            if row.iter().filter(|&&f| f).count() * 10 >= self.columns * 9 {
                row.fill(false);
                let end = ((by + 1) * BLOCK).min(self.height);
                self.pinned[by * BLOCK..end].fill(true);
            }
        }
        // Grow by two blocks to take in thin rims, anti-aliased edges and
        // drop shadows, which barely stand out from the content behind.
        let rows = core.len() / self.columns;
        for by in 0..rows {
            for bx in 0..self.columns {
                self.mask[by * self.columns + bx] = (by.saturating_sub(GROW)..(by + GROW + 1).min(rows))
                    .any(|ny| {
                        (bx.saturating_sub(GROW)..(bx + GROW + 1).min(self.columns))
                            .any(|nx| core[ny * self.columns + nx])
                    });
            }
        }
    }

    /// The image grew at the top: rows from `kept` on moved down to start at
    /// `seam`, above which the frame's first `seam` rows now sit.
    fn prepend(&mut self, kept: usize, seam: usize) {
        self.sources.drain(..kept);
        self.sources.splice(0..0, 0..seam as u16);
        self.repaired.drain(..kept * self.words);
        self.repaired.splice(0..0, std::iter::repeat_n(0, seam * self.words));
    }

    /// The image was cut at `kept` and continued with the frame from `seam`.
    fn append(&mut self, kept: usize, seam: usize, height: usize) {
        self.sources.truncate(kept);
        self.sources.extend(seam as u16..height as u16);
        self.repaired.truncate(kept * self.words);
        self.repaired
            .resize(self.repaired.len() + (height - seam) * self.words, 0);
    }

    /// `frame` now covers the image from row `top`: wherever the image still
    /// shows an overlay from its source row and the frame shows that spot
    /// uncovered by any overlay or pinned bar, take the frame's pixel. Then
    /// remember the frame.
    fn repair(&mut self, image: &mut [u8], frame: &[u8], top: usize) {
        let row_bytes = self.width * 4;
        for y in 0..self.height {
            let row = top + y;
            let source = self.sources[row] as usize;
            // Headers and footers in this frame cover content too.
            if source == y || self.pinned[y] {
                continue;
            }
            let block_row = &self.mask[source / BLOCK * self.columns..][..self.columns];
            let mut bx = 0;
            while bx < self.columns {
                if !block_row[bx] {
                    bx += 1;
                    continue;
                }
                let first = bx;
                while bx < self.columns && block_row[bx] {
                    bx += 1;
                }
                let span = first * BLOCK..(bx * BLOCK).min(self.width);
                let image_row = &mut image[row * row_bytes..][..row_bytes];
                let frame_row = &frame[y * row_bytes..][..row_bytes];
                let offset = self.shading(image_row, frame_row, y, span.clone());
                for x in span {
                    let bit = (row * self.words + x / 64, 1u64 << (x % 64));
                    if self.repaired[bit.0] & bit.1 != 0 || self.masked(x, y) {
                        continue;
                    }
                    for channel in 0..3 {
                        let value = i32::from(frame_row[x * 4 + channel]) + offset[channel];
                        image_row[x * 4 + channel] = value.clamp(0, 255) as u8;
                    }
                    self.repaired[bit.0] |= bit.1;
                }
            }
        }
        self.last.copy_from_slice(frame);
    }

    /// The content around a patch lies over the backdrop at another screen
    /// height in `frame` than in the image; the median difference just left
    /// and right of `span` brings the copied pixels to the image's shading.
    /// The median ignores a scrollbar thumb or an animation beside the patch.
    fn shading(&self, image_row: &[u8], frame_row: &[u8], y: usize, span: Range<usize>) -> [i32; 3] {
        let left = span.start.saturating_sub(RING)..span.start;
        let right = span.end..(span.end + RING).min(self.width);
        let mut differences: [Vec<i32>; 3] = Default::default();
        for x in left.chain(right).filter(|&x| !self.masked(x, y)) {
            for (channel, values) in differences.iter_mut().enumerate() {
                values.push(i32::from(image_row[x * 4 + channel]) - i32::from(frame_row[x * 4 + channel]));
            }
        }
        differences.map(|mut values| {
            if values.is_empty() {
                return 0;
            }
            let middle = values.len() / 2;
            *values.select_nth_unstable(middle).1
        })
    }
}

fn difference(a: &[u8], b: &[u8]) -> i32 {
    a.iter()
        .zip(b)
        .map(|(a, b)| i32::from(a.abs_diff(*b)))
        .max()
        .unwrap_or(0)
}

/// Per screen row: an exact hash for the cheap "nothing changed" checks, and
/// the gray steps between `CELLS` horizontal cells for tolerant comparison.
#[derive(Default)]
struct Fingerprints {
    exact: Vec<u64>,
    cells: Vec<i16>,
    /// How much each quarter of the row changes, for the quick offset scan.
    coarse: Vec<[u16; 4]>,
    /// Whether the row shows anything beyond one flat color.
    textured: Vec<bool>,
    width: usize,
}

impl Fingerprints {
    fn update(&mut self, frame: &[u8], width: usize) {
        self.exact.clear();
        self.cells.clear();
        self.coarse.clear();
        self.textured.clear();
        // Leave out the scrollbar strip and the unused fourth byte of the screen DIB.
        let right = width - (width / 20).min(24);
        let cells = CELLS.min(right);
        self.width = cells - 1;
        let mut sums = vec![0u32; cells];
        let mut counts = vec![0u32; cells];
        for x in 0..right {
            counts[x * cells / right] += 1;
        }
        for row in frame.chunks_exact(width * 4) {
            sums.fill(0);
            let mut hash = 0xcbf2_9ce4_8422_2325u64;
            for (x, pixel) in row[..right * 4].as_chunks::<4>().0.iter().enumerate() {
                let rgb = u32::from_le_bytes([pixel[0], pixel[1], pixel[2], 0]);
                hash = (hash ^ u64::from(rgb)).wrapping_mul(0x0100_0000_01b3);
                let gray = (u32::from(pixel[0]) + 5 * u32::from(pixel[1]) + 2 * u32::from(pixel[2])) / 8;
                sums[x * cells / right] += gray;
            }
            for (sum, count) in sums.iter_mut().zip(&counts) {
                *sum /= count;
            }
            let start = self.cells.len();
            self.cells
                .extend(sums.windows(2).map(|pair| pair[1] as i16 - pair[0] as i16));
            let steps = &self.cells[start..];
            let mut coarse = [0u16; 4];
            for (quarter, value) in coarse.iter_mut().enumerate() {
                let part = &steps[quarter * steps.len() / 4..(quarter + 1) * steps.len() / 4];
                *value = part.iter().map(|step| step.unsigned_abs()).sum();
            }
            self.exact.push(hash);
            self.coarse.push(coarse);
            self.textured
                .push(steps.iter().any(|step| step.unsigned_abs() > 2 * TOLERANCE as u16));
        }
    }

    fn len(&self) -> usize {
        self.exact.len()
    }

    fn row(&self, y: usize) -> &[i16] {
        &self.cells[y * self.width..(y + 1) * self.width]
    }

    /// Sum of cell differences between row `y` here and row `other_y` there.
    #[cfg(test)]
    fn distance(&self, y: usize, other: &Self, other_y: usize) -> u32 {
        if self.exact[y] == other.exact[other_y] {
            return 0;
        }
        self.row(y)
            .iter()
            .zip(other.row(other_y))
            .map(|(a, b)| u32::from(a.abs_diff(*b)))
            .sum()
    }

    /// Whether two rows show the same content: of the cells where either
    /// row has an edge, at most a quarter may disagree, so a floating button
    /// over part of the row does not hide that the rest lines up.
    fn same(&self, y: usize, other: &Self, other_y: usize) -> bool {
        if self.exact[y] == other.exact[other_y] {
            return true;
        }
        let edge = 2 * TOLERANCE as u16;
        let mut active = 0;
        let mut differing = 0;
        for (a, b) in self.row(y).iter().zip(other.row(other_y)) {
            if a.unsigned_abs() > edge || b.unsigned_abs() > edge {
                active += 1;
            }
            if a.abs_diff(*b) > edge {
                differing += 1;
            }
        }
        differing * 4 <= active
    }
}

/// A quarter of the region must stay in common: chat histories repeat whole
/// passages, and a thinner overlap lets a repeat pass for the real offset.
fn max_shift(height: usize) -> usize {
    height - (height / 4).max(32)
}

/// Finds how far the content moved from `previous` to `next` (positive when
/// it moved up), the row in `next` where to join and how many rows stayed
/// pinned at the top and the bottom, or `None` when no offset, or more than
/// one, explains the change.
fn align(previous: &Fingerprints, next: &Fingerprints) -> Option<(i32, usize, (usize, usize))> {
    let height = previous.len();
    let max_shift = max_shift(height);

    // The same screen rows at both edges can be pinned headers or footers.
    // Compare only the part between them, where both frames can scroll.
    let top = (0..height / 3)
        .take_while(|&y| previous.same(y, next, y))
        .count();
    let bottom = (0..height / 3)
        .take_while(|&i| previous.same(height - 1 - i, next, height - 1 - i))
        .count();
    let content_end = height - bottom;
    let overlap = |shift: i32| {
        let old_start = top + shift.max(0) as usize;
        let new_start = top + (-shift).max(0) as usize;
        let len = content_end.saturating_sub(old_start.max(new_start));
        (old_start, new_start, len)
    };

    // Quick scan of every offset on four values per row; keep the best few
    // local minima for the full check.
    let mut scores: Vec<(u32, i32)> = Vec::new();
    for shift in -(max_shift as i32)..=max_shift as i32 {
        let (old_start, new_start, len) = overlap(shift);
        if shift == 0 || len < MIN_RUN * 2 {
            continue;
        }
        let total: u32 = (0..len)
            .map(|i| {
                let (a, b) = (previous.coarse[old_start + i], next.coarse[new_start + i]);
                a.iter().zip(b).map(|(a, b)| u32::from(a.abs_diff(b))).sum::<u32>()
            })
            .sum();
        scores.push((total * 16 / len as u32, shift));
    }
    scores.sort_unstable();
    let mut candidates: Vec<i32> = Vec::with_capacity(CANDIDATES);
    for &(_, shift) in &scores {
        if candidates.iter().all(|other| other.abs_diff(shift) > 2) {
            candidates.push(shift);
            if candidates.len() == CANDIDATES {
                break;
            }
        }
    }

    // Full check: most of the overlap must match within the tolerance, with
    // enough real content in it, and the offset must be the only one that does.
    let mut found = None;
    for shift in candidates {
        let (old_start, new_start, len) = overlap(shift);
        let anchors = (0..len)
            .filter(|&i| {
                previous.textured[old_start + i] && previous.same(old_start + i, next, new_start + i)
            })
            .take(MIN_ANCHORS)
            .count();
        if anchors < MIN_ANCHORS {
            continue;
        }
        let matches: Vec<(bool, bool)> = (0..len)
            .map(|i| {
                let content = previous.textured[old_start + i] || next.textured[new_start + i];
                (previous.same(old_start + i, next, new_start + i), content)
            })
            .collect();
        if let Some(seam) = match_overlap(&matches) {
            if found.is_some() {
                return None;
            }
            found = Some((shift, new_start + seam, (top, bottom)));
        }
    }
    found
}

/// Where to join within the overlap: the middle of the longest run of
/// matching rows, provided three quarters of the rows with content match.
fn match_overlap(matches: &[(bool, bool)]) -> Option<usize> {
    let mut count = 0;
    let mut content = 0;
    let mut run = 0;
    let mut best_run = 0;
    let mut seam = 0;
    for (y, &(same, has_content)) in matches.iter().enumerate() {
        content += usize::from(has_content);
        if same {
            count += usize::from(has_content);
            run += 1;
            if run > best_run {
                best_run = run;
                seam = y + 1 - run / 2;
            }
        } else {
            run = 0;
        }
    }
    (best_run >= MIN_RUN && count * 4 >= content * 3).then_some(seam)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: i32 = 40;
    const HEIGHT: i32 = 128;
    const CHROME: usize = 8;
    const ROW_BYTES: usize = WIDTH as usize * 4;

    fn frame(offset: u8) -> Image {
        frame_with_chrome(offset, CHROME)
    }

    fn frame_with_chrome(offset: u8, chrome: usize) -> Image {
        frame_over(offset, chrome, |_| 0)
    }

    /// Rows are identified by the first pixel; the rest is a texture unique
    /// to the row, like text. `backdrop` lightens each screen row the same
    /// way in every frame, like a fixed gradient behind translucent content.
    fn frame_over(offset: u8, chrome: usize, backdrop: impl Fn(usize) -> u8) -> Image {
        let mut pixels = Vec::new();
        for y in 0..HEIGHT as usize {
            let color: u16 = if y < chrome {
                250
            } else if y >= HEIGHT as usize - chrome {
                251
            } else {
                u16::from(offset) + (y - chrome) as u16
            };
            let mut row = [color as u8; ROW_BYTES];
            for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                pixel[1] = (color >> 8) as u8;
                if x > 0 {
                    let texture = (u32::from(color) * 2_654_435_761 ^ x as u32 * 40_503)
                        .wrapping_mul(0x9e37_79b9)
                        >> 24;
                    let value = (texture as u8 / 2).saturating_add(backdrop(y));
                    pixel[..3].fill(value);
                }
            }
            row[ROW_BYTES - 4..].fill(offset);
            pixels.extend_from_slice(&row);
        }
        Image {
            width: WIDTH,
            height: HEIGHT,
            pixels,
        }
    }

    #[test]
    fn aligns_short_content_overlap_between_fixed_bars() {
        let chrome = 25;
        let mut stitcher = Stitcher::new(frame_with_chrome(45, chrome)).unwrap();
        assert!(stitcher.observe(&frame_with_chrome(0, chrome).pixels) == Update::Ready);
        assert!(stitcher.observe(&frame_with_chrome(60, chrome).pixels) == Update::Ready);
        let rows: Vec<_> = stitcher
            .image
            .pixels
            .chunks_exact(ROW_BYTES)
            .map(|row| row[0])
            .collect();
        assert_eq!(rows.len(), HEIGHT as usize + 60);
        assert_eq!(&rows[..chrome], &[250; 25]);
        assert_eq!(&rows[rows.len() - chrome..], &[251; 25]);
        assert!(rows[chrome..rows.len() - chrome].iter().copied().eq(0..138));
    }

    #[test]
    fn ignores_small_changes_at_the_same_scroll_position() {
        let mut stitcher = Stitcher::new(frame(0)).unwrap();
        let mut blinking = frame(0);
        blinking.pixels[42 * ROW_BYTES..43 * ROW_BYTES].fill(247);
        assert!(stitcher.observe(&blinking.pixels) == Update::Ready);
        assert_eq!(stitcher.image.height, HEIGHT);
        assert!(stitcher.observe(&frame(22).pixels) == Update::Ready);
        assert_image(&stitcher, 22);
    }

    #[test]
    fn adds_only_new_rows_and_keeps_fixed_chrome_once() {
        let mut stitcher = Stitcher::new(frame(0)).unwrap();
        assert!(stitcher.observe(&frame(24).pixels) == Update::Ready);
        assert!(stitcher.observe(&frame(52).pixels) == Update::Ready);
        assert_image(&stitcher, 52);
    }

    fn assert_image(stitcher: &Stitcher, extension: u8) {
        let image = &stitcher.image;
        assert_eq!(image.height, HEIGHT + i32::from(extension));
        let rows: Vec<_> = image
            .pixels
            .chunks_exact(ROW_BYTES)
            .map(|row| row[0])
            .collect();
        assert_eq!(&rows[..CHROME], &[250; CHROME]);
        assert_eq!(&rows[rows.len() - CHROME..], &[251; CHROME]);
        assert!(rows[CHROME..rows.len() - CHROME]
            .iter()
            .copied()
            .eq(0..(HEIGHT as u8 + extension - 2 * CHROME as u8)));
    }

    #[test]
    fn extends_both_ends_without_duplicating_revisited_content() {
        let mut stitcher = Stitcher::new(frame(36)).unwrap();
        assert!(stitcher.observe(&frame(60).pixels) == Update::Ready);
        assert!(stitcher.observe(&frame(12).pixels) == Update::Ready);
        assert!(stitcher.observe(&frame(190).pixels) == Update::NoMatch);
        assert!(stitcher.observe(&frame(0).pixels) == Update::Ready);
        assert_image(&stitcher, 60);
        assert!(stitcher.observe(&frame(36).pixels) == Update::Ready);
        assert!(stitcher.observe(&frame(12).pixels) == Update::Ready);
        assert_image(&stitcher, 60);
        assert!(stitcher.observe(&frame(47).pixels) == Update::Ready);
        assert!(stitcher.observe(&frame(79).pixels) == Update::Ready);
        assert_image(&stitcher, 79);
    }

    #[test]
    fn stationary_sidebar_does_not_break_scrolling_content_alignment() {
        let with_sidebar = |offset| {
            let mut image = frame(offset);
            for (y, row) in image.pixels.chunks_exact_mut(ROW_BYTES).enumerate() {
                for pixel in row[30 * 4..36 * 4].chunks_exact_mut(4) {
                    pixel.copy_from_slice(&[y as u8, 211, 220, 0]);
                }
            }
            image
        };
        let mut stitcher = Stitcher::new(with_sidebar(0)).unwrap();
        assert!(stitcher.observe(&with_sidebar(32).pixels) == Update::Ready);
        assert_image(&stitcher, 32);
    }

    fn load_png(path: &std::path::Path) -> Image {
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut rgba = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut rgba).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgba, "{}", path.display());
        let pixels = rgba[..info.buffer_size()]
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], 0])
            .collect();
        Image {
            width: info.width as i32,
            height: info.height as i32,
            pixels,
        }
    }

    /// Why a frame did not stitch: fixed rows at the edges and the offsets
    /// where most rows agree, with how many agree and how many have content.
    fn explain(previous: &Image, next: &Image) -> String {
        let width = previous.width as usize;
        let mut old = Fingerprints::default();
        let mut new = Fingerprints::default();
        old.update(&previous.pixels, width);
        new.update(&next.pixels, width);
        let height = old.len();
        let top = (0..height / 3).take_while(|&y| old.same(y, &new, y)).count();
        let bottom = (0..height / 3)
            .take_while(|&i| old.same(height - 1 - i, &new, height - 1 - i))
            .count();
        let max_shift = max_shift(height) as i32;
        let mut ranked: Vec<(usize, usize, usize, usize, u32, i32)> = Vec::new();
        for shift in -max_shift..=max_shift {
            if shift == 0 {
                continue;
            }
            let (o, n) = (shift.max(0) as usize, (-shift).max(0) as usize);
            let len = height - shift.unsigned_abs() as usize;
            let same = (0..len).filter(|&i| old.same(o + i, &new, n + i)).count();
            let anchors = (0..len)
                .filter(|&i| old.textured[o + i] && old.same(o + i, &new, n + i))
                .count();
            let distance: u32 = (0..len).map(|i| old.distance(o + i, &new, n + i)).sum();
            ranked.push((same * 1000 / len, same, len, anchors, distance / (len * old.width) as u32, shift));
        }
        ranked.sort_unstable_by(|a, b| b.cmp(a));
        let mut text = format!("fixed top={top} bottom={bottom}");
        for (_, same, len, anchors, mean, shift) in ranked.iter().take(3) {
            let _ = std::fmt::Write::write_fmt(
                &mut text,
                format_args!("\n      shift={shift} same rows={same}/{len} with content={anchors} mean cell diff={mean}"),
            );
        }
        text
    }

    /// Replays a folder written by `scroll_dump` through the stitcher:
    /// `SNAPSHOT_SCROLL_REPLAY=<folder> <test binary> replay_dump --ignored --nocapture`.
    #[test]
    #[ignore]
    fn replay_dump() {
        let folder = std::path::PathBuf::from(std::env::var("SNAPSHOT_SCROLL_REPLAY").unwrap());
        let first = load_png(&folder.join("first.png"));
        let mut frames: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with("frame-"))
            .collect();
        frames.sort();
        let mut accepted = Image { width: first.width, height: first.height, pixels: first.pixels.clone() };
        let mut stitcher = Stitcher::new(first).unwrap();
        let mut grown = 0;
        let mut slowest = std::time::Duration::ZERO;
        for path in &frames {
            let frame = load_png(path);
            let before = stitcher.image.height;
            let started = std::time::Instant::now();
            let update = stitcher.observe(&frame.pixels);
            slowest = slowest.max(started.elapsed());
            let name = path.file_name().unwrap().to_string_lossy();
            let after = stitcher.image.height;
            println!("{name} {update:?} height {before} -> {after}");
            if update == Update::NoMatch {
                println!("    {}", explain(&accepted, &frame));
            } else {
                if after != before {
                    grown += 1;
                }
                accepted = frame;
            }
        }
        println!(
            "{} frames, grew {grown} times, final height {}, slowest observe {slowest:?}",
            frames.len(),
            stitcher.image.height
        );
        stitcher.image.save_png(&folder.join("replay.png")).unwrap();
    }

    #[test]
    fn repeated_rows_alone_do_not_identify_a_scroll_offset() {
        let periodic = |phase: usize| {
            let mut image = frame(0);
            let first = image.pixels[CHROME * ROW_BYTES..(CHROME + 4) * ROW_BYTES].to_vec();
            for (y, row) in image.pixels.chunks_exact_mut(ROW_BYTES).enumerate() {
                let source = (y + phase) % 4;
                row.copy_from_slice(&first[source * ROW_BYTES..][..ROW_BYTES]);
            }
            image
        };
        let mut stitcher = Stitcher::new(periodic(0)).unwrap();
        assert!(stitcher.observe(&periodic(3).pixels) == Update::NoMatch);
        assert_eq!(stitcher.image.height, HEIGHT);
    }

    #[test]
    fn floating_button_appears_once_in_the_stitched_image() {
        const BUTTON: [u8; 4] = [255, 0, 255, 0];
        let with_button = |offset| {
            let mut image = frame(offset);
            for row in image.pixels.chunks_exact_mut(ROW_BYTES).skip(60).take(12) {
                for pixel in row[4 * 4..10 * 4].chunks_exact_mut(4) {
                    pixel.copy_from_slice(&BUTTON);
                }
            }
            image
        };
        let mut stitcher = Stitcher::new(with_button(0)).unwrap();
        // Each wheel notch scrolls further than the grown mask reaches.
        for offset in [40, 80, 120] {
            assert!(stitcher.observe(&with_button(offset).pixels) == Update::Ready);
        }
        assert_image(&stitcher, 120);
        let button_rows = stitcher
            .image
            .pixels
            .chunks_exact(ROW_BYTES)
            .filter(|row| row[6 * 4..7 * 4] == BUTTON)
            .count();
        assert_eq!(button_rows, 12, "one copy, from the last screen");
    }

    #[test]
    fn content_over_a_fixed_gradient_still_aligns() {
        // The chat window that never stitched: a vignette fixed to the
        // window shows through translucent message cards, so no scrolled
        // row comes back pixel-identical.
        let backdrop = |y: usize| (y * 6 / HEIGHT as usize) as u8;
        let mut stitcher = Stitcher::new(frame_over(0, 0, backdrop)).unwrap();
        assert!(stitcher.observe(&frame_over(30, 0, backdrop).pixels) == Update::Ready);
        assert_eq!(stitcher.image.height, HEIGHT + 30);
        let rows: Vec<_> = stitcher
            .image
            .pixels
            .chunks_exact(ROW_BYTES)
            .map(|row| row[0])
            .collect();
        assert!(rows.iter().copied().eq(0..(HEIGHT as u8 + 30)));
    }
}
