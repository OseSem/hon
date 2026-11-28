//! Layout of a long-strip chapter: where each page sits, which pages are near the viewport
//! (and so worth keeping decoded), and which page the reader is on. No GUI types, so it can
//! be tested on its own.

use std::ops::Range;

/// Height-to-width ratio assumed for a page until its real size is known.
pub const ASSUMED_RATIO: f32 = 1.42;
/// Space under the last page for the chapter's end controls.
pub const FOOTER_H: f32 = 160.0;

pub struct Strip {
    /// Each page's (width, height) in pixels, once known.
    sizes: Vec<Option<(u32, u32)>>,
    /// Display width of a page in logical pixels.
    width: f32,
}

impl Strip {
    pub fn new(pages: usize, width: f32) -> Strip {
        Strip {
            sizes: vec![None; pages],
            width,
        }
    }

    pub fn len(&self) -> usize {
        self.sizes.len()
    }

    pub fn width(&self) -> f32 {
        self.width
    }

    /// A page's size in pixels, once known.
    pub fn size(&self, page: usize) -> Option<(u32, u32)> {
        self.sizes.get(page).copied().flatten()
    }

    /// Whether a page is wider than tall, as a two-page spread printed as one image is.
    pub fn wide(&self, page: usize) -> bool {
        self.size(page).is_some_and(|(w, h)| w > h)
    }

    /// Pages grouped for showing two at a time: the first page and wide pages alone, the
    /// rest in pairs.
    pub fn spreads(&self) -> Vec<std::ops::Range<usize>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.len() {
            let pair = i > 0 && i + 1 < self.len() && !self.wide(i) && !self.wide(i + 1);
            let end = if pair { i + 2 } else { i + 1 };
            out.push(i..end);
            i = end;
        }
        out
    }

    pub fn height(&self, page: usize) -> f32 {
        match self.sizes[page] {
            Some((w, h)) if w > 0 => self.width * h as f32 / w as f32,
            _ => self.width * ASSUMED_RATIO,
        }
    }

    /// Top of `page`, or the end of the pages when `page == len()`.
    pub fn top(&self, page: usize) -> f32 {
        (0..page).map(|p| self.height(p)).sum()
    }

    /// Records a page's real size. Returns how far the content under it moved, which the
    /// caller scrolls by when the page is above `offset` so what's on screen stays put.
    pub fn set_size(&mut self, page: usize, size: (u32, u32), offset: f32) -> f32 {
        let before = self.height(page);
        let above = self.top(page) + before <= offset;
        self.sizes[page] = Some(size);
        if above {
            self.height(page) - before
        } else {
            0.0
        }
    }

    /// Changes the page width. Returns the offset that keeps the same spot of the same page
    /// at the top of the viewport.
    pub fn set_width(&mut self, width: f32, offset: f32) -> f32 {
        let page = self.page_at(offset);
        let into = if self.len() == 0 {
            0.0
        } else {
            (offset - self.top(page)) / self.height(page)
        };
        self.width = width.max(100.0);
        if self.len() == 0 {
            return 0.0;
        }
        self.top(page) + into * self.height(page)
    }

    /// The page under `y`, clamped to the last one.
    pub fn page_at(&self, y: f32) -> usize {
        let mut top = 0.0;
        for p in 0..self.len() {
            top += self.height(p);
            if y < top {
                return p;
            }
        }
        self.len().saturating_sub(1)
    }

    /// The page being read: the one a third of the way down the viewport.
    pub fn current(&self, offset: f32, viewport_h: f32) -> usize {
        self.page_at(offset + viewport_h / 3.0)
    }

    /// Pages within a screen above and two below the viewport.
    pub fn window(&self, offset: f32, viewport_h: f32) -> Range<usize> {
        if self.len() == 0 {
            return 0..0;
        }
        let first = self.page_at((offset - viewport_h).max(0.0));
        let last = self.page_at(offset + 3.0 * viewport_h);
        first..last + 1
    }

    /// Whether the bottom of the last page is on screen.
    pub fn at_end(&self, offset: f32, viewport_h: f32) -> bool {
        self.len() > 0 && offset + viewport_h >= self.top(self.len()) - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(sizes: &[(u32, u32)]) -> Strip {
        let mut s = Strip::new(sizes.len(), 800.0);
        for (i, &size) in sizes.iter().enumerate() {
            s.set_size(i, size, 0.0);
        }
        s
    }

    #[test]
    fn unknown_pages_take_the_assumed_ratio() {
        let s = Strip::new(3, 1000.0);
        assert_eq!(s.height(1), 1420.0);
        assert_eq!(s.top(2), 2840.0);
        assert_eq!(s.top(3), 3.0 * 1420.0);
    }

    #[test]
    fn heights_follow_the_page_size() {
        let s = strip(&[(1000, 1500), (500, 2000)]);
        assert_eq!(s.height(0), 1200.0);
        assert_eq!(s.height(1), 3200.0);
        assert_eq!(s.top(1), 1200.0);
    }

    #[test]
    fn page_at_and_current() {
        let s = strip(&[(800, 1000); 4]);
        assert_eq!(s.page_at(0.0), 0);
        assert_eq!(s.page_at(999.0), 0);
        assert_eq!(s.page_at(1000.0), 1);
        assert_eq!(s.page_at(1e9), 3);
        // A third of an 900 px viewport down from 800 lands on page 1.
        assert_eq!(s.current(800.0, 900.0), 1);
    }

    #[test]
    fn a_page_above_the_viewport_growing_asks_for_a_scroll() {
        let mut s = Strip::new(3, 800.0);
        // Page 0 ends at 1136; the reader is on page 1.
        let delta = s.set_size(0, (800, 2000), 1500.0);
        assert_eq!(delta, 2000.0 - 800.0 * ASSUMED_RATIO);
        // A page on screen changing size doesn't move the viewport.
        assert_eq!(s.set_size(2, (800, 400), 1500.0), 0.0);
    }

    #[test]
    fn window_covers_a_screen_above_and_two_below() {
        let s = strip(&[(800, 1000); 10]);
        assert_eq!(s.window(0.0, 1000.0), 0..4);
        assert_eq!(s.window(5000.0, 1000.0), 4..9);
        assert_eq!(s.window(9000.0, 1000.0), 8..10);
        assert_eq!(Strip::new(0, 800.0).window(0.0, 1000.0), 0..0);
    }

    #[test]
    fn at_end_once_the_last_page_bottom_shows() {
        let s = strip(&[(800, 1000); 3]);
        assert!(!s.at_end(1000.0, 1000.0));
        assert!(s.at_end(2000.0, 1000.0));
        assert!(!Strip::new(0, 800.0).at_end(0.0, 100.0));
    }

    #[test]
    fn spreads_pair_pages_after_the_first_and_keep_wide_ones_alone() {
        let s = strip(&[
            (800, 1200),
            (800, 1200),
            (800, 1200),
            (1600, 1200),
            (800, 1200),
            (800, 1200),
            (800, 1200),
        ]);
        assert_eq!(s.spreads(), [0..1, 1..3, 3..4, 4..6, 6..7]);
        assert!(Strip::new(0, 800.0).spreads().is_empty());
    }

    #[test]
    fn resizing_keeps_the_same_spot_in_view() {
        let mut s = strip(&[(800, 1000); 3]);
        // Halfway down page 1.
        let offset = s.set_width(400.0, 1500.0);
        assert_eq!(offset, 750.0);
        assert_eq!(s.page_at(offset), 1);
    }
}
