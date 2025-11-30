use ab_glyph::FontRef;
use image::{ImageBuffer, Rgba};
use imageproc::drawing::{draw_text_mut, text_size};
use imageproc::geometric_transformations::{Interpolation, Projection, warp};

pub struct WatermarkFactory {
    font: FontRef<'static>,
    scale: f32,
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Default for WatermarkFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl WatermarkFactory {
    pub fn new() -> Self {
        Self {
            font: FontRef::try_from_slice(include_bytes!("../resources/Inter-Black.ttf") as &[u8])
                .unwrap(),
            scale: 256.0,
        }
    }

    pub fn draw(&self, text: String, width: u32, height: u32, mirror: bool) -> Image {
        let mut image = ImageBuffer::new(width, height);

        let size = text_size(self.scale, &self.font, &text);

        draw_text_mut(
            &mut image,
            Rgba([128u8, 128u8, 128u8, 192u8]),
            (width - size.0) as i32 / 2,
            (height - size.1) as i32 / 2,
            self.scale,
            &self.font,
            &text,
        );

        let mut projection = Projection::translate(0.5 * width as f32, 0.5 * height as f32);

        if mirror {
            projection = projection * Projection::scale(1.0, -1.0);
        }

        projection = projection
            * Projection::rotate(-0.45)
            * Projection::translate(-0.5 * width as f32, -0.5 * height as f32);

        Image {
            width,
            height,
            data: warp(
                &image,
                &projection,
                Interpolation::Nearest,
                Rgba([0u8, 0u8, 0u8, 0u8]),
            )
            .as_raw()
            .to_vec(),
        }
    }
}
