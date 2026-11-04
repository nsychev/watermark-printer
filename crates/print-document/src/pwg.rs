//! PWG Raster Format [PWG 5102.4] reader.
//!
//! This is the one document format IPP Everywhere *requires*, and the reason
//! this crate exists: advertising it is what lets CUPS and Windows drive the
//! printer with no driver and no PDF round trip.
//!
//! A file is a four-byte synchronisation word followed by pages, each of which
//! is a fixed 1796-octet header and a run-length encoded bitmap. The encoding
//! is PackBits-like and works on whole *pixels*, except at 1 bit per pixel
//! where the unit is a whole octet of eight pixels:
//!
//! ```text
//! line := <repeat-1>            one octet, the line is emitted repeat times
//!         run*                  until BytesPerLine octets have been produced
//! run  := <count-1> <unit>      count in 1..=128, one unit repeated
//!       | <257-count> <unit>*   count in 2..=128, count distinct units
//! ```
//!
//! [`scan`] walks that structure without materialising pixels, which is how a
//! page count is obtained cheaply; [`decode`] materialises one page.

use std::io::Write as _;

use flate2::{Compression, write::ZlibEncoder};
use lopdf::{Dictionary, Document, Object, Stream, dictionary};

use crate::{Error, Result, SourceDocument, SourcePage};

const FORMAT: &str = "PWG Raster";

/// The four octets every PWG Raster file starts with.
pub const SYNC_WORD: &[u8; 4] = b"RaS2";

/// Every page header is exactly this long.
pub const PAGE_HEADER_LEN: usize = 1796;

// Field offsets we actually read, from Table 1 of PWG 5102.4.
const OFF_HW_RESOLUTION: usize = 276;
const OFF_PAGE_SIZE: usize = 352;
const OFF_WIDTH: usize = 372;
const OFF_HEIGHT: usize = 376;
const OFF_BITS_PER_COLOR: usize = 384;
const OFF_BITS_PER_PIXEL: usize = 388;
const OFF_BYTES_PER_LINE: usize = 392;
const OFF_COLOR_SPACE: usize = 400;
const OFF_NUM_COLORS: usize = 420;
const OFF_TOTAL_PAGE_COUNT: usize = 452;
const OFF_CROSS_FEED_TRANSFORM: usize = 456;
const OFF_FEED_TRANSFORM: usize = 460;
const OFF_PAGE_SIZE_NAME: usize = 1732;

/// A sheet larger than this in either direction is refused outright, as is a
/// bitmap whose decoded size would exceed [`MAX_BITMAP_BYTES`]. Both guard
/// against a header that claims an absurd size.
const MAX_DIMENSION: u32 = 200_000;
/// 512 MiB: comfortably more than A3 at 1200dpi in CMYK, far less than a
/// machine's memory.
pub const MAX_BITMAP_BYTES: usize = 512 * 1024 * 1024;

/// How a decoded bitmap maps onto a PDF colour space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorModel {
    /// `/DeviceGray`, `/DeviceRGB` or `/DeviceCMYK`.
    pub space: &'static str,
    /// Colour components per pixel.
    pub components: u32,
    /// Bits per component: 1, 8 or 16.
    pub bits: u32,
    /// Whether 0 means "full ink" rather than "no ink", which is how the
    /// `Black` and `DeviceN` colour spaces are defined and the opposite of
    /// PDF's `/DeviceGray`.
    pub inverted: bool,
}

/// The parts of a page header this crate uses.
#[derive(Debug, Clone)]
pub struct PageHeader {
    pub width: u32,
    pub height: u32,
    pub bits_per_color: u32,
    pub bits_per_pixel: u32,
    pub bytes_per_line: u32,
    pub color_space: u32,
    pub num_colors: u32,
    /// Horizontal and vertical resolution, in dpi.
    pub resolution: (u32, u32),
    /// Sheet size in PDF points, as the producer computed it. Zero means
    /// "derive it from the pixel size and the resolution".
    pub page_size: (u32, u32),
    pub total_page_count: u32,
    /// `-1` on a back side that the Client has already mirrored across the
    /// feed direction.
    pub cross_feed_transform: i32,
    /// `-1` on a back side that the Client has already mirrored along the
    /// feed direction.
    pub feed_transform: i32,
    /// The self-describing media name, e.g. `iso_a4_210x297mm`.
    pub page_size_name: String,
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn i32_at(buf: &[u8], off: usize) -> i32 {
    u32_at(buf, off) as i32
}

/// Reads a NUL-terminated 64-octet ASCII field.
fn cstring_at(buf: &[u8], off: usize) -> String {
    let field = &buf[off..off + 64];
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

impl PageHeader {
    /// Parses a page header from exactly [`PAGE_HEADER_LEN`] octets.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < PAGE_HEADER_LEN {
            return Err(Error::malformed(FORMAT, "truncated page header"));
        }

        let header = PageHeader {
            width: u32_at(buf, OFF_WIDTH),
            height: u32_at(buf, OFF_HEIGHT),
            bits_per_color: u32_at(buf, OFF_BITS_PER_COLOR),
            bits_per_pixel: u32_at(buf, OFF_BITS_PER_PIXEL),
            bytes_per_line: u32_at(buf, OFF_BYTES_PER_LINE),
            color_space: u32_at(buf, OFF_COLOR_SPACE),
            num_colors: u32_at(buf, OFF_NUM_COLORS),
            resolution: (
                u32_at(buf, OFF_HW_RESOLUTION),
                u32_at(buf, OFF_HW_RESOLUTION + 4),
            ),
            page_size: (u32_at(buf, OFF_PAGE_SIZE), u32_at(buf, OFF_PAGE_SIZE + 4)),
            total_page_count: u32_at(buf, OFF_TOTAL_PAGE_COUNT),
            cross_feed_transform: i32_at(buf, OFF_CROSS_FEED_TRANSFORM),
            feed_transform: i32_at(buf, OFF_FEED_TRANSFORM),
            page_size_name: cstring_at(buf, OFF_PAGE_SIZE_NAME),
        };

        header.validate()?;
        Ok(header)
    }

    fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::malformed(FORMAT, "page has a zero dimension"));
        }
        if self.width > MAX_DIMENSION || self.height > MAX_DIMENSION {
            return Err(Error::unsupported(
                FORMAT,
                format!("page is {}x{} pixels", self.width, self.height),
            ));
        }
        if self.bits_per_pixel == 0 || self.bits_per_color == 0 {
            return Err(Error::malformed(FORMAT, "page has a zero bit depth"));
        }
        // The encoding gives no way to resynchronise, so a stride that cannot
        // hold the line would desynchronise every following page.
        let minimum = (self.width as u64 * self.bits_per_pixel as u64).div_ceil(8);
        if (self.bytes_per_line as u64) < minimum {
            return Err(Error::malformed(
                FORMAT,
                format!(
                    "BytesPerLine is {} but {} octets are needed for {} pixels",
                    self.bytes_per_line, minimum, self.width
                ),
            ));
        }
        if self.bytes_per_line as u64 * self.height as u64 > MAX_BITMAP_BYTES as u64 {
            return Err(Error::unsupported(
                FORMAT,
                format!(
                    "bitmap would be {} octets",
                    self.bytes_per_line as u64 * self.height as u64
                ),
            ));
        }
        self.color_model()?;
        Ok(())
    }

    /// The unit the run-length encoding repeats: one pixel, or one octet of
    /// eight pixels when the depth is below 8 bits.
    pub fn unit_len(&self) -> usize {
        if self.bits_per_pixel < 8 {
            1
        } else {
            (self.bits_per_pixel / 8) as usize
        }
    }

    /// How this page's colour space maps onto PDF.
    ///
    /// The mapping follows Table 12 of PWG 5102.4. Device colour spaces with
    /// an unusual number of colorants are refused rather than guessed at:
    /// printing them wrong is worse than refusing them.
    pub fn color_model(&self) -> Result<ColorModel> {
        let bits = self.bits_per_color;
        if !matches!(bits, 1 | 8 | 16) {
            return Err(Error::unsupported(
                FORMAT,
                format!("{bits} bits per colour"),
            ));
        }

        // (space, components, inverted)
        let (space, components, inverted) = match self.color_space {
            // Black, and single-colorant device colour: 0 is no ink.
            3 | 48 => ("DeviceGray", 1, true),
            // sRGB grayscale: 0 is black, as in PDF.
            18 => ("DeviceGray", 1, false),
            // Device RGB, sRGB and Adobe RGB all become /DeviceRGB. Treating
            // Adobe RGB as sRGB shifts saturated colours slightly; for a
            // watermarking proxy that is well inside "good enough", and the
            // alternative is embedding an ICC profile per page.
            1 | 19 | 20 => ("DeviceRGB", 3, false),
            // CMYK, and four-colorant device colour.
            6 | 51 => ("DeviceCMYK", 4, false),
            other => {
                return Err(Error::unsupported(FORMAT, format!("ColorSpace {other}")));
            }
        };

        if self.num_colors != 0 && self.num_colors != components {
            return Err(Error::malformed(
                FORMAT,
                format!(
                    "ColorSpace {} implies {} colours but NumColors is {}",
                    self.color_space, components, self.num_colors
                ),
            ));
        }
        if self.bits_per_pixel != bits * components {
            return Err(Error::malformed(
                FORMAT,
                format!(
                    "BitsPerPixel is {} but {} colours at {} bits need {}",
                    self.bits_per_pixel,
                    components,
                    bits,
                    bits * components
                ),
            ));
        }

        Ok(ColorModel {
            space,
            components,
            bits,
            inverted,
        })
    }

    /// The sheet size in PDF points.
    ///
    /// The header's own `PageSize` is preferred, because the producer rounded
    /// the pixel dimensions to it and recomputing from pixels would drift.
    pub fn size_in_points(&self) -> (f64, f64) {
        if self.page_size.0 > 0 && self.page_size.1 > 0 {
            return (self.page_size.0 as f64, self.page_size.1 as f64);
        }
        let (xdpi, ydpi) = self.resolution;
        let xdpi = if xdpi == 0 { 300 } else { xdpi };
        let ydpi = if ydpi == 0 { 300 } else { ydpi };
        (
            self.width as f64 * 72.0 / xdpi as f64,
            self.height as f64 * 72.0 / ydpi as f64,
        )
    }

    /// The row stride PDF expects, which — unlike `BytesPerLine` — carries no
    /// padding beyond the byte boundary.
    fn pdf_stride(&self) -> usize {
        (self.width as usize * self.bits_per_pixel as usize).div_ceil(8)
    }
}

/// One page located within a raster file.
#[derive(Debug, Clone)]
pub struct PageSpan {
    pub header: PageHeader,
    /// Where the encoded bitmap starts.
    pub offset: usize,
    /// How long the encoded bitmap is.
    pub len: usize,
}

/// Locates every page in a raster file without decoding any pixels.
pub fn scan(bytes: &[u8]) -> Result<Vec<PageSpan>> {
    if !bytes.starts_with(SYNC_WORD) {
        return Err(Error::malformed(
            FORMAT,
            "missing RaS2 synchronisation word",
        ));
    }

    let mut pages = Vec::new();
    let mut pos = SYNC_WORD.len();

    while pos < bytes.len() {
        // Trailing padding shorter than a header means the file is done; a
        // partial header means it was truncated.
        let remaining = bytes.len() - pos;
        if remaining < PAGE_HEADER_LEN {
            if bytes[pos..].iter().all(|&b| b == 0) {
                break;
            }
            return Err(Error::malformed(
                FORMAT,
                format!("{remaining} trailing octets are not a page header"),
            ));
        }

        let header = PageHeader::parse(&bytes[pos..pos + PAGE_HEADER_LEN])?;
        let data_start = pos + PAGE_HEADER_LEN;
        let len = walk(&header, &bytes[data_start..], None)?;

        pages.push(PageSpan {
            header,
            offset: data_start,
            len,
        });
        pos = data_start + len;
    }

    if pages.is_empty() {
        return Err(Error::Empty);
    }
    Ok(pages)
}

/// Walks one page's encoded bitmap, returning how many octets it occupied.
///
/// With `out` given, the decoded rows are appended to it at PDF's row stride;
/// with `out` `None` nothing is written, which is what makes [`scan`] cheap.
fn walk(header: &PageHeader, data: &[u8], mut out: Option<&mut Vec<u8>>) -> Result<usize> {
    let unit = header.unit_len();
    let stride = header.bytes_per_line as usize;
    let pdf_stride = header.pdf_stride();

    let mut line = vec![0u8; stride];
    let mut pos = 0usize;
    let mut rows_done = 0u32;

    let truncated = || Error::malformed(FORMAT, "bitmap ends mid-line");

    while rows_done < header.height {
        let repeat = *data.get(pos).ok_or_else(truncated)? as u32 + 1;
        pos += 1;

        let mut filled = 0usize;
        while filled < stride {
            let marker = *data.get(pos).ok_or_else(truncated)? as usize;
            pos += 1;

            // 0..=127 repeats one unit; 128..=255 copies 257-marker units.
            let (count, distinct) = if marker <= 127 {
                (marker + 1, false)
            } else {
                (257 - marker, true)
            };

            let source_len = if distinct { count * unit } else { unit };
            let source = data
                .get(pos..pos + source_len)
                .ok_or_else(truncated)?
                .to_vec();
            pos += source_len;

            let produced = count * unit;
            if filled + produced > stride {
                return Err(Error::malformed(
                    FORMAT,
                    format!("run overruns the {stride}-octet line"),
                ));
            }

            if distinct {
                line[filled..filled + produced].copy_from_slice(&source);
            } else {
                for chunk in line[filled..filled + produced].chunks_mut(unit) {
                    chunk.copy_from_slice(&source);
                }
            }
            filled += produced;
        }

        if let Some(out) = out.as_deref_mut() {
            // A run of repeated lines may claim to go past the last row; the
            // header's height wins.
            let emit = repeat.min(header.height - rows_done);
            for _ in 0..emit {
                out.extend_from_slice(&line[..pdf_stride]);
            }
        }
        rows_done = rows_done.saturating_add(repeat);
    }

    Ok(pos)
}

/// A decoded page bitmap, laid out as PDF wants it.
#[derive(Debug, Clone)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub color: ColorModel,
    /// Rows, top to bottom, each `ceil(width * bits_per_pixel / 8)` long.
    pub data: Vec<u8>,
}

/// Decodes one page's bitmap.
pub fn decode(span: &PageSpan, bytes: &[u8]) -> Result<Bitmap> {
    let data = bytes
        .get(span.offset..span.offset + span.len)
        .ok_or_else(|| Error::malformed(FORMAT, "page data outside the file"))?;

    let header = &span.header;
    let mut out = Vec::with_capacity(header.pdf_stride() * header.height as usize);
    walk(header, data, Some(&mut out))?;

    Ok(Bitmap {
        width: header.width,
        height: header.height,
        color: header.color_model()?,
        data: out,
    })
}

/// Reads a whole raster file.
pub fn load(bytes: &[u8]) -> Result<SourceDocument> {
    let spans = scan(bytes)?;
    let mut doc = Document::with_version("1.7");
    let mut pages = Vec::with_capacity(spans.len());

    for span in &spans {
        let bitmap = decode(span, bytes)?;
        let (width, height) = span.header.size_in_points();
        let image = add_image(&mut doc, &bitmap);
        let form = wrap_image(
            &mut doc,
            image,
            width,
            height,
            span.header.cross_feed_transform,
            span.header.feed_transform,
        );
        pages.push(SourcePage {
            form,
            width,
            height,
        });
    }

    SourceDocument::new(doc, pages)
}

/// Adds a bitmap as a Flate-compressed image XObject.
pub(crate) fn add_image(doc: &mut Document, bitmap: &Bitmap) -> lopdf::ObjectId {
    let mut dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => bitmap.width as i64,
        "Height" => bitmap.height as i64,
        "ColorSpace" => Object::Name(bitmap.color.space.into()),
        "BitsPerComponent" => bitmap.color.bits as i64,
        "Filter" => Object::Name("FlateDecode".into()),
    };

    if bitmap.color.inverted {
        // /Decode flips the sample range, which turns "amount of ink" into
        // PDF's "amount of light" without touching a single pixel.
        let max = Object::Real(1.0);
        let min = Object::Real(0.0);
        let pair = vec![max, min];
        dict.set(
            "Decode",
            Object::Array(
                std::iter::repeat_n(pair, bitmap.color.components as usize)
                    .flatten()
                    .collect(),
            ),
        );
    }

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    // Writing to a Vec cannot fail, and neither can finishing it.
    encoder
        .write_all(&bitmap.data)
        .and_then(|()| encoder.finish())
        .map(|compressed| doc.add_object(Stream::new(dict, compressed).with_compression(false)))
        .expect("in-memory deflate cannot fail")
}

/// Wraps an image XObject in a form that paints it over `width` x `height`.
///
/// `cross_feed` and `feed` are the page header's transform fields: `-1` means
/// the Client already mirrored the image for a back side, so we mirror it back
/// as we place it.
pub(crate) fn wrap_image(
    doc: &mut Document,
    image: lopdf::ObjectId,
    width: f64,
    height: f64,
    cross_feed: i32,
    feed: i32,
) -> lopdf::ObjectId {
    let sx = if cross_feed < 0 { -1.0 } else { 1.0 };
    let sy = if feed < 0 { -1.0 } else { 1.0 };
    let tx = if sx < 0.0 { width } else { 0.0 };
    let ty = if sy < 0.0 { height } else { 0.0 };

    let content = format!(
        "q {:.4} 0 0 {:.4} {:.4} {:.4} cm /Im Do Q",
        sx * width,
        sy * height,
        tx,
        ty
    );

    let mut xobjects = Dictionary::new();
    xobjects.set("Im", Object::Reference(image));

    let dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Form",
        "FormType" => 1i64,
        "BBox" => vec![0.into(), 0.into(), Object::Real(width as f32), Object::Real(height as f32)],
        "Resources" => dictionary! { "XObject" => xobjects },
    };

    doc.add_object(Stream::new(dict, content.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a one-page raster file around an already-encoded bitmap.
    fn file(fields: &[(usize, u32)], name: &str, encoded: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(SYNC_WORD);
        let mut header = vec![0u8; PAGE_HEADER_LEN];
        header[..9].copy_from_slice(b"PwgRaster");
        for &(off, value) in fields {
            header[off..off + 4].copy_from_slice(&value.to_be_bytes());
        }
        header[OFF_PAGE_SIZE_NAME..OFF_PAGE_SIZE_NAME + name.len()]
            .copy_from_slice(name.as_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(encoded);
        out
    }

    /// The 23x8 1-bit grayscale sample from section 4.4.1 of PWG 5102.4.
    fn gray_sample() -> Vec<u8> {
        file(
            &[
                (OFF_WIDTH, 23),
                (OFF_HEIGHT, 8),
                (OFF_BITS_PER_COLOR, 1),
                (OFF_BITS_PER_PIXEL, 1),
                (OFF_BYTES_PER_LINE, 3),
                (OFF_COLOR_SPACE, 18), // Sgray
                (OFF_NUM_COLORS, 1),
                (OFF_HW_RESOLUTION, 300),
                (OFF_HW_RESOLUTION + 4, 300),
                (OFF_CROSS_FEED_TRANSFORM, 1),
                (OFF_FEED_TRANSFORM, 1),
            ],
            "custom_max_23x8px",
            &[
                0x00, 0xFE, 0x8F, 0x78, 0xF7, //
                0x00, 0xFE, 0x76, 0x77, 0x67, //
                0x03, 0x02, 0x77, //
                0x00, 0xFE, 0x8E, 0x38, 0xE3, //
                0x00, 0x02, 0xFF,
            ],
        )
    }

    /// The 8x8 24-bit sRGB sample from section 4.4.2 of PWG 5102.4.
    fn rgb_sample() -> Vec<u8> {
        file(
            &[
                (OFF_WIDTH, 8),
                (OFF_HEIGHT, 8),
                (OFF_BITS_PER_COLOR, 8),
                (OFF_BITS_PER_PIXEL, 24),
                (OFF_BYTES_PER_LINE, 24),
                (OFF_COLOR_SPACE, 19), // Srgb
                (OFF_NUM_COLORS, 3),
                (OFF_HW_RESOLUTION, 72),
                (OFF_HW_RESOLUTION + 4, 72),
            ],
            "custom_max_8x8px",
            &[
                0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x02, 0xFF, 0xFF, 0x00, 0x03, 0xFF, 0xFF,
                0xFF, //
                0x00, 0xFE, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x00, 0x02, 0xFF, 0xFF,
                0xFF, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0xFF, 0xFF, //
                0x00, 0x01, 0xFF, 0xFF, 0x00, 0x02, 0xFF, 0xFF, 0xFF, 0x02, 0x00, 0xFF,
                0x00, //
                0x00, 0x02, 0xFF, 0xFF, 0x00, 0x02, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0xFF, 0x00, 0xFF,
                0xFF, 0xFF, //
                0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x02, 0xFF, 0xFF, 0x00, 0x03, 0xFF, 0xFF,
                0xFF, //
                0x00, 0x07, 0xFF, 0xFF, 0xFF, //
                0x01, 0x07, 0xFF, 0x00, 0x00,
            ],
        )
    }

    #[test]
    fn the_spec_gray_sample_decodes_to_its_figure() {
        let bytes = gray_sample();
        let spans = scan(&bytes).unwrap();
        assert_eq!(spans.len(), 1);
        // 21 octets, as the spec says.
        assert_eq!(spans[0].len, 21);

        let bitmap = decode(&spans[0], &bytes).unwrap();
        assert_eq!((bitmap.width, bitmap.height), (23, 8));
        assert_eq!(bitmap.data.len(), 3 * 8);
        assert_eq!(
            bitmap.data,
            vec![
                0x8F, 0x78, 0xF7, // line 1
                0x76, 0x77, 0x67, // line 2
                0x77, 0x77, 0x77, // lines 3..6 are one repeated line
                0x77, 0x77, 0x77, //
                0x77, 0x77, 0x77, //
                0x77, 0x77, 0x77, //
                0x8E, 0x38, 0xE3, // line 7
                0xFF, 0xFF, 0xFF, // line 8
            ]
        );
    }

    #[test]
    fn the_spec_rgb_sample_decodes_to_its_figure() {
        let bytes = rgb_sample();
        let spans = scan(&bytes).unwrap();
        // 87 octets, as the spec says.
        assert_eq!(spans[0].len, 87);

        let bitmap = decode(&spans[0], &bytes).unwrap();
        assert_eq!(bitmap.data.len(), 24 * 8);
        assert_eq!(bitmap.color.space, "DeviceRGB");

        let row = |n: usize| &bitmap.data[n * 24..(n + 1) * 24];
        // One white pixel, three yellow, four white.
        assert_eq!(&row(0)[0..3], &[0xFF, 0xFF, 0xFF]);
        assert_eq!(&row(0)[3..6], &[0xFF, 0xFF, 0x00]);
        assert_eq!(&row(0)[15..18], &[0xFF, 0xFF, 0xFF]);
        // A yellow + blue + yellow literal run.
        assert_eq!(&row(1)[3..6], &[0x00, 0x00, 0xFF]);
        // The sixth line is all white, and the last line repeats twice in red.
        assert_eq!(row(5), [0xFF; 24]);
        assert_eq!(&row(6)[0..3], &[0xFF, 0x00, 0x00]);
        assert_eq!(&row(7)[0..3], &[0xFF, 0x00, 0x00]);
    }

    #[test]
    fn a_page_count_needs_no_pixels() {
        let mut bytes = gray_sample();
        // Append a second page by repeating header + data.
        let one = bytes[SYNC_WORD.len()..].to_vec();
        bytes.extend_from_slice(&one);
        assert_eq!(scan(&bytes).unwrap().len(), 2);
    }

    #[test]
    fn sizes_come_from_the_header_then_the_resolution() {
        let bytes = file(
            &[
                (OFF_WIDTH, 2480),
                (OFF_HEIGHT, 3508),
                (OFF_BITS_PER_COLOR, 8),
                (OFF_BITS_PER_PIXEL, 8),
                (OFF_BYTES_PER_LINE, 2480),
                (OFF_COLOR_SPACE, 18),
                (OFF_NUM_COLORS, 1),
                (OFF_HW_RESOLUTION, 300),
                (OFF_HW_RESOLUTION + 4, 300),
            ],
            "iso_a4_210x297mm",
            &[],
        );
        let header = PageHeader::parse(&bytes[4..]).unwrap();
        // No PageSize field, so it is derived: 2480 px at 300 dpi.
        let (w, h) = header.size_in_points();
        assert!((w - 595.2).abs() < 0.1, "{w}");
        assert!((h - 841.92).abs() < 0.1, "{h}");
        assert_eq!(header.page_size_name, "iso_a4_210x297mm");

        // With PageSize set, it is used verbatim.
        let mut with_size = bytes.clone();
        with_size[4 + OFF_PAGE_SIZE..4 + OFF_PAGE_SIZE + 4].copy_from_slice(&595u32.to_be_bytes());
        with_size[4 + OFF_PAGE_SIZE + 4..4 + OFF_PAGE_SIZE + 8]
            .copy_from_slice(&842u32.to_be_bytes());
        let header = PageHeader::parse(&with_size[4..]).unwrap();
        assert_eq!(header.size_in_points(), (595.0, 842.0));
    }

    #[test]
    fn ink_colour_spaces_are_marked_inverted() {
        let black = PageHeader {
            color_space: 3,
            bits_per_color: 1,
            bits_per_pixel: 1,
            num_colors: 1,
            ..PageHeader::parse(&gray_sample()[4..]).unwrap()
        };
        assert!(black.color_model().unwrap().inverted);
        // sRGB grayscale is already PDF's way round.
        let gray = PageHeader::parse(&gray_sample()[4..]).unwrap();
        assert!(!gray.color_model().unwrap().inverted);
    }

    #[test]
    fn cmyk_and_sixteen_bit_pages_are_accepted() {
        for (space, bits, bpp, colors, expected) in [
            (6u32, 8u32, 32u32, 4u32, "DeviceCMYK"),
            (19, 16, 48, 3, "DeviceRGB"),
            (18, 16, 16, 1, "DeviceGray"),
        ] {
            let header = PageHeader {
                color_space: space,
                bits_per_color: bits,
                bits_per_pixel: bpp,
                num_colors: colors,
                ..PageHeader::parse(&gray_sample()[4..]).unwrap()
            };
            assert_eq!(header.color_model().unwrap().space, expected);
        }
    }

    #[test]
    fn inconsistent_and_exotic_headers_are_refused() {
        let base = PageHeader::parse(&gray_sample()[4..]).unwrap();

        // A colour space we will not guess at.
        let header = PageHeader {
            color_space: 52,
            bits_per_color: 8,
            bits_per_pixel: 40,
            num_colors: 5,
            ..base.clone()
        };
        assert!(header.color_model().is_err());

        // BitsPerPixel that does not match the colour count.
        let header = PageHeader {
            color_space: 19,
            bits_per_color: 8,
            bits_per_pixel: 8,
            num_colors: 3,
            ..base.clone()
        };
        assert!(header.color_model().is_err());

        // A depth PDF has no equivalent for.
        let header = PageHeader {
            bits_per_color: 4,
            bits_per_pixel: 4,
            ..base
        };
        assert!(header.color_model().is_err());
    }

    #[test]
    fn a_short_line_stride_is_refused_rather_than_desynchronising() {
        let bytes = file(
            &[
                (OFF_WIDTH, 100),
                (OFF_HEIGHT, 1),
                (OFF_BITS_PER_COLOR, 8),
                (OFF_BITS_PER_PIXEL, 8),
                (OFF_BYTES_PER_LINE, 10), // needs 100
                (OFF_COLOR_SPACE, 18),
                (OFF_NUM_COLORS, 1),
            ],
            "custom_max_100x1px",
            &[],
        );
        let err = scan(&bytes).unwrap_err();
        assert!(err.to_string().contains("BytesPerLine"), "{err}");
    }

    #[test]
    fn a_missing_sync_word_or_truncated_data_is_refused() {
        assert!(scan(b"nope").is_err());
        let bytes = gray_sample();
        // Chop the last run off.
        let err = scan(&bytes[..bytes.len() - 2]).unwrap_err();
        assert!(err.is_client_fault(), "{err}");
    }

    #[test]
    fn trailing_zero_padding_ends_the_file_quietly() {
        let mut bytes = gray_sample();
        bytes.extend_from_slice(&[0u8; 16]);
        assert_eq!(scan(&bytes).unwrap().len(), 1);
    }

    #[test]
    fn a_whole_file_becomes_one_form_per_page() {
        let bytes = rgb_sample();
        let doc = load(&bytes).unwrap();
        assert_eq!(doc.page_count(), 1);
        let page = doc.pages()[0];
        // 8 px at 72 dpi is 8 pt.
        assert!((page.width - 8.0).abs() < 1e-6);
        assert!((page.height - 8.0).abs() < 1e-6);
        // The form exists and paints the image.
        let form = doc.document().get_object(page.form).unwrap();
        let stream = form.as_stream().unwrap();
        let content = String::from_utf8_lossy(&stream.content);
        assert!(content.contains("/Im Do"), "{content}");
    }

    #[test]
    fn a_mirrored_back_side_is_mirrored_back() {
        let mut doc = Document::with_version("1.7");
        let image = doc.add_object(Stream::new(Dictionary::new(), Vec::new()));
        let form = wrap_image(&mut doc, image, 100.0, 200.0, -1, 1);
        let stream = doc.get_object(form).unwrap().as_stream().unwrap();
        let content = String::from_utf8_lossy(&stream.content);
        // Negative horizontal scale, shifted right by the page width.
        assert!(
            content.contains("-100.0000 0 0 200.0000 100.0000 0.0000 cm"),
            "{content}"
        );
    }
}
