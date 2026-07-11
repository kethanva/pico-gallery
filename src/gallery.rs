//! Browsable thumbnail grid — PhotoPrism kiosk style.
//!
//! Shows all loaded photos in a scrollable grid. Clicking a thumbnail opens the
//! fullscreen slideshow; Escape or the close control returns here.

use crate::compose::{blit_clipped, cover_square, fill_rect};
use image::{Rgba, RgbaImage};
use std::collections::HashMap;

pub const GAP: u32 = 8;
pub const MIN_CELL: u32 = 140;
pub const HEADER_H: u32 = 36;

/// Scrollable photo grid with a small in-memory thumb cache.
pub struct GalleryGrid {
    pub scroll_y: i32,
    pub cols: u32,
    pub cell: u32,
    /// Keyboard / click selection — opened with Enter or double-tap click.
    pub selected: usize,
    thumbs: HashMap<usize, RgbaImage>,
}

impl GalleryGrid {
    pub fn new(screen_w: u32) -> Self {
        let cols = ((screen_w.saturating_sub(GAP)) / (MIN_CELL + GAP)).max(1);
        let cell = (screen_w.saturating_sub(GAP * (cols + 1))) / cols;
        Self {
            scroll_y: 0,
            cols,
            cell,
            selected: 0,
            thumbs: HashMap::new(),
        }
    }

    pub fn rows_for_count(&self, count: usize) -> u32 {
        if count == 0 || self.cols == 0 {
            return 0;
        }
        (count as u32).div_ceil(self.cols)
    }

    pub fn content_height(&self, count: usize) -> u32 {
        HEADER_H + self.rows_for_count(count) * (self.cell + GAP) + GAP
    }

    pub fn cell_origin(&self, index: usize) -> (u32, i32) {
        let col = (index as u32) % self.cols;
        let row = (index as u32) / self.cols;
        let x = GAP + col * (self.cell + GAP);
        let y = HEADER_H as i32 + row as i32 * (self.cell + GAP) as i32 - self.scroll_y;
        (x, y)
    }

    /// Hit-test a click. Returns `None` for header, gaps, or out-of-range cells.
    pub fn index_at(&self, x: i32, y: i32, count: usize) -> Option<usize> {
        if count == 0 || x < 0 || y < HEADER_H as i32 {
            return None;
        }
        let y_adj = y + self.scroll_y - HEADER_H as i32;
        if y_adj < 0 {
            return None;
        }
        let pitch = (self.cell + GAP) as i32;
        let row = y_adj / pitch;
        let col_x = x - GAP as i32;
        if col_x < 0 {
            return None;
        }
        let col = col_x / pitch;
        // Reject clicks that land in the gap between cells.
        let in_cell_x = col_x % pitch;
        let in_cell_y = y_adj % pitch;
        if in_cell_x >= self.cell as i32 || in_cell_y >= self.cell as i32 {
            return None;
        }
        if col < 0 || col as u32 >= self.cols {
            return None;
        }
        let idx = row as usize * self.cols as usize + col as usize;
        if idx < count {
            Some(idx)
        } else {
            None
        }
    }

    pub fn visible_indices(&self, screen_h: u32, count: usize) -> Vec<usize> {
        let mut out = Vec::new();
        if count == 0 || self.cols == 0 {
            return out;
        }
        // Visibility (`y_end > HEADER_H && y < screen_h`) depends only on the
        // row, so visible cells form one contiguous band of rows. Compute the
        // first visible row directly instead of scanning all `count` cells,
        // then walk rows until one falls below the viewport. O(visible), not
        // O(count) — this runs every gallery tick.
        let pitch = (self.cell + GAP) as i32;
        let rows = self.rows_for_count(count);
        // Smallest row whose bottom edge clears the header:
        //   y(r) + cell > HEADER_H  ⟺  r*pitch > scroll_y - cell
        let numer = self.scroll_y - self.cell as i32;
        let first_row = if numer < 0 { 0 } else { numer / pitch + 1 };
        let first_row = (first_row as u32).min(rows);
        let cols = self.cols as usize;
        for row in first_row..rows {
            let y = HEADER_H as i32 + row as i32 * pitch - self.scroll_y;
            if y >= screen_h as i32 {
                break; // this row and every later one is below the viewport
            }
            let base = row as usize * cols;
            for col in 0..cols {
                let idx = base + col;
                if idx >= count {
                    break;
                }
                out.push(idx);
            }
        }
        out
    }

    pub fn insert_thumb(&mut self, index: usize, img: RgbaImage) {
        // Prefer evicting thumbs that are far from the newly inserted index.
        if self.thumbs.len() >= 96 {
            let mut keys: Vec<_> = self.thumbs.keys().copied().collect();
            keys.sort_by_key(|k| k.abs_diff(index));
            for k in keys.into_iter().rev().take(48) {
                self.thumbs.remove(&k);
            }
        }
        // Cover-crop to the cell once at insert time so render never resizes.
        let square = cover_square(img, self.cell);
        self.thumbs.insert(index, square);
    }

    pub fn thumb(&self, index: usize) -> Option<&RgbaImage> {
        self.thumbs.get(&index)
    }

    /// Drop all cached thumbs (e.g. after the play queue is rebuilt).
    pub fn clear(&mut self) {
        self.thumbs.clear();
        self.scroll_y = 0;
        self.selected = 0;
    }

    pub fn set_selected(&mut self, index: usize, count: usize) {
        if count > 0 {
            self.selected = index.min(count - 1);
        }
    }

    /// Move the selection by grid cells (dx = column, dy = row).
    ///
    /// Returns true only when the selection was already on the very last
    /// loaded photo and a forward move (right/down) was requested — the
    /// signal callers use to try fetching more photos. This is deliberately
    /// narrower than "the move didn't change the selection": pressing Right
    /// at the last column of *any* row also leaves `selected` unchanged (the
    /// column clamps), but that is a normal grid-edge no-op, not "we've run
    /// out of data" — conflating the two would fire a queue-extension fetch
    /// on every such press throughout the whole gallery, not just at the end.
    pub fn move_selection(&mut self, dx: i32, dy: i32, count: usize) -> bool {
        if count == 0 || self.cols == 0 || (dx == 0 && dy == 0) {
            return false;
        }
        let at_last_item = self.selected >= count - 1;
        let cols = self.cols as i32;
        let row = (self.selected as i32 / cols) + dy;
        let col = (self.selected as i32 % cols) + dx;
        let row = row.max(0);
        let col = col.clamp(0, cols - 1);
        let idx = row as usize * self.cols as usize + col as usize;
        self.selected = idx.min(count - 1);
        at_last_item && (dx > 0 || dy > 0)
    }

    /// True when the grid is scrolled to the bottom of `count` photos.
    pub fn at_scroll_bottom(&self, count: usize, screen_h: u32) -> bool {
        if count == 0 {
            return true;
        }
        let max_scroll = self.content_height(count).saturating_sub(screen_h) as i32;
        self.scroll_y >= max_scroll.max(0)
    }

    /// Scroll the minimum amount to bring the selected cell into the viewport.
    pub fn ensure_selected_visible(&mut self, screen_h: u32, count: usize) {
        if count == 0 {
            return;
        }
        let (_, y) = self.cell_origin(self.selected);
        let bottom = y + self.cell as i32;
        let top = HEADER_H as i32;
        if y < top {
            self.scroll_y += y - top;
        } else if bottom > screen_h as i32 {
            self.scroll_y += bottom - screen_h as i32;
        }
        self.clamp_scroll(count, screen_h);
    }

    /// Full rows visible below the header — one "page" of the grid.
    pub fn rows_per_page(&self, screen_h: u32) -> i32 {
        let body = screen_h.saturating_sub(HEADER_H);
        let pitch = self.cell + GAP;
        ((body / pitch).max(1)) as i32
    }

    /// Scroll by one page (`direction` 1 = next/down, -1 = previous/up).
    pub fn scroll_page(&mut self, direction: i32, count: usize, screen_h: u32) -> bool {
        if count == 0 || direction == 0 {
            return false;
        }
        let before = self.scroll_y;
        let pitch = (self.cell + GAP) as i32;
        self.scroll_y += direction.signum() * self.rows_per_page(screen_h) * pitch;
        self.clamp_scroll(count, screen_h);
        if self.scroll_y == before {
            return false;
        }
        let visible = self.visible_indices(screen_h, count);
        if !visible.contains(&self.selected) {
            if direction > 0 {
                if let Some(&last) = visible.last() {
                    self.selected = last;
                }
            } else if let Some(&first) = visible.first() {
                self.selected = first;
            }
        }
        true
    }

    pub fn scroll_by(&mut self, delta_y: i32, count: usize, screen_h: u32) {
        // Positive delta_y = finger/wheel up = content moves down = scroll_y decreases.
        // Renderer sends wheel * 40 where SDL y>0 is scroll up.
        self.scroll_y -= delta_y;
        self.clamp_scroll(count, screen_h);
    }

    pub fn clamp_scroll(&mut self, count: usize, screen_h: u32) {
        let max_scroll = self.content_height(count).saturating_sub(screen_h) as i32;
        self.scroll_y = self.scroll_y.clamp(0, max_scroll.max(0));
    }

    pub fn render(&self, screen_w: u32, screen_h: u32, count: usize) -> RgbaImage {
        let mut frame = RgbaImage::from_pixel(screen_w, screen_h, Rgba([10, 10, 10, 255]));

        draw_text_line(
            &mut frame,
            &format!("{count} photos — click to open, Esc closes preview"),
            GAP as i32,
            10,
        );

        for i in self.visible_indices(screen_h, count) {
            let (x, y) = self.cell_origin(i);
            // Clip cells that straddle the top/bottom of the viewport.
            let src_y0 = if y < HEADER_H as i32 {
                (HEADER_H as i32 - y) as u32
            } else {
                0
            };
            let dst_y = y.max(HEADER_H as i32) as u32;
            if dst_y >= screen_h {
                continue;
            }
            let draw_h = self.cell.saturating_sub(src_y0).min(screen_h - dst_y);
            if draw_h == 0 {
                continue;
            }
            let bg = if i == self.selected {
                Rgba([36, 48, 72, 255])
            } else {
                Rgba([28, 28, 28, 255])
            };
            fill_rect(&mut frame, x, dst_y, self.cell, draw_h, bg);
            if let Some(thumb) = self.thumb(i) {
                blit_clipped(&mut frame, thumb, x, dst_y, self.cell, draw_h, src_y0);
            }
            if i == self.selected {
                draw_selection_ring(&mut frame, x, dst_y, self.cell, draw_h);
            }
        }
        frame
    }
}

const SEL_BORDER: u32 = 3;
const SEL_COLOR: Rgba<u8> = Rgba([100, 180, 255, 255]);

/// Highlight ring around the selected thumbnail. Four inset bars via the
/// row-copy `fill_rect` instead of per-pixel `put_pixel`.
fn draw_selection_ring(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32) {
    if w == 0 || h == 0 {
        return;
    }
    let b = SEL_BORDER.min(w).min(h);
    fill_rect(img, x, y, w, b, SEL_COLOR); // top
    fill_rect(img, x, y + h - b, w, b, SEL_COLOR); // bottom
    fill_rect(img, x, y, b, h, SEL_COLOR); // left
    fill_rect(img, x + w - b, y, b, h, SEL_COLOR); // right
}

fn draw_text_line(img: &mut RgbaImage, text: &str, x: i32, y: i32) {
    use font8x8::UnicodeFonts;
    const SCALE: u32 = 2;
    let mut cx = x;
    for ch in text.chars().take(72) {
        if let Some(glyph) = font8x8::BASIC_FONTS.get(ch) {
            for (row_i, byte) in glyph.iter().enumerate() {
                for col in 0..8u32 {
                    if byte & (1 << col) == 0 {
                        continue;
                    }
                    for dy in 0..SCALE {
                        for dx in 0..SCALE {
                            let px = cx + (col * SCALE + dx) as i32;
                            let py = y + (row_i as u32 * SCALE + dy) as i32;
                            if px >= 0
                                && py >= 0
                                && (px as u32) < img.width()
                                && (py as u32) < img.height()
                            {
                                img.put_pixel(px as u32, py as u32, Rgba([220, 220, 220, 255]));
                            }
                        }
                    }
                }
            }
        }
        cx += 8 * SCALE as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_at_hits_first_cell() {
        let g = GalleryGrid::new(800);
        assert!(g.cols >= 1);
        let (x, y) = g.cell_origin(0);
        assert_eq!(g.index_at(x as i32 + 4, y + 4, 10), Some(0));
    }

    #[test]
    fn index_at_rejects_header_and_gaps() {
        let g = GalleryGrid::new(800);
        assert_eq!(g.index_at(10, 5, 10), None); // header
        let (x, _y) = g.cell_origin(0);
        // Just past the right edge of cell 0 into the gap.
        let gap_x = x as i32 + g.cell as i32 + 1;
        assert_eq!(g.index_at(gap_x, HEADER_H as i32 + 4, 10), None);
    }

    #[test]
    fn scroll_clamps_to_content() {
        let mut g = GalleryGrid::new(400);
        g.scroll_by(-10_000, 3, 300);
        assert!(g.scroll_y >= 0);
        g.scroll_by(10_000, 3, 300);
        assert_eq!(g.scroll_y, 0);
    }

    #[test]
    fn scroll_page_moves_by_full_rows() {
        let mut g = GalleryGrid::new(400);
        let count = 40;
        let h = 300;
        let before = g.scroll_y;
        assert!(g.scroll_page(1, count, h));
        assert!(g.scroll_y > before);
        let page = g.rows_per_page(h) * (g.cell + GAP) as i32;
        assert_eq!(g.scroll_y - before, page);
    }

    #[test]
    fn move_selection_wraps_columns() {
        let mut g = GalleryGrid::new(800);
        g.set_selected(0, 20);
        assert!(!g.move_selection(1, 0, 20));
        assert_eq!(g.selected, 1);
        assert!(!g.move_selection(0, 1, 20));
        assert_eq!(g.selected, 1 + g.cols as usize);
    }

    #[test]
    fn move_selection_blocked_at_last_cell() {
        let mut g = GalleryGrid::new(800);
        g.set_selected(9, 10);
        assert!(g.move_selection(1, 0, 10));
        assert_eq!(g.selected, 9);
    }

    #[test]
    fn move_selection_not_blocked_at_row_edge_mid_grid() {
        // Pressing Right at the last column of an early row (nowhere near the
        // last loaded photo) must not report "blocked" — that would trigger a
        // wasted queue-extension fetch on every such press throughout the
        // gallery, not just at the true end of the data.
        let mut g = GalleryGrid::new(800);
        let cols = g.cols as usize;
        let count = cols * 20; // many rows below the current one
        g.set_selected(cols - 1, count); // last column of row 0
        assert!(!g.move_selection(1, 0, count));
        assert_eq!(g.selected, cols - 1); // clamped in place, as before
    }

    #[test]
    fn move_selection_not_blocked_moving_into_short_last_row() {
        // Moving down into a short final row lands on a real existing item —
        // that is a successful move, not a "ran out of data" signal.
        let mut g = GalleryGrid::new(800);
        let cols = g.cols as usize;
        if cols < 2 {
            return; // degenerate screen width — nothing to test
        }
        let count = cols + 1; // row 0 full, row 1 has exactly one item
        g.set_selected(1, count); // row 0, col 1
        assert!(!g.move_selection(0, 1, count));
        assert_eq!(g.selected, cols); // snapped to the only item in row 1
    }

    #[test]
    fn move_selection_blocked_moving_down_from_last_item() {
        // The Down arrow key (one row at a time, not a full page) must also
        // report "blocked" at the true last photo, same as Right — this is
        // the signal that triggers fetching more photos.
        let mut g = GalleryGrid::new(800);
        g.set_selected(9, 10);
        assert!(g.move_selection(0, 1, 10));
        assert_eq!(g.selected, 9); // stays put; caller retries after loading more
    }

    #[test]
    fn move_selection_up_from_first_row_does_not_trigger_extend() {
        // Moving Up when already at the top must never report "blocked" —
        // there's nothing to load by going backwards.
        let mut g = GalleryGrid::new(800);
        g.set_selected(0, 100);
        assert!(!g.move_selection(0, -1, 100));
        assert_eq!(g.selected, 0);
    }

    #[test]
    fn ensure_selected_visible_scrolls_by_one_row_when_moving_down() {
        // Moving the selection down one row at a time should bring it into
        // view with the minimum scroll needed — not jump by a full page.
        let mut g = GalleryGrid::new(400);
        let count = g.cols as usize * 20;
        let h = 300;
        let rows_visible = g.rows_per_page(h) as usize;
        // Land the selection just below the current viewport.
        g.set_selected((rows_visible + 1) * g.cols as usize, count);
        g.ensure_selected_visible(h, count);
        let (_, y) = g.cell_origin(g.selected);
        assert!(y >= HEADER_H as i32);
        assert!(y + g.cell as i32 <= h as i32);
    }

    #[test]
    fn at_scroll_bottom_when_fully_scrolled() {
        let mut g = GalleryGrid::new(400);
        let count = 40;
        let h = 300;
        while g.scroll_page(1, count, h) {}
        assert!(g.at_scroll_bottom(count, h));
    }

    /// Reference implementation: the original O(count) predicate.
    fn naive_visible(g: &GalleryGrid, screen_h: u32, count: usize) -> Vec<usize> {
        let mut out = Vec::new();
        for i in 0..count {
            let (_, y) = g.cell_origin(i);
            let y_end = y + g.cell as i32;
            if y_end > HEADER_H as i32 && y < screen_h as i32 {
                out.push(i);
            }
        }
        out
    }

    #[test]
    fn visible_indices_matches_bruteforce_across_scrolls() {
        let mut g = GalleryGrid::new(400);
        let count = g.cols as usize * 30;
        let h = 300;
        let max = g.content_height(count) as i32;
        for sy in (0..=max).step_by(7) {
            g.scroll_y = sy;
            assert_eq!(
                g.visible_indices(h, count),
                naive_visible(&g, h, count),
                "mismatch at scroll_y={sy}"
            );
        }
        // Degenerate counts must also match.
        for c in [0usize, 1, g.cols as usize, g.cols as usize + 1] {
            g.scroll_y = 0;
            assert_eq!(g.visible_indices(h, c), naive_visible(&g, h, c));
        }
    }
}
