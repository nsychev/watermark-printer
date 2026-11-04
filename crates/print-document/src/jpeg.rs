//! JPEG reader conformant to the IPP Everywhere specification.

use lopdf::{Dictionary, Document, Object, Stream, dictionary};

use crate::{Error, Media, Result, SourceDocument, SourcePage};

const FORMAT: &str = "JPEG";

/// What we need to know about a JPEG to place it on a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub width: u32,
    pub height: u32,
    /// 1 for grayscale, 3 for YCbCr, 4 for CMYK/YCCK.
    pub components: u8,
    /// Exif orientation, 1..=8; 1 when absent.
    pub orientation: u8,
    /// Whether an Adobe APP14 marker is present, which for a four-component
    /// file means the samples are stored inverted.
    pub adobe: bool,
    /// Progressive files are outside PDF's `/DCTDecode`, strictly speaking,
    /// though most printers cope. Reported so the caller can say so.
    pub progressive: bool,
}

impl Info {
    /// The size as it should be displayed, with the Exif orientation applied.
    pub fn display_size(&self) -> (u32, u32) {
        if self.orientation >= 5 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }
}

/// Reads the markers of a JPEG file.
pub fn probe(bytes: &[u8]) -> Result<Info> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return Err(Error::malformed(FORMAT, "missing SOI marker"));
    }

    let mut info = Info {
        width: 0,
        height: 0,
        components: 0,
        orientation: 1,
        adobe: false,
        progressive: false,
    };

    let mut pos = 2usize;
    loop {
        // Markers may be preceded by any number of 0xFF fill octets.
        while bytes.get(pos) == Some(&0xFF) && bytes.get(pos + 1) == Some(&0xFF) {
            pos += 1;
        }
        let (&0xFF, Some(&marker)) = (
            bytes
                .get(pos)
                .ok_or_else(|| Error::malformed(FORMAT, "truncated before a marker"))?,
            bytes.get(pos + 1),
        ) else {
            return Err(Error::malformed(FORMAT, "lost marker synchronisation"));
        };
        pos += 2;

        match marker {
            // Standalone markers carry no payload.
            0x01 | 0xD0..=0xD9 => {
                if marker == 0xD9 {
                    break;
                }
                continue;
            }
            // Start of scan: the entropy-coded data follows, so stop here.
            0xDA => break,
            _ => {}
        }

        let len = bytes
            .get(pos..pos + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
            .ok_or_else(|| Error::malformed(FORMAT, "truncated segment length"))?;
        if len < 2 {
            return Err(Error::malformed(FORMAT, "segment shorter than its length"));
        }
        let payload = bytes
            .get(pos + 2..pos + len)
            .ok_or_else(|| Error::malformed(FORMAT, "truncated segment"))?;
        pos += len;

        match marker {
            // SOF0..SOF15, excluding DHT (C4), JPGn (C8) and DAC (CC).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                if payload.len() < 6 {
                    return Err(Error::malformed(FORMAT, "short frame header"));
                }
                info.progressive = matches!(marker, 0xC2 | 0xC6 | 0xCA | 0xCE);
                info.height = u16::from_be_bytes([payload[1], payload[2]]) as u32;
                info.width = u16::from_be_bytes([payload[3], payload[4]]) as u32;
                info.components = payload[5];
            }
            // APP1 may hold Exif.
            0xE1 => {
                if let Some(orientation) = exif_orientation(payload) {
                    info.orientation = orientation;
                }
            }
            // APP14 "Adobe".
            0xEE if payload.starts_with(b"Adobe") => {
                info.adobe = true;
            }
            _ => {}
        }
    }

    if info.width == 0 || info.height == 0 {
        return Err(Error::malformed(FORMAT, "no frame header"));
    }
    if !matches!(info.components, 1 | 3 | 4) {
        return Err(Error::unsupported(
            FORMAT,
            format!("{} colour components", info.components),
        ));
    }
    Ok(info)
}

/// Pulls tag 0x0112 (Orientation) out of an Exif APP1 payload.
fn exif_orientation(payload: &[u8]) -> Option<u8> {
    let tiff = payload.strip_prefix(b"Exif\0\0")?;
    let big_endian = match tiff.get(..2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };

    let u16_at = |off: usize| -> Option<u16> {
        let b = tiff.get(off..off + 2)?;
        Some(if big_endian {
            u16::from_be_bytes([b[0], b[1]])
        } else {
            u16::from_le_bytes([b[0], b[1]])
        })
    };
    let u32_at = |off: usize| -> Option<u32> {
        let b = tiff.get(off..off + 4)?;
        Some(if big_endian {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        })
    };

    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;

    for i in 0..count {
        let entry = ifd + 2 + i * 12;
        if u16_at(entry)? == 0x0112 {
            // SHORT, stored in the value field itself.
            let value = u16_at(entry + 8)?;
            return (1..=8).contains(&value).then_some(value as u8);
        }
    }
    None
}

/// Reads a JPEG as a single page fitted to `media`.
pub fn load(bytes: &[u8], media: Media) -> Result<SourceDocument> {
    let info = probe(bytes)?;
    let mut doc = Document::with_version("1.7");

    let space = match info.components {
        1 => "DeviceGray",
        3 => "DeviceRGB",
        _ => "DeviceCMYK",
    };

    let mut dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => info.width as i64,
        "Height" => info.height as i64,
        "ColorSpace" => Object::Name(space.into()),
        "BitsPerComponent" => 8i64,
        "Filter" => Object::Name("DCTDecode".into()),
    };
    if info.components == 4 && info.adobe {
        // Adobe writes CMYK JPEGs with inverted samples.
        dict.set(
            "Decode",
            Object::Array(
                std::iter::repeat_n([Object::Real(1.0), Object::Real(0.0)], 4)
                    .flatten()
                    .collect(),
            ),
        );
    }

    let image = doc.add_object(Stream::new(dict, bytes.to_vec()).with_compression(false));

    let (dw, dh) = info.display_size();
    let (width, height) = media.fit(dw as f64, dh as f64);
    let form = place(&mut doc, image, width, height, info.orientation);

    SourceDocument::new(
        doc,
        vec![SourcePage {
            form,
            width,
            height,
        }],
    )
}

/// Wraps an image in a form that paints it over `width` x `height`, undoing
/// the Exif orientation as it goes.
///
/// The matrix is the orientation's transform of the unit square composed with
/// a scale into the target box, so there is no second drawing operation and
/// nothing to get out of step.
fn place(
    doc: &mut Document,
    image: lopdf::ObjectId,
    width: f64,
    height: f64,
    orientation: u8,
) -> lopdf::ObjectId {
    // (a, b, c, d, e, f) mapping the unit square onto itself.
    let (a, b, c, d, e, f) = match orientation {
        2 => (-1.0, 0.0, 0.0, 1.0, 1.0, 0.0),
        3 => (-1.0, 0.0, 0.0, -1.0, 1.0, 1.0),
        4 => (1.0, 0.0, 0.0, -1.0, 0.0, 1.0),
        5 => (0.0, 1.0, 1.0, 0.0, 0.0, 0.0),
        6 => (0.0, -1.0, 1.0, 0.0, 0.0, 1.0),
        7 => (0.0, -1.0, -1.0, 0.0, 1.0, 1.0),
        8 => (0.0, 1.0, -1.0, 0.0, 1.0, 0.0),
        _ => (1.0, 0.0, 0.0, 1.0, 0.0, 0.0),
    };

    let content = format!(
        "q {:.4} {:.4} {:.4} {:.4} {:.4} {:.4} cm /Im Do Q",
        a * width,
        b * height,
        c * width,
        d * height,
        e * width,
        f * height
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

    /// A minimal baseline JPEG: SOI, optional segments, SOF0, SOS.
    fn jpeg(segments: &[(u8, Vec<u8>)], width: u16, height: u16, components: u8) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        for (marker, payload) in segments {
            out.extend_from_slice(&[0xFF, *marker]);
            out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(payload);
        }
        let mut sof = vec![8u8];
        sof.extend_from_slice(&height.to_be_bytes());
        sof.extend_from_slice(&width.to_be_bytes());
        sof.push(components);
        out.extend_from_slice(&[0xFF, 0xC0]);
        out.extend_from_slice(&((sof.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&sof);
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    /// Writes a 16- or 32-bit value in the TIFF header's byte order.
    type Write16 = fn(u16) -> [u8; 2];
    type Write32 = fn(u32) -> [u8; 4];

    /// An Exif APP1 payload carrying one Orientation tag.
    fn exif(orientation: u16, big_endian: bool) -> Vec<u8> {
        let mut out = b"Exif\0\0".to_vec();
        let (u16b, u32b): (Write16, Write32) = if big_endian {
            (u16::to_be_bytes, u32::to_be_bytes)
        } else {
            (u16::to_le_bytes, u32::to_le_bytes)
        };
        out.extend_from_slice(if big_endian { b"MM" } else { b"II" });
        out.extend_from_slice(&u16b(42));
        out.extend_from_slice(&u32b(8)); // IFD0 at offset 8
        out.extend_from_slice(&u16b(1)); // one entry
        out.extend_from_slice(&u16b(0x0112)); // Orientation
        out.extend_from_slice(&u16b(3)); // SHORT
        out.extend_from_slice(&u32b(1)); // count
        out.extend_from_slice(&u16b(orientation));
        out.extend_from_slice(&u16b(0)); // padding of the value field
        out
    }

    #[test]
    fn a_baseline_file_reports_its_frame() {
        let info = probe(&jpeg(&[], 640, 480, 3)).unwrap();
        assert_eq!(
            info,
            Info {
                width: 640,
                height: 480,
                components: 3,
                orientation: 1,
                adobe: false,
                progressive: false,
            }
        );
        assert_eq!(info.display_size(), (640, 480));
    }

    #[test]
    fn exif_orientation_is_read_in_either_byte_order() {
        for big_endian in [true, false] {
            let bytes = jpeg(&[(0xE1, exif(6, big_endian))], 640, 480, 3);
            let info = probe(&bytes).unwrap();
            assert_eq!(info.orientation, 6);
            // A quarter turn swaps the displayed edges.
            assert_eq!(info.display_size(), (480, 640));
        }
    }

    #[test]
    fn a_nonsensical_orientation_is_ignored() {
        let bytes = jpeg(&[(0xE1, exif(99, true))], 8, 8, 1);
        assert_eq!(probe(&bytes).unwrap().orientation, 1);
        // So is an APP1 that is not Exif at all.
        let bytes = jpeg(
            &[(0xE1, b"http://ns.adobe.com/xap/1.0/\0".to_vec())],
            8,
            8,
            1,
        );
        assert_eq!(probe(&bytes).unwrap().orientation, 1);
    }

    #[test]
    fn adobe_and_progressive_files_are_flagged() {
        let bytes = jpeg(&[(0xEE, b"Adobe\0\0\0\0\0\0\0".to_vec())], 8, 8, 4);
        let info = probe(&bytes).unwrap();
        assert!(info.adobe);
        assert_eq!(info.components, 4);

        // SOF2 is progressive.
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC2, 0x00, 0x0B, 0x08];
        bytes.extend_from_slice(&8u16.to_be_bytes());
        bytes.extend_from_slice(&8u16.to_be_bytes());
        bytes.extend_from_slice(&[3, 0, 0, 0]);
        bytes.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        assert!(probe(&bytes).unwrap().progressive);
    }

    #[test]
    fn broken_files_are_refused() {
        assert!(probe(b"").is_err());
        assert!(probe(b"\xFF\xD8").is_err());
        // SOI then straight to SOS: no frame header.
        assert!(probe(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02]).is_err());
        // Two components is neither gray, RGB nor CMYK.
        assert!(probe(&jpeg(&[], 8, 8, 2)).is_err());
    }

    #[test]
    fn a_file_becomes_one_page_fitted_to_the_sheet() {
        let doc = load(&jpeg(&[], 1200, 600, 3), Media::A4).unwrap();
        assert_eq!(doc.page_count(), 1);
        let page = doc.pages()[0];
        // Twice as wide as tall, and as wide as A4 allows.
        assert!((page.width / page.height - 2.0).abs() < 1e-6);
        assert!((page.width - Media::A4.width).abs() < 1e-6);
    }

    #[test]
    fn a_quarter_turned_photo_is_fitted_by_its_displayed_size() {
        let bytes = jpeg(&[(0xE1, exif(6, true))], 1200, 600, 3);
        let doc = load(&bytes, Media::A4).unwrap();
        let page = doc.pages()[0];
        // Displayed 600x1200, so now taller than wide.
        assert!(page.height > page.width);
        assert!((page.height - Media::A4.height).abs() < 1e-6);
    }

    #[test]
    fn the_orientation_matrix_rotates_rather_than_just_resizing() {
        let mut doc = Document::with_version("1.7");
        let image = doc.add_object(Stream::new(Dictionary::new(), Vec::new()));

        let upright = place(&mut doc, image, 100.0, 200.0, 1);
        // Orientation 6 maps the stored left edge onto the top edge.
        let turned = place(&mut doc, image, 100.0, 200.0, 6);

        let content = |id| {
            String::from_utf8_lossy(&doc.get_object(id).unwrap().as_stream().unwrap().content)
                .into_owned()
        };
        let upright = content(upright);
        let turned = content(turned);

        assert!(
            upright.contains("100.0000 0.0000 0.0000 200.0000 0.0000 0.0000 cm"),
            "{upright}"
        );
        assert!(
            turned.contains("0.0000 -200.0000 100.0000 0.0000 0.0000 200.0000 cm"),
            "{turned}"
        );
    }
}
