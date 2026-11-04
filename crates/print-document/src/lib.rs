//! Reads the document formats a printer must accept and
//! reduces all of them to a single internal representation.

mod error;
pub mod jpeg;
pub mod pdf;
pub mod pjl;
pub mod pwg;

pub use error::{Error, Result};

use lopdf::{Document, ObjectId};

/// A document format this printer accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// `image/pwg-raster`, REQUIRED by IPP Everywhere.
    PwgRaster,
    /// `image/jpeg`.
    Jpeg,
    /// `application/pdf`.
    Pdf,
}

impl Format {
    /// The MIME media type, as it appears in `document-format-supported`.
    pub fn mime(self) -> &'static str {
        match self {
            Format::PwgRaster => "image/pwg-raster",
            Format::Jpeg => "image/jpeg",
            Format::Pdf => "application/pdf",
        }
    }

    /// Parses a `document-format` operation attribute value. Any MIME
    /// parameters are ignored, so `image/jpeg; foo=bar` still matches.
    pub fn from_mime(mime: &str) -> Option<Self> {
        let base = mime.split(';').next().unwrap_or("").trim();
        match base.to_ascii_lowercase().as_str() {
            "image/pwg-raster" => Some(Format::PwgRaster),
            "image/jpeg" | "image/jpg" => Some(Format::Jpeg),
            "application/pdf" => Some(Format::Pdf),
            _ => None,
        }
    }

    /// Identifies a format from the first bytes of the document.
    ///
    /// Clients are allowed to send `application/octet-stream`, which means
    /// "work it out yourself", and some send the wrong type outright — so the
    /// sniffed answer is preferred over the declared one.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(pwg::SYNC_WORD) {
            Some(Format::PwgRaster)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Format::Jpeg)
        } else if bytes.starts_with(b"%PDF-") {
            Some(Format::Pdf)
        } else {
            None
        }
    }
}

/// A page size in PDF points (1/72 inch).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Media {
    pub width: f64,
    pub height: f64,
}

impl Media {
    /// ISO A4, 210x297 mm.
    pub const A4: Media = Media {
        width: 595.276,
        height: 841.890,
    };
    /// US Letter, 8.5x11 in.
    pub const LETTER: Media = Media {
        width: 612.0,
        height: 792.0,
    };

    pub const fn new(width: f64, height: f64) -> Self {
        Media { width, height }
    }

    /// The same size with the longer edge horizontal.
    pub fn landscape(self) -> Self {
        Media {
            width: self.width.max(self.height),
            height: self.width.min(self.height),
        }
    }

    /// Scales `(w, h)` to fit inside this media without distorting it,
    /// returning the fitted size. Never enlarges beyond the media.
    pub fn fit(self, w: f64, h: f64) -> (f64, f64) {
        if w <= 0.0 || h <= 0.0 {
            return (self.width, self.height);
        }
        let scale = (self.width / w).min(self.height / h);
        (w * scale, h * scale)
    }
}

impl Default for Media {
    fn default() -> Self {
        Media::A4
    }
}

/// One input page, reduced to a form that can be painted anywhere.
#[derive(Debug, Clone, Copy)]
pub struct SourcePage {
    /// A Form XObject in the owning [`SourceDocument`].
    pub form: ObjectId,
    /// The width the form should be painted at, in points.
    pub width: f64,
    /// The height the form should be painted at, in points.
    pub height: f64,
}

impl SourcePage {
    /// Whether the page is wider than it is tall.
    pub fn is_landscape(&self) -> bool {
        self.width > self.height
    }
}

/// An incoming document, normalised.
///
/// The PDF object arena holds the forms (and whatever they reference); it has
/// no page tree worth speaking of, because the caller is expected to build the
/// output pages itself from [`SourceDocument::pages`].
pub struct SourceDocument {
    doc: Document,
    pages: Vec<SourcePage>,
}

impl SourceDocument {
    pub(crate) fn new(doc: Document, pages: Vec<SourcePage>) -> Result<Self> {
        if pages.is_empty() {
            return Err(Error::Empty);
        }
        Ok(SourceDocument { doc, pages })
    }

    /// The pages, in order.
    pub fn pages(&self) -> &[SourcePage] {
        &self.pages
    }

    /// How many pages the document has.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// The arena holding the forms, for adding objects of your own.
    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.doc
    }

    /// The arena holding the forms.
    pub fn document(&self) -> &Document {
        &self.doc
    }

    /// Splits into the arena and the page list.
    pub fn into_parts(self) -> (Document, Vec<SourcePage>) {
        (self.doc, self.pages)
    }
}

/// Reads `bytes` as `format`, normalising it.
///
/// `media` is the sheet an image is fitted to; it is ignored for PDF, whose
/// pages carry their own size.
pub fn load(bytes: &[u8], format: Format, media: Media) -> Result<SourceDocument> {
    match format {
        Format::Pdf => pdf::load(bytes),
        Format::PwgRaster => pwg::load(bytes),
        Format::Jpeg => jpeg::load(bytes, media),
    }
}

/// Counts the pages in `bytes` without fully decoding it.
pub fn page_count(bytes: &[u8], format: Format) -> Result<usize> {
    match format {
        Format::Pdf => pdf::page_count(bytes),
        Format::PwgRaster => Ok(pwg::scan(bytes)?.len()),
        Format::Jpeg => Ok(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_types_round_trip() {
        for f in [Format::PwgRaster, Format::Jpeg, Format::Pdf] {
            assert_eq!(Format::from_mime(f.mime()), Some(f));
        }
    }

    #[test]
    fn mime_parameters_and_case_are_ignored() {
        assert_eq!(
            Format::from_mime("Application/PDF; version=1.7"),
            Some(Format::Pdf)
        );
        assert_eq!(Format::from_mime("application/octet-stream"), None);
        assert_eq!(Format::from_mime(""), None);
    }

    #[test]
    fn formats_are_sniffed_from_their_magic() {
        assert_eq!(Format::sniff(b"RaS2\x00"), Some(Format::PwgRaster));
        assert_eq!(Format::sniff(b"%PDF-1.7\n"), Some(Format::Pdf));
        assert_eq!(Format::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Format::Jpeg));
        assert_eq!(Format::sniff(b"not a document"), None);
        assert_eq!(Format::sniff(b""), None);
    }

    #[test]
    fn fitting_preserves_aspect_ratio_and_stays_inside() {
        let (w, h) = Media::A4.fit(1000.0, 500.0);
        assert!((w / h - 2.0).abs() < 1e-9);
        assert!(w <= Media::A4.width + 1e-9 && h <= Media::A4.height + 1e-9);
        // The limiting edge is used exactly.
        assert!((w - Media::A4.width).abs() < 1e-9);
    }

    #[test]
    fn fitting_a_degenerate_image_falls_back_to_the_sheet() {
        assert_eq!(
            Media::A4.fit(0.0, 10.0),
            (Media::A4.width, Media::A4.height)
        );
    }

    #[test]
    fn landscape_swaps_the_edges_only_when_needed() {
        let l = Media::A4.landscape();
        assert!(l.width > l.height);
        assert_eq!(l.landscape(), l);
    }
}
