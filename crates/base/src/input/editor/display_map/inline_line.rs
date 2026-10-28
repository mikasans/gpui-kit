//! A visual row whose offsets remain in source UTF-8 bytes.
use gpui::{App, Pixels, Point, ShapedLine, SharedString, TextAlign, Window, point, px};
use ropey::Rope;
use std::ops::Range;

pub(crate) struct InlineFragment {
    pub(crate) range: Range<usize>,
    pub(crate) x: Pixels,
    pub(crate) width: Pixels,
    pub(crate) text: Option<ShapedLine>,
}

pub(crate) struct InputLine {
    pub(crate) len: usize,
    pub(crate) width: Pixels,
    pub(crate) text: SharedString,
    content: Content,
}
// A ShapedLine carries 32 decoration runs inline (~1.4 KB): kept inline, a
// 10 MB wrapped line's 50k rows rebuilt ~70 MB of rows every frame. Boxed,
// a row is small and only shaped rows pay for the allocation.
enum Content {
    Text(Box<ShapedLine>),
    Inline(Vec<InlineFragment>),
    Partial(Partial),
}

/// A row of a very long line that is only shaped in part: the bytes in
/// `window` (row-relative) are shaped and drawn at `x0`; everything else is
/// placed at an estimated `advance` per byte (exact for ASCII in a monospace
/// font) and not drawn. `text` is the whole document (a cheap rope clone:
/// copying a 10 MB line into every frame's layout dominated it) and
/// `row_start` this row's offset in it, so estimated indices can snap to
/// char boundaries.
pub(crate) struct Partial {
    text: Rope,
    row_start: usize,
    window: Range<usize>,
    x0: Pixels,
    shaped: Option<Box<ShapedLine>>,
    advance: Pixels,
}
impl From<ShapedLine> for InputLine {
    fn from(line: ShapedLine) -> Self {
        Self {
            len: line.len,
            width: line.width,
            text: line.text.clone(),
            content: Content::Text(Box::new(line)),
        }
    }
}
impl InputLine {
    /// A long-line row shaped only over `window` (row-relative bytes, on
    /// char boundaries; empty for a row that is off screen). See [`Partial`].
    pub(crate) fn partial(
        text: Rope,
        row: Range<usize>,
        window: Range<usize>,
        shaped: Option<ShapedLine>,
        advance: Pixels,
    ) -> Self {
        let len = row.len();
        let shaped_width = shaped.as_ref().map_or(px(0.), |l| l.width);
        let x0 = advance * window.start as f32;
        let width = x0 + shaped_width + advance * (len - window.end) as f32;
        Self {
            len,
            width,
            text: SharedString::default(),
            content: Content::Partial(Partial {
                text,
                row_start: row.start,
                window,
                x0,
                shaped: shaped.map(Box::new),
                advance,
            }),
        }
    }
    pub(crate) fn inline(text: SharedString, fragments: Vec<InlineFragment>) -> Self {
        let width = fragments.last().map_or(px(0.), |f| f.x + f.width);
        Self {
            len: text.len(),
            text,
            width,
            content: Content::Inline(fragments),
        }
    }
    pub(crate) fn x_for_index(&self, ix: usize) -> Pixels {
        match &self.content {
            Content::Text(line) => line.x_for_index(ix),
            Content::Partial(p) => {
                let shaped_width = p.shaped.as_ref().map_or(px(0.), |l| l.width);
                if ix < p.window.start || (p.window.is_empty() && ix == p.window.start) {
                    p.advance * ix as f32
                } else if ix <= p.window.end {
                    p.x0 + p
                        .shaped
                        .as_ref()
                        .map_or(px(0.), |l| l.x_for_index(ix - p.window.start))
                } else {
                    p.x0 + shaped_width + p.advance * (ix - p.window.end) as f32
                }
            }
            Content::Inline(fragments) => {
                for f in fragments {
                    if ix < f.range.end {
                        return f.x
                            + f.text.as_ref().map_or(
                                if ix <= f.range.start { px(0.) } else { f.width },
                                |line| line.x_for_index(ix.saturating_sub(f.range.start)),
                            );
                    }
                }
                self.width
            }
        }
    }
    pub(crate) fn closest_index_for_x(&self, x: Pixels) -> usize {
        match &self.content {
            Content::Text(line) => line.closest_index_for_x(x),
            Content::Partial(p) => {
                let shaped_width = p.shaped.as_ref().map_or(px(0.), |l| l.width);
                let estimate = |x: Pixels| {
                    if p.advance <= px(0.) {
                        0
                    } else {
                        (x / p.advance).round().max(0.) as usize
                    }
                };
                let ix = match &p.shaped {
                    None => estimate(x),
                    Some(_) if x < p.x0 => estimate(x).min(p.window.start),
                    Some(line) if x <= p.x0 + shaped_width => {
                        p.window.start + line.closest_index_for_x(x - p.x0)
                    }
                    Some(_) => p.window.end + estimate(x - p.x0 - shaped_width),
                };
                p.snap(ix.min(self.len))
            }
            Content::Inline(fragments) => {
                for f in fragments {
                    if x <= f.x + f.width {
                        return f.text.as_ref().map_or(
                            if x - f.x < f.width / 2. {
                                f.range.start
                            } else {
                                f.range.end
                            },
                            |line| f.range.start + line.closest_index_for_x(x - f.x),
                        );
                    }
                }
                self.len
            }
        }
    }
    pub(crate) fn index_for_x(&self, x: Pixels) -> Option<usize> {
        match &self.content {
            Content::Text(line) => line.index_for_x(x),
            Content::Inline(_) | Content::Partial(_) => {
                (x >= px(0.) && x <= self.width).then(|| self.closest_index_for_x(x))
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint(
        &self,
        pos: Point<Pixels>,
        height: Pixels,
        align: TextAlign,
        width: Option<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.paint_pass(pos, height, align, width, false, window, cx);
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_background(
        &self,
        pos: Point<Pixels>,
        height: Pixels,
        align: TextAlign,
        width: Option<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.paint_pass(pos, height, align, width, true, window, cx);
    }
    #[allow(clippy::too_many_arguments)]
    fn paint_pass(
        &self,
        pos: Point<Pixels>,
        height: Pixels,
        align: TextAlign,
        width: Option<Pixels>,
        background: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let paint = |line: &ShapedLine, pos, align, width, window: &mut Window, cx: &mut App| {
            if background {
                let _ = line.paint_background(pos, height, align, width, window, cx);
            } else {
                let _ = line.paint(pos, height, align, width, window, cx);
            }
        };
        match &self.content {
            Content::Text(line) => paint(line, pos, align, width, window, cx),
            Content::Partial(p) => {
                if let Some(line) = &p.shaped {
                    paint(
                        line,
                        pos + point(p.x0, px(0.)),
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }
            Content::Inline(fragments) => {
                let remaining = (width.unwrap_or(self.width) - self.width).max(px(0.));
                let offset = match align {
                    TextAlign::Right => remaining,
                    TextAlign::Center => remaining / 2.,
                    _ => px(0.),
                };
                for f in fragments {
                    if let Some(line) = &f.text {
                        paint(
                            line,
                            pos + point(offset + f.x, px(0.)),
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }
                }
            }
        }
    }
}

impl Partial {
    /// Move a row-relative index back to a char boundary of the line.
    fn snap(&self, ix: usize) -> usize {
        let at = (self.row_start + ix).min(self.text.len());
        self.text.floor_char_boundary(at).max(self.row_start) - self.row_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_off_screen_long_line_row_estimates_positions_and_snaps_to_chars() {
        // "aé" repeated: 3 bytes per pair; the row is bytes 3..15 of the line.
        let line = Rope::from("aé".repeat(10));
        let row = InputLine::partial(line, 3..15, 0..0, None, px(10.));
        assert_eq!(row.len, 12);
        assert_eq!(row.width, px(120.));
        assert_eq!(row.x_for_index(0), px(0.));
        assert_eq!(row.x_for_index(6), px(60.));
        // x = 20 estimates byte 2 of the row: line byte 5, inside "é"
        // (bytes 4..6), so it snaps back to the char's start.
        assert_eq!(row.closest_index_for_x(px(20.)), 1);
        assert_eq!(row.closest_index_for_x(px(1000.)), 12);
        assert_eq!(row.index_for_x(px(-1.)), None);
    }

    #[test]
    fn an_unwrapped_long_line_places_its_shaped_window_by_estimate() {
        let line = Rope::from("x".repeat(100));
        // A stand-in shape: 10 bytes, zero width.
        let shaped = ShapedLine::default().with_len(10);
        let row = InputLine::partial(line, 0..100, 40..50, Some(shaped), px(10.));
        assert_eq!(row.len, 100);
        // Before the window: estimated; after it: window width + estimate.
        assert_eq!(row.x_for_index(20), px(200.));
        assert_eq!(row.x_for_index(40), px(400.));
        assert_eq!(row.x_for_index(60), px(400.) + px(100.));
        assert_eq!(row.closest_index_for_x(px(100.)), 10);
        assert_eq!(row.closest_index_for_x(px(500.)), 60);
        // Round trip outside the window.
        assert_eq!(row.closest_index_for_x(row.x_for_index(80)), 80);
    }
}
