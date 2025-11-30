// This code is inspired by https://github.com/fschutt/printpdf/blob/2bebdc65d06dafbe926ed4b43fedd10f966c59d3/src/xobject.rs

use crate::error::Error;
use lopdf::Object::{Integer, Real};
use lopdf::{Document, ObjectId};
use png::{BitDepth, ColorType};

#[derive(Debug, Clone)]
pub struct ImageXObject {
    pub width: u32,
    pub height: u32,
    pub color_space: ColorType,
    pub bits_per_component: BitDepth,
    pub interpolate: bool,
    pub image_data: Vec<u8>,
    pub s_mask: Option<ObjectId>,
}

impl ImageXObject {
    pub fn try_from(width: u32, height: u32, image_data: Vec<u8>) -> Result<(Self, Self), Error> {
        let image_color_data = Self::rgba_to_rgb(&image_data);
        let alpha_data = Self::rgba_to_a(&image_data);

        Ok((
            Self {
                width,
                height,
                color_space: ColorType::Rgb,
                bits_per_component: BitDepth::Eight,
                image_data: image_color_data,
                interpolate: false,
                s_mask: None, // This should be filled in later
            },
            Self {
                width,
                height,
                color_space: ColorType::Grayscale,
                bits_per_component: BitDepth::Eight,
                image_data: alpha_data,
                interpolate: false,
                s_mask: None,
            },
        ))
    }

    fn rgba_to_rgb(data: &[u8]) -> Vec<u8> {
        let mut temp_counter = 0;
        let mut temp = [0u8; 3];
        // Doing `/4*3` will just make things to complicated and images will be small anyway.
        let mut output = Vec::with_capacity(data.len());
        for byte in data {
            match temp_counter {
                0..=2 => {
                    // Store value r, g or b.
                    temp[temp_counter] = *byte;
                    // Increase counter
                    temp_counter += 1;
                }
                _ => {
                    // Skip alpha
                    // and
                    // Color of 1 pixel is consumes (r, g, b, a)
                    output.extend_from_slice(&temp);
                    temp_counter = 0;
                }
            }
        }
        output
    }

    fn rgba_to_a(data: &[u8]) -> Vec<u8> {
        let mut temp_counter = 0;
        let mut output = Vec::with_capacity(data.len() / 4);
        for byte in data {
            match temp_counter {
                0..=2 => {
                    // Skip r, g, b
                    // Increase counter
                    temp_counter += 1;
                }
                _ => {
                    // Store alpha
                    // and
                    // Color of 1 pixel is consumes (r, g, b, a)
                    output.extend_from_slice(&[*byte]);
                    temp_counter = 0;
                }
            }
        }
        output
    }
}

// Inspired and derived from: https://github.com/fschutt/printpdf/blob/2bebdc65d06dafbe926ed4b43fedd10f966c59d3/src/xobject.rs#L245
impl From<ImageXObject> for lopdf::Stream {
    fn from(image: ImageXObject) -> Self {
        use lopdf::Object::*;

        let cs: &'static str = match image.color_space {
            ColorType::Rgb => "DeviceRGB",
            ColorType::Grayscale => "DeviceGray",
            ColorType::Indexed => "Indexed",
            ColorType::Rgba | ColorType::GrayscaleAlpha => "DeviceN",
        };

        let bbox: lopdf::Object = Array(
            [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
                .iter()
                .copied()
                .map(Real)
                .collect(),
        );

        let mut dict = lopdf::Dictionary::from_iter(vec![
            ("Type", Name("XObject".as_bytes().to_vec())),
            ("Subtype", Name("Image".as_bytes().to_vec())),
            ("Width", Integer(image.width as i64)),
            ("Height", Integer(image.height as i64)),
            ("Interpolate", image.interpolate.into()),
            ("BitsPerComponent", Integer(image.bits_per_component as i64)),
            ("ColorSpace", Name(cs.as_bytes().to_vec())),
            ("BBox", bbox),
        ]);
        if let Some(s_mask) = image.s_mask {
            dict.set("SMask", Reference(s_mask));
        }

        lopdf::Stream::new(dict, image.image_data)
    }
}

impl From<ImageXObject> for lopdf::Object {
    fn from(image: ImageXObject) -> Self {
        lopdf::Object::Stream(image.into())
    }
}

/// Microsoft Print to PDF generates documents with a flipped Y-axis.
/// This function checks if such document is provided.
pub fn has_flipped_coordinates(document: &Document) -> bool {
    for (_page_number, page_id) in document.get_pages() {
        if let Ok(content) = document.get_and_decode_page_content(page_id) {
            for operation in &content.operations {
                if operation.operator == "cm" && operation.operands.len() == 6 {
                    // Matrix format: [a b c d e f] cm
                    // d is the Y-scale component (index 3)
                    let y_scale = match &operation.operands[3] {
                        Real(val) => *val,
                        Integer(val) => *val as f32,
                        _ => continue,
                    };

                    if y_scale < 0.0 {
                        return true;
                    }
                }
            }
        }
    }
    false
}
