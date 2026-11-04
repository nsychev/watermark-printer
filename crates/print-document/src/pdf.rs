//! PDF reader.
//!
//! Each incoming page is turned into a Form XObject holding that page's
//! content and resources, with a `/Matrix` that undoes the page's `/Rotate`
//! and its media box origin. Two useful things follow.

use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::{Error, Result, SourceDocument, SourcePage};

const FORMAT: &str = "PDF";

/// How deep a `/Parent` chain may be before we call it a loop.
const MAX_INHERITANCE_DEPTH: usize = 64;

/// Counts the pages of a PDF.
pub fn page_count(bytes: &[u8]) -> Result<usize> {
    let doc = open(bytes)?;
    Ok(doc.get_pages().len())
}

fn open(bytes: &[u8]) -> Result<Document> {
    let mut doc = Document::load_mem(bytes)?;
    if doc.trailer.get(b"Encrypt").is_ok() {
        // An empty user password covers the common "owner password only" case,
        // which is permissions metadata rather than real protection.
        doc.decrypt("")
            .map_err(|_| Error::unsupported(FORMAT, "the document is password protected"))?;
    }
    Ok(doc)
}

/// Reads a PDF, turning every page into a form.
pub fn load(bytes: &[u8]) -> Result<SourceDocument> {
    let mut doc = open(bytes)?;
    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    if page_ids.is_empty() {
        return Err(Error::Empty);
    }

    let mut pages = Vec::with_capacity(page_ids.len());
    for page_id in page_ids {
        pages.push(page_to_form(&mut doc, page_id)?);
    }

    SourceDocument::new(doc, pages)
}

/// Looks up an attribute on a page, walking `/Parent` for inheritable ones.
fn inherited(doc: &Document, page_id: ObjectId, key: &[u8]) -> Option<Object> {
    let mut node = page_id;
    for _ in 0..MAX_INHERITANCE_DEPTH {
        let dict = doc.get_dictionary(node).ok()?;
        if let Ok(value) = dict.get(key) {
            // The value may itself be a reference.
            return match value {
                Object::Reference(id) => doc.get_object(*id).ok().cloned(),
                other => Some(other.clone()),
            };
        }
        node = dict.get(b"Parent").and_then(Object::as_reference).ok()?;
    }
    None
}

/// The page's media box as `[x0, y0, x1, y1]`, normalised so that
/// `x0 < x1` and `y0 < y1`.
fn media_box(doc: &Document, page_id: ObjectId) -> [f64; 4] {
    let numbers = inherited(doc, page_id, b"MediaBox")
        .as_ref()
        .and_then(|o| o.as_array().ok())
        .map(|array| {
            array
                .iter()
                .map(|v| match v {
                    Object::Integer(i) => *i as f64,
                    Object::Real(r) => *r as f64,
                    Object::Reference(id) => doc
                        .get_object(*id)
                        .ok()
                        .and_then(|o| match o {
                            Object::Integer(i) => Some(*i as f64),
                            Object::Real(r) => Some(*r as f64),
                            _ => None,
                        })
                        .unwrap_or(0.0),
                    _ => 0.0,
                })
                .collect::<Vec<f64>>()
        })
        .filter(|v| v.len() == 4);

    let Some(v) = numbers else {
        // A page with no media box is Letter by PDF's own default.
        return [0.0, 0.0, 612.0, 792.0];
    };

    let (x0, x1) = (v[0].min(v[2]), v[0].max(v[2]));
    let (y0, y1) = (v[1].min(v[3]), v[1].max(v[3]));

    // A degenerate box would make the page invisible; fall back instead.
    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        return [0.0, 0.0, 612.0, 792.0];
    }
    [x0, y0, x1, y1]
}

/// The page's `/Rotate`, reduced to one of 0, 90, 180, 270.
fn rotation(doc: &Document, page_id: ObjectId) -> i64 {
    let degrees = inherited(doc, page_id, b"Rotate")
        .as_ref()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(0);
    // Rotation is a multiple of 90 per the spec, but be forgiving.
    let quarter = (degrees as f64 / 90.0).round() as i64;
    quarter.rem_euclid(4) * 90
}

/// Flattens a page's resources into one dictionary.
///
/// `get_page_resources` hands back the page's own dictionary plus the ids of
/// every dictionary up the `/Parent` chain, nearest first. Merging them
/// nearest-wins reproduces PDF's inheritance in a single flat dictionary,
/// which is what a form needs.
fn resources(doc: &Document, page_id: ObjectId) -> Dictionary {
    let mut merged = Dictionary::new();
    let Ok((own, inherited_ids)) = doc.get_page_resources(page_id) else {
        return merged;
    };

    let mut sources: Vec<Dictionary> = Vec::new();
    if let Some(dict) = own {
        sources.push(dict.clone());
    }
    for id in inherited_ids {
        if let Ok(dict) = doc.get_dictionary(id) {
            sources.push(dict.clone());
        }
    }

    for dict in sources {
        for (key, value) in dict.iter() {
            if !merged.has(key) {
                merged.set(key.to_vec(), value.clone());
            }
        }
    }
    merged
}

/// Concatenates a page's content streams.
///
/// They have to be joined with whitespace: PDF treats the streams as a single
/// stream with at least one whitespace character between them, and glueing
/// them directly can fuse a trailing operator onto a leading one.
fn content(doc: &Document, page_id: ObjectId) -> Vec<u8> {
    let mut out = Vec::new();
    for id in doc.get_page_contents(page_id) {
        let Ok(stream) = doc.get_object(id).and_then(Object::as_stream) else {
            continue;
        };
        let data = stream
            .decompressed_content()
            .unwrap_or_else(|_| stream.content.clone());
        if !out.is_empty() {
            out.push(b'\n');
        }
        out.extend_from_slice(&data);
    }
    out
}

/// Turns one page into a form, and reports the size it should be painted at.
fn page_to_form(doc: &mut Document, page_id: ObjectId) -> Result<SourcePage> {
    let [x0, y0, x1, y1] = media_box(doc, page_id);
    let (bw, bh) = (x1 - x0, y1 - y0);
    let rotate = rotation(doc, page_id);
    let resources = resources(doc, page_id);
    let data = content(doc, page_id);

    // A matrix that moves the media box origin to (0, 0) and applies
    // /Rotate clockwise, so the form always occupies [0, width] x [0, height].
    let (matrix, width, height) = match rotate {
        90 => ([0.0, -1.0, 1.0, 0.0, -y0, x1], bh, bw),
        180 => ([-1.0, 0.0, 0.0, -1.0, x1, y1], bw, bh),
        270 => ([0.0, 1.0, -1.0, 0.0, y1, -x0], bh, bw),
        _ => ([1.0, 0.0, 0.0, 1.0, -x0, -y0], bw, bh),
    };

    let dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Form",
        "FormType" => 1i64,
        // The box is in form space, i.e. before /Matrix applies.
        "BBox" => vec![
            Object::Real(x0 as f32),
            Object::Real(y0 as f32),
            Object::Real(x1 as f32),
            Object::Real(y1 as f32),
        ],
        "Matrix" => matrix.iter().map(|v| Object::Real(*v as f32)).collect::<Vec<_>>(),
        "Resources" => Object::Dictionary(resources),
    };

    let form = doc.add_object(Stream::new(dict, data));
    Ok(SourcePage {
        form,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a one-page PDF, optionally rotated and with an offset box.
    fn one_page(media: [f64; 4], rotate: Option<i64>) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let content_id = doc.add_object(Stream::new(
            Dictionary::new(),
            b"1 0 0 RG 10 10 100 100 re S".to_vec(),
        ));
        let pages_id = doc.new_object_id();

        let mut page = dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => media.iter().map(|v| Object::Real(*v as f32)).collect::<Vec<_>>(),
            "Resources" => dictionary! {},
        };
        if let Some(rotate) = rotate {
            page.set("Rotate", rotate);
        }
        let page_id = doc.add_object(page);

        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1i64,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn form_of(bytes: &[u8]) -> (SourcePage, Dictionary) {
        let loaded = load(bytes).unwrap();
        let page = loaded.pages()[0];
        let dict = loaded
            .document()
            .get_object(page.form)
            .unwrap()
            .as_stream()
            .unwrap()
            .dict
            .clone();
        (page, dict)
    }

    fn matrix_of(dict: &Dictionary) -> Vec<f64> {
        dict.get(b"Matrix")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|o| match o {
                Object::Real(r) => *r as f64,
                Object::Integer(i) => *i as f64,
                _ => panic!("not a number"),
            })
            .collect()
    }

    #[test]
    fn a_page_becomes_a_form_of_its_own_size() {
        let (page, dict) = form_of(&one_page([0.0, 0.0, 595.0, 842.0], None));
        assert!((page.width - 595.0).abs() < 1e-6);
        assert!((page.height - 842.0).abs() < 1e-6);
        assert_eq!(
            dict.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Form".as_slice()
        );
        assert_eq!(matrix_of(&dict), vec![1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn the_content_travels_into_the_form() {
        let loaded = load(&one_page([0.0, 0.0, 595.0, 842.0], None)).unwrap();
        let stream = loaded
            .document()
            .get_object(loaded.pages()[0].form)
            .unwrap()
            .as_stream()
            .unwrap();
        let content = String::from_utf8_lossy(&stream.content);
        assert!(content.contains("re S"), "{content}");
    }

    #[test]
    fn a_quarter_turn_swaps_the_edges() {
        let (page, dict) = form_of(&one_page([0.0, 0.0, 595.0, 842.0], Some(90)));
        assert!((page.width - 842.0).abs() < 1e-6);
        assert!((page.height - 595.0).abs() < 1e-6);
        // Rotate clockwise, then shift back into the positive quadrant.
        assert_eq!(matrix_of(&dict), vec![0.0, -1.0, 1.0, 0.0, 0.0, 595.0]);
    }

    #[test]
    fn three_quarter_and_half_turns_are_handled() {
        let (page, dict) = form_of(&one_page([0.0, 0.0, 595.0, 842.0], Some(270)));
        assert!((page.width - 842.0).abs() < 1e-6);
        assert_eq!(matrix_of(&dict), vec![0.0, 1.0, -1.0, 0.0, 842.0, 0.0]);

        let (page, dict) = form_of(&one_page([0.0, 0.0, 595.0, 842.0], Some(180)));
        assert!((page.width - 595.0).abs() < 1e-6);
        assert_eq!(matrix_of(&dict), vec![-1.0, 0.0, 0.0, -1.0, 595.0, 842.0]);
    }

    #[test]
    fn a_rotation_that_is_not_a_right_angle_is_rounded() {
        // -90 is 270, and 450 is 90.
        let (page, _) = form_of(&one_page([0.0, 0.0, 595.0, 842.0], Some(-90)));
        assert!((page.width - 842.0).abs() < 1e-6);
        let (page, _) = form_of(&one_page([0.0, 0.0, 595.0, 842.0], Some(450)));
        assert!((page.width - 842.0).abs() < 1e-6);
    }

    #[test]
    fn an_offset_media_box_is_moved_to_the_origin() {
        let (page, dict) = form_of(&one_page([20.0, 30.0, 615.0, 872.0], None));
        assert!((page.width - 595.0).abs() < 1e-6);
        assert!((page.height - 842.0).abs() < 1e-6);
        assert_eq!(matrix_of(&dict), vec![1.0, 0.0, 0.0, 1.0, -20.0, -30.0]);
        // The box itself stays in form space.
        let bbox = dict.get(b"BBox").unwrap().as_array().unwrap();
        assert!(matches!(bbox[0], Object::Real(r) if (r - 20.0).abs() < 1e-6));
    }

    #[test]
    fn an_inverted_media_box_is_normalised() {
        let (page, _) = form_of(&one_page([595.0, 842.0, 0.0, 0.0], None));
        assert!((page.width - 595.0).abs() < 1e-6);
        assert!((page.height - 842.0).abs() < 1e-6);
    }

    #[test]
    fn a_degenerate_media_box_falls_back_to_letter() {
        let (page, _) = form_of(&one_page([0.0, 0.0, 0.0, 0.0], None));
        assert!((page.width - 612.0).abs() < 1e-6);
        assert!((page.height - 792.0).abs() < 1e-6);
    }

    #[test]
    fn pages_are_counted_without_being_converted() {
        assert_eq!(
            page_count(&one_page([0.0, 0.0, 595.0, 842.0], None)).unwrap(),
            1
        );
    }

    #[test]
    fn a_media_box_inherited_from_the_page_tree_is_found() {
        // Build a document whose page has no MediaBox of its own.
        let mut doc = Document::with_version("1.7");
        let content_id = doc.add_object(Stream::new(Dictionary::new(), b"".to_vec()));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1i64,
                "MediaBox" => vec![0.into(), 0.into(), Object::Real(200.0), Object::Real(400.0)],
                "Resources" => dictionary! {},
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        let (page, _) = form_of(&bytes);
        assert!((page.width - 200.0).abs() < 1e-6);
        assert!((page.height - 400.0).abs() < 1e-6);
    }

    #[test]
    fn rubbish_is_refused() {
        assert!(load(b"not a pdf at all").is_err());
    }
}
