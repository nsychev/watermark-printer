//! Builds a watermarked PDF.

pub mod text;

use lopdf::{Dictionary, Object, Stream, dictionary};
use print_document::{Media, SourceDocument, SourcePage};

/// Sizes and shades of the marks, all in PDF points unless stated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    /// Clear space left at every edge of the sheet.
    pub margin: f64,
    /// Space between the header and the frame.
    pub gap: f64,
    /// Type size of the team, in the middle of the header.
    pub team_size: f64,
    /// Type size of the time and the page counter, at its ends.
    pub meta_size: f64,
    /// Stroke width of the frame.
    pub border_width: f64,
    /// Grey level of the frame, 0 black to 1 white.
    pub border_gray: f64,
    /// Largest type size used for the diagonal overlay. It is reduced to fit
    /// the page when the team name is long.
    pub overlay_size: f64,
    /// Angle of the diagonal overlay, in degrees anticlockwise.
    pub overlay_angle: f64,
    /// Grey level the overlay glyphs are filled with, or `None` to leave them
    /// hollow.
    pub overlay_fill_gray: Option<f64>,
    /// Grey level the overlay glyphs are outlined with.
    pub overlay_stroke_gray: f64,
    /// Outline width, as a fraction of the type size.
    pub overlay_stroke_width: f64,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            margin: 14.0,
            gap: 7.0,
            team_size: 18.0,
            // Large enough to read without stooping over the page.
            meta_size: 14.0,
            border_width: 0.6,
            border_gray: 0.25,
            overlay_size: 150.0,
            overlay_angle: 26.0,
            // Hollow, so nothing on the page is covered up.
            overlay_fill_gray: None,
            overlay_stroke_gray: 0.30,
            overlay_stroke_width: 0.030,
        }
    }
}

impl Layout {
    /// A sheet smaller than this in either direction has no room for a header
    /// above the page, so a framed style falls back to an overlay.
    const MIN_FRAMEABLE: f64 = 120.0;

    /// Height reserved above the frame for the header.
    fn header_band(&self) -> f64 {
        self.team_size.max(self.meta_size)
    }
}

/// What to write on the page.
#[derive(Debug, Clone, Copy)]
pub struct Stamp<'a> {
    /// The team identifier, shown in the middle of the header.
    pub team: &'a str,
    /// When the job arrived, already formatted, shown at the left.
    pub timestamp: &'a str,
}

/// A finished PDF.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub bytes: Vec<u8>,
    pub pages: usize,
    /// Whether any mark contained characters the standard font cannot show,
    /// which were replaced with `?`.
    pub lossy_text: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pdf error: {0}")]
    Pdf(#[from] lopdf::Error),
    #[error("failed to serialise the pdf: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Marks every page of `source` and serialises the result.
pub fn render(
    source: SourceDocument,
    stamp: &Stamp<'_>,
    layout: &Layout,
    sheet: Media,
) -> Result<Rendered> {
    let (mut doc, pages) = source.into_parts();
    let total = pages.len();
    let mut lossy_text = false;

    // The marks are drawn as paths, so the output references no font at all
    // and the faces are parsed once for the whole document.
    let fonts = Fonts {
        regular: text::Font::regular(),
        bold: text::Font::bold(),
    };

    // The page tree is referenced by its children, so its id is reserved
    // before the pages are built and filled in afterwards.
    let pages_id = doc.new_object_id();
    let mut kids = Vec::with_capacity(total);

    for (index, page) in pages.iter().enumerate() {
        let (geometry, content, lossy) =
            page_content(&fonts, layout, stamp, page, index + 1, total, sheet);
        lossy_text |= lossy;

        let content_id = doc.add_object(Stream::new(Dictionary::new(), content.into_bytes()));

        let mut xobjects = Dictionary::new();
        xobjects.set("Pg", Object::Reference(page.form));

        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![
                0.into(),
                0.into(),
                Object::Real(geometry.sheet.0 as f32),
                Object::Real(geometry.sheet.1 as f32),
            ],
            "Resources" => dictionary! {
                "XObject" => xobjects,
                "ProcSet" => vec!["PDF".into(), "ImageB".into(), "ImageC".into()],
            },
        });
        kids.push(Object::Reference(page_id));
    }

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Count" => total as i64,
            "Kids" => kids,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    // A document that arrived encrypted was decrypted on the way in; leaving
    // the trailer entry behind would describe a file we are not writing.
    doc.trailer.remove(b"Encrypt");

    // The incoming page tree and its content streams are now unreferenced —
    // the content was copied into the forms — so drop them rather than ship
    // every page twice.
    doc.prune_objects();
    doc.compress();

    let mut bytes = Vec::new();
    doc.save_to(&mut bytes)?;

    Ok(Rendered {
        bytes,
        pages: total,
        lossy_text,
    })
}

/// The two faces the marks are drawn with.
struct Fonts {
    regular: text::Font,
    bold: text::Font,
}

/// Everything the drawing routines need besides the page itself.
struct Marks<'a> {
    fonts: &'a Fonts,
    layout: &'a Layout,
    stamp: &'a Stamp<'a>,
}

/// Where the page goes on the sheet, and the sheet it goes on.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Geometry {
    /// The output sheet, which is always portrait when the page is framed.
    sheet: (f64, f64),
    /// The `cm` matrix that places the source page.
    matrix: [f64; 6],
    /// Where the placed page lands, for drawing the frame around it.
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    /// Whether the page was turned a quarter to fit the sheet.
    turned: bool,
}

impl Geometry {
    /// The sheet a page is printed on, always in portrait orientation.
    fn sheet_for(sheet: Media) -> (f64, f64) {
        if sheet.width > sheet.height {
            (sheet.height, sheet.width)
        } else {
            (sheet.width, sheet.height)
        }
    }

    /// Fits `page` onto `sheet`, leaving room for the header above it.
    fn frame(layout: &Layout, page: &SourcePage, sheet: Media) -> Self {
        let sheet = Self::sheet_for(sheet);
        let turned = page.width > page.height;

        // What the page measures once it has been turned.
        let (shown_width, shown_height) = if turned {
            (page.height, page.width)
        } else {
            (page.width, page.height)
        };

        let box_x = layout.margin;
        let box_w = sheet.0 - 2.0 * box_x;
        let box_y = layout.margin;
        let box_top = sheet.1 - layout.margin - layout.header_band() - layout.gap;
        let box_h = box_top - box_y;

        // Never enlarge: a page that already fits is left at its own size,
        // only moved, so a small sheet is not blown up to fill the frame.
        let scale = (box_w / shown_width).min(box_h / shown_height).min(1.0);
        let width = shown_width * scale;
        let height = shown_height * scale;
        let x = box_x + (box_w - width) / 2.0;
        let y = box_y + (box_h - height) / 2.0;

        // A quarter turn clockwise, which puts the page's bottom edge up the
        // left of the sheet, so it is read by turning the sheet
        // anticlockwise. That is the way CUPS lays landscape work out, and a
        // deployment can see both: CUPS turns the page when a client drives a
        // portrait queue, and this turns it when a landscape page arrives
        // whole.
        let matrix = if turned {
            [0.0, -scale, scale, 0.0, x, y + height]
        } else {
            [scale, 0.0, 0.0, scale, x, y]
        };

        Geometry {
            sheet,
            matrix,
            x,
            y,
            width,
            height,
            turned,
        }
    }

    /// How much the page was scaled by, however it was placed.
    #[cfg(test)]
    fn scale(&self) -> f64 {
        // The first column of the matrix is the scaled x axis, turned or not.
        self.matrix[0].hypot(self.matrix[1])
    }

    /// Reproduces `page` at its own size, in place, on its own sheet.
    fn untouched(page: &SourcePage) -> Self {
        Geometry {
            sheet: (page.width, page.height),
            matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            x: 0.0,
            y: 0.0,
            width: page.width,
            height: page.height,
            turned: false,
        }
    }
}

/// Builds one output page's content stream.
fn page_content(
    fonts: &Fonts,
    layout: &Layout,
    stamp: &Stamp<'_>,
    page: &SourcePage,
    number: usize,
    total: usize,
    sheet: Media,
) -> (Geometry, String, bool) {
    // A sheet too small to carry a header gets the overlay alone, so a label
    // or an envelope still comes out marked rather than unreadable.
    let framed = page.width >= Layout::MIN_FRAMEABLE && page.height >= Layout::MIN_FRAMEABLE;

    let geometry = if framed {
        Geometry::frame(layout, page, sheet)
    } else {
        Geometry::untouched(page)
    };

    let marks = Marks {
        fonts,
        layout,
        stamp,
    };
    let mut out = String::with_capacity(512);
    let mut lossy = false;

    // The page first, so that the overlay lands on top of it.
    let [a, b, c, d, e, f] = geometry.matrix;
    out.push_str("q\n");
    out.push_str(&format!("{a:.4} {b:.4} {c:.4} {d:.4} {e:.4} {f:.4} cm\n"));
    out.push_str("/Pg Do\nQ\n");

    if framed {
        lossy |= draw_frame(&mut out, &marks, &geometry, number, total);
    }
    lossy |= draw_overlay(&mut out, &marks, geometry.sheet);

    (geometry, out, lossy)
}

/// Draws the rectangle and the header above a scaled-down page.
///
/// The header is one line: the time at the left, the team in the middle and
/// the page counter at the right, all sharing a baseline. Nothing is drawn
/// over the page itself, so nothing the document says is obscured.
fn draw_frame(
    out: &mut String,
    marks: &Marks<'_>,
    geometry: &Geometry,
    number: usize,
    total: usize,
) -> bool {
    let (sheet_width, sheet_height) = geometry.sheet;
    let Marks {
        fonts,
        layout,
        stamp,
    } = marks;
    let mut lossy = false;

    // Frame the page in the rectangle.
    out.push_str("q\n");
    out.push_str(&format!(
        "{:.3} G {:.3} w\n{:.4} {:.4} {:.4} {:.4} re S\nQ\n",
        layout.border_gray,
        layout.border_width,
        geometry.x,
        geometry.y,
        geometry.width,
        geometry.height,
    ));

    let counter = format!("{number} / {total}");
    let header = Header::fit(
        fonts,
        layout,
        sheet_width,
        stamp.timestamp,
        stamp.team,
        &counter,
    );

    // All three sit on one baseline, with the team's capitals just inside the
    // top margin.
    let baseline = sheet_height - layout.margin - fonts.bold.cap_height(header.team_size);

    lossy |= fill(
        out,
        &fonts.regular,
        stamp.timestamp,
        header.meta_size,
        layout.margin,
        baseline,
        0.0,
    );
    lossy |= fill(
        out,
        &fonts.bold,
        stamp.team,
        header.team_size,
        (sheet_width - header.team_width) / 2.0,
        baseline,
        0.0,
    );
    lossy |= fill(
        out,
        &fonts.regular,
        &counter,
        header.meta_size,
        sheet_width - layout.margin - header.counter_width,
        baseline,
        0.0,
    );

    lossy
}

/// Type sizes for the header, reduced together until the three items fit.
///
/// A long team name on a narrow sheet would otherwise run into the time or
/// the counter. Shrinking all three by the same factor keeps the line looking
/// deliberate rather than squeezed in one place.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Header {
    team_size: f64,
    meta_size: f64,
    team_width: f64,
    counter_width: f64,
}

impl Header {
    fn fit(
        fonts: &Fonts,
        layout: &Layout,
        sheet_width: f64,
        timestamp: &str,
        team: &str,
        counter: &str,
    ) -> Self {
        let available = sheet_width - 2.0 * layout.margin;
        let natural = fonts.regular.width(timestamp, layout.meta_size)
            + fonts.bold.width(team, layout.team_size)
            + fonts.regular.width(counter, layout.meta_size)
            // Two gaps, so the items never touch.
            + 2.0 * layout.gap;

        // Widths are linear in the type size, so one division is exact.
        let factor = if natural > available && natural > 0.0 {
            available / natural
        } else {
            1.0
        };

        let team_size = layout.team_size * factor;
        let meta_size = layout.meta_size * factor;
        Header {
            team_size,
            meta_size,
            team_width: fonts.bold.width(team, team_size),
            counter_width: fonts.regular.width(counter, meta_size),
        }
    }
}

/// Draws solid text, as the frame's labels are.
fn fill(
    out: &mut String,
    font: &text::Font,
    value: &str,
    size: f64,
    x: f64,
    y: f64,
    gray: f64,
) -> bool {
    let mut path = String::new();
    let lossy = font.outline(value, size, x, y, &mut path);
    if path.is_empty() {
        return lossy;
    }
    // Non-zero winding, so a counter such as the middle of an "o" stays open.
    out.push_str(&format!("q {gray:.3} g\n{path}f\nQ\n"));
    lossy
}

/// Draws the diagonal team number across the middle of the page.
///
/// Text rendering mode 2 fills *and* strokes each glyph: a pale fill keeps the
/// mark obvious at a glance, while the darker outline keeps its shape readable
/// where it crosses dark content, and neither hides the text underneath the
/// way a solid grey block does.
fn draw_overlay(out: &mut String, marks: &Marks<'_>, sheet: (f64, f64)) -> bool {
    let (sheet_width, sheet_height) = sheet;
    let Marks {
        fonts,
        layout,
        stamp,
    } = marks;
    if stamp.team.is_empty() {
        return false;
    }

    let angle = layout.overlay_angle.to_radians();
    let (sin, cos) = angle.sin_cos();

    // Shrink the type until the word spans no more than 80% of the diagonal
    // it is drawn along, so a long team name cannot run off the sheet.
    let diagonal = sheet_width.hypot(sheet_height) * 0.8;
    let at_full_size = fonts.bold.width(stamp.team, layout.overlay_size);
    let size = if at_full_size > diagonal {
        // Width is linear in the type size, so one division is exact.
        (layout.overlay_size * diagonal / at_full_size).max(6.0)
    } else {
        layout.overlay_size
    };
    let width = fonts.bold.width(stamp.team, size);

    // The glyphs are drawn around the origin and the whole lot is then moved
    // to the middle of the page and turned, which keeps the outline maths
    // free of the rotation.
    let mut path = String::new();
    let lossy = fonts.bold.outline(
        stamp.team,
        size,
        -width / 2.0,
        // Centre the capitals on the middle of the page rather than the
        // baseline, which would sit visibly low.
        -fonts.bold.cap_height(size) / 2.0,
        &mut path,
    );
    if path.is_empty() {
        return lossy;
    }

    out.push_str("q\n");
    out.push_str(&format!(
        "1 0 0 1 {:.4} {:.4} cm\n",
        sheet_width / 2.0,
        sheet_height / 2.0
    ));
    out.push_str(&format!("{cos:.6} {sin:.6} {:.6} {cos:.6} 0 0 cm\n", -sin));
    if let Some(gray) = layout.overlay_fill_gray {
        out.push_str(&format!("{gray:.3} g\n"));
    }
    out.push_str(&format!(
        "{:.3} G {:.3} w\n",
        layout.overlay_stroke_gray,
        (size * layout.overlay_stroke_width).max(0.4),
    ));
    out.push_str(&path);
    // `B` fills and strokes, `S` only strokes. Stroking alone is what leaves
    // the page beneath the mark readable.
    out.push_str(if layout.overlay_fill_gray.is_some() {
        "B\n"
    } else {
        "S\n"
    });
    out.push_str("Q\n");

    lossy
}

#[cfg(test)]
mod tests {
    use super::*;
    use print_document::{Media, jpeg};

    fn stamp() -> Stamp<'static> {
        Stamp {
            team: "042",
            timestamp: "2026-11-04 07:30:00",
        }
    }

    /// A one-page source document of the given size, via a JPEG of that shape.
    fn source(width: f64, height: f64) -> SourceDocument {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08];
        bytes.extend_from_slice(&(height as u16).to_be_bytes());
        bytes.extend_from_slice(&(width as u16).to_be_bytes());
        bytes.extend_from_slice(&[3, 0, 0, 0]);
        bytes.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        jpeg::load(&bytes, Media::new(width, height)).unwrap()
    }

    fn page(width: f64, height: f64) -> SourcePage {
        SourcePage {
            form: (1, 0),
            width,
            height,
        }
    }

    fn fonts() -> Fonts {
        Fonts {
            regular: text::Font::regular(),
            bold: text::Font::bold(),
        }
    }

    fn content(layout: &Layout, page: &SourcePage) -> String {
        page_content(&fonts(), layout, &stamp(), page, 2, 7, Media::A4).1
    }

    /// The overlay's part of a content stream.
    ///
    /// The frame is always drawn as well, so assertions about the overlay
    /// have to look at the overlay. It begins with the only translation
    /// written with bare integers.
    fn overlay_of(body: &str) -> &str {
        let start = body.rfind("1 0 0 1 ").expect("no overlay in the stream");
        &body[start..]
    }

    /// How many glyphs a content stream draws, counted by their subpaths'
    /// starts. The marks are outlines, so there is no text to search for.
    fn subpaths(body: &str) -> usize {
        body.lines().filter(|l| l.ends_with(" m")).count()
    }

    #[test]
    fn framing_scales_the_page_down_and_centres_it() {
        let a4 = page(595.276, 841.89);
        let geometry = Geometry::frame(&Layout::default(), &a4, Media::A4);

        assert!(geometry.scale() < 1.0, "{geometry:?}");
        assert!(geometry.scale() > 0.9, "{geometry:?}");
        // Aspect ratio is kept.
        let ratio = geometry.width / geometry.height;
        assert!((ratio - a4.width / a4.height).abs() < 1e-9);
        // Centred horizontally.
        let left = geometry.x;
        let right = a4.width - (geometry.x + geometry.width);
        assert!((left - right).abs() < 1e-6, "{left} vs {right}");
        // Clear of the sheet edges.
        let layout = Layout::default();
        assert!(geometry.x >= layout.margin - 1e-9, "{geometry:?}");
        assert!(geometry.y >= layout.margin - 1e-9, "{geometry:?}");
    }

    #[test]
    fn the_header_clears_the_frame_and_the_frame_clears_the_sheet() {
        let layout = Layout::default();
        let fonts = fonts();
        let a4 = page(595.276, 841.89);
        let geometry = Geometry::frame(&layout, &a4, Media::A4);

        let baseline = a4.height - layout.margin - fonts.bold.cap_height(layout.team_size);
        let frame_top = geometry.y + geometry.height;
        // The header's baseline sits above the frame, with its descenders
        // clear of it.
        assert!(
            baseline - layout.team_size * 0.25 > frame_top,
            "baseline {baseline} vs frame top {frame_top}"
        );
        // And its capitals stay inside the top margin.
        assert!(
            baseline + fonts.bold.cap_height(layout.team_size) <= a4.height - layout.margin + 1e-9
        );

        // With no footer, the frame reaches down to the bottom margin.
        let frame_bottom = geometry.y;
        assert!(
            (frame_bottom - layout.margin).abs() < 1e-9,
            "frame bottom {frame_bottom} vs margin {}",
            layout.margin
        );
    }

    #[test]
    fn the_frame_draws_a_rectangle_and_all_three_labels() {
        let body = content(&Layout::default(), &page(595.276, 841.89));
        assert!(body.contains(" re S"), "no frame:\n{body}");
        assert!(body.contains("/Pg Do"), "page not drawn:\n{body}");

        // Three filled runs of outlines: team, timestamp, page counter.
        assert_eq!(body.matches("f\n").count(), 3, "{body}");
        // The marks reference no font, so none of PDF's text operators
        // should appear at all.
        for operator in ["BT", "ET", "Tj", "Tf", "Td", "Tr"] {
            assert!(!body.contains(operator), "found {operator} in:\n{body}");
        }

        // "042" plus "2026-11-04 07:30:00  job 1042" plus "Page 2 / 7" is a
        // lot of glyphs, each at least one subpath.
        assert!(subpaths(&body) > 30, "only {} subpaths", subpaths(&body));
    }

    #[test]
    fn the_labels_change_with_what_is_stamped() {
        let layout = Layout::default();
        let a4 = page(595.276, 841.89);
        let one = page_content(&fonts(), &layout, &stamp(), &a4, 2, 7, Media::A4).1;
        let other = page_content(
            &fonts(),
            &layout,
            &Stamp {
                team: "999",
                timestamp: "2026-01-01 00:00:00",
            },
            &a4,
            1,
            1,
            Media::A4,
        )
        .1;
        assert_ne!(one, other);
    }

    #[test]
    fn the_page_counter_is_right_aligned_within_the_margin() {
        let a4 = page(595.276, 841.89);
        let layout = Layout::default();
        let body = content(&layout, &a4);

        let font = text::Font::regular();
        let left_edge = a4.width - layout.margin - font.width("2 / 7", layout.meta_size);
        assert!(
            left_edge > a4.width / 2.0,
            "the counter is not on the right"
        );

        // Something is drawn out at the right-hand end, and nothing runs past
        // the margin.
        let xs: Vec<f64> = body
            .lines()
            .filter(|l| l.ends_with(" m") || l.ends_with(" l"))
            .filter_map(|l| l.split_whitespace().next()?.parse().ok())
            .collect();
        assert!(!xs.is_empty());
        let rightmost = xs.iter().cloned().fold(f64::MIN, f64::max);
        assert!(rightmost > left_edge - 1.0, "{rightmost} vs {left_edge}");
        assert!(
            rightmost <= a4.width - layout.margin + 1.0,
            "a mark runs past the margin: {rightmost}"
        );
    }

    #[test]
    fn every_page_gets_both_the_frame_and_the_overlay() {
        let body = content(&Layout::default(), &page(595.276, 841.89));
        // The frame, its three filled labels, and a stroked overlay.
        assert!(body.contains(" re S"), "no frame:\n{body}");
        assert_eq!(body.matches("f\n").count(), 3, "{body}");
        assert!(body.contains("S\n"), "no overlay:\n{body}");
        // The page is scaled down, as framing requires.
        assert!(
            !body.contains("1.0000 0 0 1.0000 0.0000 0.0000 cm"),
            "{body}"
        );
    }

    /// The three header items, as drawn, with their horizontal extents.
    fn header_extents(layout: &Layout, page: &SourcePage) -> (f64, f64, f64, f64) {
        let fonts = fonts();
        let counter = "2 / 7";
        let header = Header::fit(
            &fonts,
            layout,
            page.width,
            stamp().timestamp,
            stamp().team,
            counter,
        );
        let time_right = layout.margin + fonts.regular.width(stamp().timestamp, header.meta_size);
        let team_left = (page.width - header.team_width) / 2.0;
        let team_right = team_left + header.team_width;
        let counter_left = page.width - layout.margin - header.counter_width;
        (time_right, team_left, team_right, counter_left)
    }

    #[test]
    fn the_header_fits_every_sheet_this_printer_offers() {
        let layout = Layout::default();
        // Portrait and landscape, A4 and US Letter.
        for (name, sheet) in [
            ("A4 portrait", page(595.276, 841.89)),
            ("A4 landscape", page(841.89, 595.276)),
            ("Letter portrait", page(612.0, 792.0)),
            ("Letter landscape", page(792.0, 612.0)),
        ] {
            let (time_right, team_left, team_right, counter_left) = header_extents(&layout, &sheet);

            assert!(
                time_right < team_left,
                "{name}: the time runs into the team ({time_right} vs {team_left})"
            );
            assert!(
                team_right < counter_left,
                "{name}: the team runs into the counter ({team_right} vs {counter_left})"
            );
            assert!(
                counter_left + 1.0 >= sheet.width / 2.0,
                "{name}: the counter is not on the right"
            );

            // And the page still gets most of the sheet.
            let geometry = Geometry::frame(&layout, &sheet, Media::new(sheet.width, sheet.height));
            assert!(
                geometry.scale() > 0.85,
                "{name}: scaled to {}",
                geometry.scale()
            );
        }
    }

    #[test]
    fn the_header_shrinks_rather_than_overlapping_on_a_narrow_sheet() {
        let layout = Layout::default();
        // A5 portrait is only 420 points wide.
        let a5 = page(419.5, 595.276);
        let long = Stamp {
            team: "A-VERY-LONG-TEAM-NAME",
            timestamp: "2026-11-04 07:30:00",
        };
        let fonts = fonts();
        let header = Header::fit(
            &fonts,
            &layout,
            a5.width,
            long.timestamp,
            long.team,
            "2 / 7",
        );

        // It had to shrink...
        assert!(header.team_size < layout.team_size, "{header:?}");
        assert!(header.meta_size < layout.meta_size, "{header:?}");
        // ...and having shrunk, it fits.
        let total = fonts.regular.width(long.timestamp, header.meta_size)
            + header.team_width
            + header.counter_width;
        assert!(
            total <= a5.width - 2.0 * layout.margin + 1e-6,
            "{total} does not fit {}",
            a5.width - 2.0 * layout.margin
        );
    }

    #[test]
    fn a_header_that_already_fits_is_left_at_its_configured_size() {
        let layout = Layout::default();
        let fonts = fonts();
        let header = Header::fit(
            &fonts,
            &layout,
            841.89,
            stamp().timestamp,
            stamp().team,
            "2 / 7",
        );
        assert_eq!(header.team_size, layout.team_size);
        assert_eq!(header.meta_size, layout.meta_size);
        // And the requested size is the one the user asked for.
        assert!(layout.meta_size >= 14.0);
    }

    #[test]
    fn the_overlay_is_rotated_and_stroked_but_not_filled() {
        let layout = Layout::default();
        let body = content(&layout, &page(595.276, 841.89));

        // Stroked, never filled, so nothing underneath is covered.
        let overlay = overlay_of(&body);
        assert!(overlay.contains("S\n"), "{overlay}");
        assert!(
            !overlay.contains("f\n"),
            "overlay must not fill:\n{overlay}"
        );
        assert!(
            !overlay.contains("B\n"),
            "overlay must not fill:\n{overlay}"
        );
        assert!(overlay.contains("0.300 G"), "{overlay}");
        // Rotated by the configured angle.
        let cos = layout.overlay_angle.to_radians().cos();
        assert!(body.contains(&format!("{cos:.6}")), "{body}");
    }

    #[test]
    fn an_overlay_fill_can_be_asked_for_and_then_it_fills() {
        let layout = Layout {
            overlay_fill_gray: Some(0.8),
            ..Layout::default()
        };
        let body = content(&layout, &page(595.276, 841.89));
        let overlay = overlay_of(&body);
        assert!(overlay.contains("0.800 g"), "{overlay}");
        assert!(overlay.contains("B\n"), "{overlay}");
        assert!(!overlay.contains("S\n"), "{overlay}");
    }

    #[test]
    fn a_long_team_name_shrinks_to_stay_on_the_sheet() {
        let layout = Layout::default();
        let a4 = page(595.276, 841.89);
        let long = "A-VERY-LONG-TEAM-NAME-INDEED";
        let body = page_content(
            &fonts(),
            &layout,
            &Stamp {
                team: long,
                timestamp: "now",
            },
            &a4,
            1,
            1,
            Media::A4,
        )
        .1;

        // The glyphs are drawn around the origin before being rotated, so the
        // span is twice the furthest coordinate from it.
        let body = overlay_of(&body).to_string();
        let furthest = body
            .lines()
            .filter(|l| l.ends_with(" m") || l.ends_with(" l"))
            .filter_map(|l| l.split_whitespace().next()?.parse::<f64>().ok())
            .fold(0.0f64, |acc, x| acc.max(x.abs()));
        let diagonal = a4.width.hypot(a4.height) * 0.8;
        assert!(
            furthest * 2.0 <= diagonal + 1.0,
            "span {} exceeds the diagonal {diagonal}",
            furthest * 2.0
        );
        // And it did have to shrink to get there.
        assert!(text::Font::bold().width(long, layout.overlay_size) > diagonal);
    }

    #[test]
    fn a_sheet_too_small_to_frame_falls_back_to_an_overlay() {
        // A 4x6 label is wide enough; a stamp is not.
        let body = content(&Layout::default(), &page(100.0, 60.0));
        assert!(!body.contains(" re S"), "a stamp was framed:\n{body}");
        // No header either, so the only marks are the overlay's.
        assert!(overlay_of(&body).contains("S\n"), "{body}");
        // And the page is reproduced at full size, in place.
        assert!(
            body.contains("1.0000 0.0000 0.0000 1.0000 0.0000 0.0000 cm"),
            "{body}"
        );
    }

    #[test]
    fn a_landscape_page_is_stood_up_on_a_portrait_sheet() {
        let layout = Layout::default();
        let landscape = page(841.89, 595.276);
        let geometry = Geometry::frame(&layout, &landscape, Media::A4);

        // The sheet is portrait, whatever shape the page was.
        assert!(geometry.sheet.1 > geometry.sheet.0, "{geometry:?}");
        assert_eq!(geometry.sheet, (595.276, 841.89));
        assert!(geometry.turned);

        // The placed page is wider than it is tall no longer: turning it
        // swapped its edges.
        assert!(geometry.height > geometry.width, "{geometry:?}");

        // A quarter turn clockwise, so the matrix has a zero diagonal and
        // the page's own "up" becomes the sheet's "right".
        let [a, b, c, d, ..] = geometry.matrix;
        assert_eq!((a, d), (0.0, 0.0));
        assert!(b < 0.0 && c > 0.0, "{geometry:?}");
        // And it is a pure rotation and scale: both axes the same length.
        assert!((b.abs() - c.abs()).abs() < 1e-9);
    }

    #[test]
    fn a_portrait_page_is_left_the_way_it_is() {
        let layout = Layout::default();
        let geometry = Geometry::frame(&layout, &page(595.276, 841.89), Media::A4);
        assert!(!geometry.turned);
        assert_eq!(geometry.sheet, (595.276, 841.89));
        let [a, b, c, d, ..] = geometry.matrix;
        assert_eq!((b, c), (0.0, 0.0));
        assert!(a > 0.0 && d > 0.0);
    }

    #[test]
    fn the_header_is_in_the_same_place_whatever_shape_the_page_was() {
        let layout = Layout::default();
        // The same A4 sheet, fed portrait and landscape.
        let portrait = content(&layout, &page(595.276, 841.89));
        let landscape = content(&layout, &page(841.89, 595.276));

        // The marks are drawn at the same coordinates in both, because both
        // are laid out on the same portrait sheet. Only the `cm` that places
        // the page itself differs.
        let marks = |body: &str| -> Vec<String> {
            body.lines()
                .skip_while(|l| !l.starts_with("0.250 G"))
                .map(str::to_string)
                .collect()
        };
        assert_eq!(marks(&portrait), marks(&landscape));
    }

    #[test]
    fn a_turned_page_lands_inside_its_frame() {
        let layout = Layout::default();
        let landscape = page(841.89, 595.276);
        let geometry = Geometry::frame(&layout, &landscape, Media::A4);

        // Map the source page's four corners through the matrix and check
        // they all land in the framed box.
        let [a, b, c, d, e, f] = geometry.matrix;
        for (px, py) in [
            (0.0, 0.0),
            (landscape.width, 0.0),
            (0.0, landscape.height),
            (landscape.width, landscape.height),
        ] {
            let x = a * px + c * py + e;
            let y = b * px + d * py + f;
            assert!(
                x >= geometry.x - 1e-6 && x <= geometry.x + geometry.width + 1e-6,
                "corner ({px}, {py}) lands at x {x}, outside {}..{}",
                geometry.x,
                geometry.x + geometry.width
            );
            assert!(
                y >= geometry.y - 1e-6 && y <= geometry.y + geometry.height + 1e-6,
                "corner ({px}, {py}) lands at y {y}, outside {}..{}",
                geometry.y,
                geometry.y + geometry.height
            );
        }
    }

    #[test]
    fn a_cyrillic_team_name_prints_properly() {
        let rendered = render(
            source(595.0, 842.0),
            &Stamp {
                team: "Команда",
                timestamp: "now",
            },
            &Layout::default(),
            Media::A4,
        )
        .unwrap();
        // The bundled font covers Cyrillic, so nothing is substituted.
        assert!(!rendered.lossy_text);
    }

    #[test]
    fn a_team_name_the_font_cannot_show_is_reported() {
        let rendered = render(
            source(595.0, 842.0),
            &Stamp {
                team: "\u{1F600}",
                timestamp: "now",
            },
            &Layout::default(),
            Media::A4,
        )
        .unwrap();
        assert!(rendered.lossy_text);
    }

    #[test]
    fn rendering_produces_a_loadable_pdf_with_one_page_per_source_page() {
        let rendered = render(
            source(595.0, 842.0),
            &stamp(),
            &Layout::default(),
            Media::A4,
        )
        .unwrap();
        assert_eq!(rendered.pages, 1);
        assert!(!rendered.lossy_text);
        assert!(rendered.bytes.starts_with(b"%PDF-"), "no header");

        let reloaded = lopdf::Document::load_mem(&rendered.bytes).unwrap();
        assert_eq!(reloaded.get_pages().len(), 1);

        // The page carries the form and nothing else it does not need.
        let page_id = *reloaded.get_pages().values().next().unwrap();
        let page = reloaded.get_dictionary(page_id).unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        assert!(
            resources
                .get(b"XObject")
                .unwrap()
                .as_dict()
                .unwrap()
                .has(b"Pg")
        );
        // The marks are outlines, so the output embeds and references no font.
        assert!(!resources.has(b"Font"), "the output references a font");
    }

    #[test]
    fn marking_a_page_costs_kilobytes_rather_than_an_embedded_font() {
        // Everything a marked page adds is in its content stream: the frame,
        // three labels and the overlay, all as outlines.
        let body = content(&Layout::default(), &page(595.276, 841.89));
        // Nothing like the ~270 KB one embedded face would cost.
        assert!(
            body.len() < 40_000,
            "the marks cost {} bytes a page",
            body.len()
        );

        // And the finished PDF references no font, so nothing is embedded.
        let rendered = render(
            source(595.0, 842.0),
            &stamp(),
            &Layout::default(),
            Media::A4,
        )
        .unwrap();
        let reloaded = lopdf::Document::load_mem(&rendered.bytes).unwrap();
        let page_id = *reloaded.get_pages().values().next().unwrap();
        let resources = reloaded
            .get_dictionary(page_id)
            .unwrap()
            .get(b"Resources")
            .unwrap()
            .as_dict()
            .unwrap();
        assert!(!resources.has(b"Font"));
    }

    #[test]
    fn the_original_page_tree_does_not_survive_into_the_output() {
        let rendered = render(
            source(595.0, 842.0),
            &stamp(),
            &Layout::default(),
            Media::A4,
        )
        .unwrap();
        let reloaded = lopdf::Document::load_mem(&rendered.bytes).unwrap();
        // Exactly one page tree root and one catalog, i.e. the ones we built.
        let catalogs = reloaded
            .objects
            .values()
            .filter(|o| {
                o.as_dict()
                    .map(|d| {
                        d.get(b"Type").and_then(Object::as_name).ok() == Some(b"Catalog".as_slice())
                    })
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(catalogs, 1);
    }

    #[test]
    fn a_letter_page_is_fitted_to_the_sheet_the_printer_offers() {
        let layout = Layout::default();
        let letter = page(612.0, 792.0);
        let geometry = Geometry::frame(&layout, &letter, Media::A4);

        // The sheet is the Printer's, not the one the Client happened to send.
        assert!((geometry.sheet.0 - Media::A4.width).abs() < 0.01);
        assert!((geometry.sheet.1 - Media::A4.height).abs() < 0.01);

        // And the page sits inside it, margins and all.
        assert!(geometry.x >= layout.margin - 0.01);
        assert!(geometry.y >= layout.margin - 0.01);
        assert!(geometry.x + geometry.width <= geometry.sheet.0 - layout.margin + 0.01);
        assert!(geometry.y + geometry.height <= geometry.sheet.1 - layout.margin + 0.01);
    }

    #[test]
    fn an_a4_page_is_fitted_to_a_letter_sheet() {
        let layout = Layout::default();
        let a4 = page(595.276, 841.89);
        let geometry = Geometry::frame(&layout, &a4, Media::LETTER);

        assert!((geometry.sheet.0 - Media::LETTER.width).abs() < 0.01);
        assert!((geometry.sheet.1 - Media::LETTER.height).abs() < 0.01);
        // Letter is the shorter sheet, so the page had to come down a little.
        assert!(geometry.scale() < 1.0, "scale was {}", geometry.scale());
        assert!(geometry.y + geometry.height <= geometry.sheet.1 - layout.margin + 0.01);
    }
}
