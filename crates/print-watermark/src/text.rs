//! Text for the marks, drawn as outlines from a bundled font.

use std::fmt::Write as _;

use ttf_parser::{Face, OutlineBuilder};

pub const REGULAR: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.ttf");
pub const BOLD: &[u8] = include_bytes!("../assets/JetBrainsMono-Bold.ttf");

/// Shown in place of a character the font has no glyph for.
const REPLACEMENT: char = '?';

pub struct Font {
    face: Face<'static>,
}

impl Font {
    /// Parses one of the bundled faces.
    ///
    /// # Panics
    ///
    /// If the bundled font is not a readable TrueType file, which would mean
    /// the build is broken rather than the input.
    pub fn new(data: &'static [u8]) -> Self {
        Font {
            face: Face::parse(data, 0).expect("the bundled font must parse"),
        }
    }

    pub fn regular() -> Self {
        Font::new(REGULAR)
    }

    pub fn bold() -> Self {
        Font::new(BOLD)
    }

    /// Font units per em, the scale all glyph coordinates are in.
    fn units_per_em(&self) -> f64 {
        f64::from(self.face.units_per_em())
    }

    /// The glyph for `ch`, falling back to `?` and reporting the substitution.
    fn glyph(&self, ch: char) -> (ttf_parser::GlyphId, bool) {
        match self.face.glyph_index(ch) {
            Some(glyph) => (glyph, false),
            None => (
                self.face
                    .glyph_index(REPLACEMENT)
                    .unwrap_or(ttf_parser::GlyphId(0)),
                true,
            ),
        }
    }

    /// Advance width of one glyph, in font units.
    fn advance(&self, glyph: ttf_parser::GlyphId) -> f64 {
        f64::from(self.face.glyph_hor_advance(glyph).unwrap_or(0))
    }

    /// Width of `text` set at `size` points.
    pub fn width(&self, text: &str, size: f64) -> f64 {
        let scale = size / self.units_per_em();
        text.chars()
            .map(|ch| self.advance(self.glyph(ch).0) * scale)
            .sum()
    }

    /// Distance from the baseline to the top of a capital, at `size` points.
    ///
    /// Used to centre text on something other than its baseline.
    pub fn cap_height(&self, size: f64) -> f64 {
        let units = self
            .face
            .capital_height()
            .filter(|h| *h > 0)
            .map(f64::from)
            .unwrap_or_else(|| self.units_per_em() * 0.7);
        units * size / self.units_per_em()
    }

    /// Appends the glyph outlines of `text` to `out` as PDF path operators,
    /// with the left end of the baseline at `(x, y)`.
    ///
    /// Only the paths are written: the caller decides whether to fill them
    /// (`f`), stroke them (`S`) or both (`B`), which is what makes one
    /// routine serve both the solid marks and the hollow overlay.
    ///
    /// Returns whether any character had to be replaced.
    pub fn outline(&self, text: &str, size: f64, x: f64, y: f64, out: &mut String) -> bool {
        let scale = size / self.units_per_em();
        let mut pen = x;
        let mut substituted = false;

        for ch in text.chars() {
            let (glyph, missing) = self.glyph(ch);
            substituted |= missing;

            let mut sink = Outline {
                out,
                scale,
                dx: pen,
                dy: y,
                last: (0.0, 0.0),
            };
            // A glyph with no outline, such as a space, simply advances.
            self.face.outline_glyph(glyph, &mut sink);
            pen += self.advance(glyph) * scale;
        }

        substituted
    }
}

/// Turns `ttf_parser`'s outline callbacks into PDF path operators.
struct Outline<'a> {
    out: &'a mut String,
    scale: f64,
    dx: f64,
    dy: f64,
    /// The current point, in PDF user space, for converting quadratics.
    last: (f64, f64),
}

impl Outline<'_> {
    fn point(&self, x: f32, y: f32) -> (f64, f64) {
        (
            self.dx + f64::from(x) * self.scale,
            self.dy + f64::from(y) * self.scale,
        )
    }
}

impl OutlineBuilder for Outline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.point(x, y);
        let _ = writeln!(self.out, "{x:.3} {y:.3} m");
        self.last = (x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.point(x, y);
        let _ = writeln!(self.out, "{x:.3} {y:.3} l");
        self.last = (x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        // PDF has no quadratic segment, so raise it to the equivalent cubic:
        // the control points sit two thirds of the way from each end point
        // towards the quadratic's single control point.
        let (qx, qy) = self.point(x1, y1);
        let (ex, ey) = self.point(x, y);
        let (sx, sy) = self.last;

        let c1 = (sx + 2.0 / 3.0 * (qx - sx), sy + 2.0 / 3.0 * (qy - sy));
        let c2 = (ex + 2.0 / 3.0 * (qx - ex), ey + 2.0 / 3.0 * (qy - ey));

        let _ = writeln!(
            self.out,
            "{:.3} {:.3} {:.3} {:.3} {ex:.3} {ey:.3} c",
            c1.0, c1.1, c2.0, c2.1
        );
        self.last = (ex, ey);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (c1x, c1y) = self.point(x1, y1);
        let (c2x, c2y) = self.point(x2, y2);
        let (ex, ey) = self.point(x, y);
        let _ = writeln!(
            self.out,
            "{c1x:.3} {c1y:.3} {c2x:.3} {c2y:.3} {ex:.3} {ey:.3} c"
        );
        self.last = (ex, ey);
    }

    fn close(&mut self) {
        self.out.push_str("h\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(text: &str) -> (String, bool) {
        let font = Font::bold();
        let mut out = String::new();
        let substituted = font.outline(text, 100.0, 0.0, 0.0, &mut out);
        (out, substituted)
    }

    #[test]
    fn both_bundled_faces_parse() {
        assert!(Font::regular().units_per_em() > 0.0);
        assert!(Font::bold().units_per_em() > 0.0);
    }

    #[test]
    fn the_font_is_monospaced_so_every_glyph_is_the_same_width() {
        let font = Font::regular();
        assert_eq!(font.width("MMM", 12.0), font.width("iii", 12.0));
        assert_eq!(font.width("", 12.0), 0.0);
        // And width scales with the type size.
        assert!((font.width("abc", 24.0) - 2.0 * font.width("abc", 12.0)).abs() < 1e-9);
    }

    #[test]
    fn width_is_proportional_to_the_number_of_characters() {
        let font = Font::regular();
        let one = font.width("0", 10.0);
        assert!(one > 0.0);
        assert!((font.width("00000", 10.0) - 5.0 * one).abs() < 1e-9);
    }

    #[test]
    fn a_digit_is_drawn_as_a_closed_path() {
        let (path, substituted) = draw("1");
        assert!(!substituted);
        assert!(path.contains(" m\n"), "no move_to:\n{path}");
        assert!(path.contains("h\n"), "not closed:\n{path}");
    }

    #[test]
    fn a_glyph_with_a_counter_has_more_than_one_subpath() {
        // "0" is a ring: an outer contour and an inner one. Non-zero winding
        // then leaves the middle empty, which is what makes the mark legible.
        let (path, _) = draw("0");
        let subpaths = path.matches(" m\n").count();
        assert!(subpaths >= 2, "expected a counter, got {subpaths}:\n{path}");
    }

    #[test]
    fn cyrillic_and_greek_are_drawn_rather_than_substituted() {
        for text in ["Команда", "Ομάδα", "Grüße"] {
            let (path, substituted) = draw(text);
            assert!(!substituted, "{text} was substituted");
            assert!(!path.is_empty(), "{text} drew nothing");
        }
    }

    #[test]
    fn a_character_the_font_lacks_is_substituted_and_reported() {
        let (path, substituted) = draw("\u{1F600}");
        assert!(substituted);
        // It still draws something, so the mark does not silently shrink.
        assert!(!path.is_empty());
    }

    #[test]
    fn a_space_advances_without_drawing() {
        let font = Font::regular();
        let mut out = String::new();
        font.outline(" ", 100.0, 0.0, 0.0, &mut out);
        assert!(out.is_empty(), "a space drew: {out}");
        assert!(font.width(" ", 100.0) > 0.0);
    }

    #[test]
    fn glyphs_are_placed_where_they_are_asked_for() {
        let font = Font::bold();
        let mut here = String::new();
        font.outline("1", 100.0, 0.0, 0.0, &mut here);
        let mut there = String::new();
        font.outline("1", 100.0, 500.0, 300.0, &mut there);

        assert_ne!(here, there);
        // Every coordinate in the shifted copy is further right and up.
        let first = |path: &str| -> (f64, f64) {
            let line = path.lines().next().unwrap();
            let mut parts = line.split_whitespace();
            (
                parts.next().unwrap().parse().unwrap(),
                parts.next().unwrap().parse().unwrap(),
            )
        };
        let (x0, y0) = first(&here);
        let (x1, y1) = first(&there);
        assert!((x1 - x0 - 500.0).abs() < 1e-3, "{x0} vs {x1}");
        assert!((y1 - y0 - 300.0).abs() < 1e-3, "{y0} vs {y1}");
    }

    #[test]
    fn successive_characters_advance_to_the_right() {
        let font = Font::bold();
        let mut one = String::new();
        font.outline("1", 100.0, 0.0, 0.0, &mut one);
        let mut two = String::new();
        font.outline("11", 100.0, 0.0, 0.0, &mut two);
        // The second glyph adds more path data, further right.
        assert!(two.len() > one.len());
        let rightmost = |path: &str| {
            path.lines()
                .filter_map(|l| l.split_whitespace().next())
                .filter_map(|v| v.parse::<f64>().ok())
                .fold(f64::MIN, f64::max)
        };
        assert!(rightmost(&two) > rightmost(&one));
    }

    #[test]
    fn cap_height_is_a_sensible_fraction_of_the_size() {
        let font = Font::bold();
        let cap = font.cap_height(100.0);
        assert!(cap > 50.0 && cap < 90.0, "{cap}");
    }

    #[test]
    fn quadratics_become_cubics_and_nothing_else_appears() {
        // TrueType outlines are quadratic, so a curved glyph must come out as
        // `c` operators and no stray operator types.
        let (path, _) = draw("S");
        assert!(path.contains(" c\n"), "no curves:\n{path}");
        for line in path.lines() {
            let op = line.split_whitespace().last().unwrap();
            assert!(
                matches!(op, "m" | "l" | "c" | "h"),
                "unexpected operator {op} in:\n{path}"
            );
        }
    }
}
