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

/// Reads a number, following a reference if that is what it is.
fn number(doc: &Document, value: &Object) -> f64 {
    match value {
        Object::Integer(i) => *i as f64,
        Object::Real(r) => *r as f64,
        Object::Reference(id) => doc.get_object(*id).map(|o| number(doc, o)).unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Reads a rectangle, normalised so that `x0 < x1` and `y0 < y1`.
fn rectangle(doc: &Document, value: &Object) -> Option<[f64; 4]> {
    let array = match value {
        Object::Array(a) => a.clone(),
        Object::Reference(id) => doc.get_object(*id).ok()?.as_array().ok()?.clone(),
        _ => return None,
    };
    if array.len() != 4 {
        return None;
    }
    let v: Vec<f64> = array.iter().map(|o| number(doc, o)).collect();
    Some([
        v[0].min(v[2]),
        v[1].min(v[3]),
        v[0].max(v[2]),
        v[1].max(v[3]),
    ])
}

/// Reads one of the page's rectangle attributes, if it is usable.
fn page_box(doc: &Document, page_id: ObjectId, key: &[u8]) -> Option<[f64; 4]> {
    let value = inherited(doc, page_id, key)?;
    let rect = rectangle(doc, &value)?;
    // A degenerate box would make the page invisible.
    if rect[2] - rect[0] < 1.0 || rect[3] - rect[1] < 1.0 {
        return None;
    }
    Some(rect)
}

/// The page's media box as `[x0, y0, x1, y1]`.
fn media_box(doc: &Document, page_id: ObjectId) -> [f64; 4] {
    // A page with no media box is Letter by PDF's own default.
    page_box(doc, page_id, b"MediaBox").unwrap_or([0.0, 0.0, 612.0, 792.0])
}

/// `/CropBox` is a visible part of the page.
fn visible_box(doc: &Document, page_id: ObjectId) -> [f64; 4] {
    let media = media_box(doc, page_id);
    let Some(crop) = page_box(doc, page_id, b"CropBox") else {
        return media;
    };

    let clipped = [
        crop[0].max(media[0]),
        crop[1].max(media[1]),
        crop[2].min(media[2]),
        crop[3].min(media[3]),
    ];
    // Boxes that do not overlap leave nothing to show, so keep the media box.
    if clipped[2] - clipped[0] < 1.0 || clipped[3] - clipped[1] < 1.0 {
        return media;
    }
    clipped
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

/// Annotation flags, from PDF 32000-1 table 165.
mod annot_flag {
    /// Not shown at all, on screen or on paper.
    pub const HIDDEN: i64 = 1 << 1;
    /// Not shown on screen; a mark the author did not mean to be seen.
    pub const NOVIEW: i64 = 1 << 5;
}

/// The page's annotations, as dictionaries.
fn annotations(doc: &Document, page_id: ObjectId) -> Vec<Dictionary> {
    let Ok(page) = doc.get_dictionary(page_id) else {
        return Vec::new();
    };
    let array = match page.get(b"Annots") {
        Ok(Object::Array(a)) => a.clone(),
        Ok(Object::Reference(id)) => match doc.get_object(*id) {
            Ok(Object::Array(a)) => a.clone(),
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    };

    array
        .iter()
        .filter_map(|entry| match entry {
            Object::Reference(id) => doc.get_dictionary(*id).ok().cloned(),
            Object::Dictionary(dict) => Some(dict.clone()),
            _ => None,
        })
        .collect()
}

/// The appearance stream an annotation is currently showing.
///
/// `/AP` `/N` is either the stream itself or, for anything with more than one
/// look such as a checkbox, a dictionary of states that `/AS` chooses from.
fn appearance_stream(doc: &Document, annot: &Dictionary) -> Option<ObjectId> {
    let appearances = match annot.get(b"AP").ok()? {
        Object::Dictionary(dict) => dict.clone(),
        Object::Reference(id) => doc.get_dictionary(*id).ok()?.clone(),
        _ => return None,
    };

    match appearances.get(b"N").ok()? {
        Object::Reference(id) => match doc.get_object(*id) {
            Ok(Object::Stream(_)) => Some(*id),
            Ok(Object::Dictionary(states)) => chosen_state(annot, states),
            _ => None,
        },
        Object::Dictionary(states) => chosen_state(annot, states),
        _ => None,
    }
}

/// Picks the appearance named by `/AS`.
fn chosen_state(annot: &Dictionary, states: &Dictionary) -> Option<ObjectId> {
    if let Ok(Object::Name(state)) = annot.get(b"AS")
        && let Ok(Object::Reference(id)) = states.get(state)
    {
        return Some(*id);
    }
    // With no `/AS` to go on, a single state is still unambiguous.
    if states.len() == 1
        && let Some((_, Object::Reference(id))) = states.iter().next()
    {
        return Some(*id);
    }
    None
}

/// The matrix that puts an appearance stream inside its annotation rectangle.
///
/// PDF 32000-1 12.5.5: send the appearance's `/BBox` through its `/Matrix`,
/// take the bounds of what comes out, and map those onto `/Rect`.
fn placement(doc: &Document, annot: &Dictionary, appearance: ObjectId) -> Option<[f64; 6]> {
    let rect = rectangle(doc, annot.get(b"Rect").ok()?)?;
    let stream = doc.get_object(appearance).ok()?.as_stream().ok()?;
    let bbox = rectangle(doc, stream.dict.get(b"BBox").ok()?)?;

    let m = stream
        .dict
        .get(b"Matrix")
        .ok()
        .and_then(|o| o.as_array().ok())
        .map(|a| a.iter().map(|v| number(doc, v)).collect::<Vec<f64>>())
        .filter(|v| v.len() == 6)
        .unwrap_or_else(|| vec![1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    let corners = [
        (bbox[0], bbox[1]),
        (bbox[2], bbox[1]),
        (bbox[2], bbox[3]),
        (bbox[0], bbox[3]),
    ];
    let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
    let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (x, y) in corners {
        let tx = m[0] * x + m[2] * y + m[4];
        let ty = m[1] * x + m[3] * y + m[5];
        x0 = x0.min(tx);
        x1 = x1.max(tx);
        y0 = y0.min(ty);
        y1 = y1.max(ty);
    }

    // An appearance with no area cannot be scaled onto anything.
    if x1 - x0 <= f64::EPSILON || y1 - y0 <= f64::EPSILON {
        return None;
    }

    let sx = (rect[2] - rect[0]) / (x1 - x0);
    let sy = (rect[3] - rect[1]) / (y1 - y0);
    Some([sx, 0.0, 0.0, sy, rect[0] - x0 * sx, rect[1] - y0 * sy])
}

/// Draws the page's annotations into its own content.
fn flatten_annotations(doc: &Document, page_id: ObjectId, resources: &mut Dictionary) -> Vec<u8> {
    let annots = annotations(doc, page_id);
    if annots.is_empty() {
        return Vec::new();
    }

    let mut xobjects = match resources.get(b"XObject") {
        Ok(Object::Dictionary(dict)) => dict.clone(),
        Ok(Object::Reference(id)) => doc.get_dictionary(*id).cloned().unwrap_or_default(),
        _ => Dictionary::new(),
    };

    let mut out = Vec::new();
    for (index, annot) in annots.iter().enumerate() {
        let flags = annot.get(b"F").and_then(Object::as_i64).unwrap_or(0);
        if flags & (annot_flag::HIDDEN | annot_flag::NOVIEW) != 0 {
            continue;
        }
        // A popup is the note window, not the mark left on the page.
        if matches!(
            annot.get(b"Subtype").and_then(Object::as_name),
            Ok(b"Popup")
        ) {
            continue;
        }

        let Some(appearance) = appearance_stream(doc, annot) else {
            continue;
        };
        let Some(m) = placement(doc, annot, appearance) else {
            continue;
        };

        // The page may already have an XObject under this name.
        let mut name = format!("WmAnnot{index}");
        while xobjects.has(name.as_bytes()) {
            name.push('_');
        }
        xobjects.set(name.clone(), Object::Reference(appearance));

        out.extend_from_slice(
            format!(
                "q {:.4} {:.4} {:.4} {:.4} {:.4} {:.4} cm /{} Do Q\n",
                m[0], m[1], m[2], m[3], m[4], m[5], name
            )
            .as_bytes(),
        );
    }

    if !out.is_empty() {
        resources.set("XObject", Object::Dictionary(xobjects));
    }
    out
}

/// Turns one page into a form, and reports the size it should be painted at.
fn page_to_form(doc: &mut Document, page_id: ObjectId) -> Result<SourcePage> {
    let [x0, y0, x1, y1] = visible_box(doc, page_id);
    let (bw, bh) = (x1 - x0, y1 - y0);
    let rotate = rotation(doc, page_id);
    let mut resources = resources(doc, page_id);
    let mut data = content(doc, page_id);

    // Annotations go on top of the page content, the way a viewer stacks them.
    let annotations = flatten_annotations(doc, page_id, &mut resources);
    if !annotations.is_empty() {
        data.push(b'\n');
        data.extend_from_slice(&annotations);
    }

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

    /// Builds a one-page PDF with a crop box inside its media box.
    fn cropped_page(media: [f64; 4], crop: [f64; 4]) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let content_id = doc.add_object(Stream::new(Dictionary::new(), b"0 0 0 rg".to_vec()));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => media.iter().map(|v| Object::Real(*v as f32)).collect::<Vec<_>>(),
            "CropBox" => crop.iter().map(|v| Object::Real(*v as f32)).collect::<Vec<_>>(),
            "Resources" => dictionary! {},
        });
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

    /// Builds a one-page PDF carrying one annotation with an appearance.
    fn annotated_page(subtype: &str, flags: Option<i64>) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let content_id = doc.add_object(Stream::new(Dictionary::new(), b"0 0 0 rg".to_vec()));
        let appearance = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            },
            b"1 0 0 rg 0 0 10 10 re f".to_vec(),
        ));
        let mut annot = dictionary! {
            "Type" => "Annot",
            "Subtype" => subtype,
            // Ten units square becomes a hundred wide and fifty tall.
            "Rect" => vec![20.into(), 30.into(), 120.into(), 80.into()],
            "AP" => dictionary! { "N" => Object::Reference(appearance) },
        };
        if let Some(flags) = flags {
            annot.set("F", flags);
        }
        let annot_id = doc.add_object(annot);

        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 400.into(), 400.into()],
            "Resources" => dictionary! {},
            "Annots" => vec![Object::Reference(annot_id)],
        });
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

    /// The content stream of the first page's form.
    fn form_body(bytes: &[u8]) -> String {
        let loaded = load(bytes).unwrap();
        let stream = loaded
            .document()
            .get_object(loaded.pages()[0].form)
            .unwrap()
            .as_stream()
            .unwrap()
            .clone();
        let data = stream
            .decompressed_content()
            .unwrap_or_else(|_| stream.content.clone());
        String::from_utf8_lossy(&data).into_owned()
    }

    #[test]
    fn the_crop_box_decides_how_much_of_the_page_is_shown() {
        let (page, _) = form_of(&cropped_page(
            [0.0, 0.0, 400.0, 300.0],
            [20.0, 20.0, 380.0, 280.0],
        ));
        assert!(
            (page.width - 360.0).abs() < 0.01,
            "width was {}",
            page.width
        );
        assert!(
            (page.height - 260.0).abs() < 0.01,
            "height was {}",
            page.height
        );
    }

    #[test]
    fn a_crop_box_reaching_outside_the_sheet_cannot_enlarge_it() {
        let (page, _) = form_of(&cropped_page(
            [0.0, 0.0, 400.0, 300.0],
            [-50.0, -50.0, 900.0, 900.0],
        ));
        assert!(
            (page.width - 400.0).abs() < 0.01,
            "width was {}",
            page.width
        );
        assert!(
            (page.height - 300.0).abs() < 0.01,
            "height was {}",
            page.height
        );
    }

    #[test]
    fn an_annotation_is_drawn_into_the_page() {
        let body = form_body(&annotated_page("Widget", None));
        assert!(body.contains("Do"), "the appearance was not drawn: {body}");
        assert!(
            body.contains("WmAnnot0"),
            "the appearance was not named: {body}"
        );
    }

    #[test]
    fn an_annotation_is_stretched_over_its_rectangle() {
        let body = form_body(&annotated_page("Widget", None));
        // A ten-unit square mapped onto 100x50 at (20, 30).
        let line = body
            .lines()
            .find(|l| l.contains("cm"))
            .unwrap_or_else(|| panic!("no placement in {body}"));
        for expected in ["10.0000", "5.0000", "20.0000", "30.0000"] {
            assert!(line.contains(expected), "{expected} missing from {line}");
        }
    }

    #[test]
    fn a_hidden_annotation_is_left_where_the_author_put_it() {
        // Flag bit 2 is Hidden; flattening must not reveal it.
        let body = form_body(&annotated_page("Widget", Some(1 << 1)));
        assert!(
            !body.contains("WmAnnot"),
            "a hidden annotation was drawn: {body}"
        );
    }

    #[test]
    fn a_popup_is_not_drawn_on_the_page() {
        let body = form_body(&annotated_page("Popup", None));
        assert!(!body.contains("WmAnnot"), "a popup was drawn: {body}");
    }
}
