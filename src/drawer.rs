use image::{ImageBuffer, Rgba};
use imageproc::drawing::{draw_text_mut, text_size};
use imageproc::geometric_transformations::{warp, Interpolation, Projection};
use rusttype::{Font, Scale};

pub struct WatermarkFactory {
    font: Font<'static>,
    scale: Scale
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>
}

impl WatermarkFactory {
    pub fn new() -> Self {
        Self {
            font: Font::try_from_vec(Vec::from(include_bytes!("../resources/Inter-Black.ttf") as &[u8])).unwrap(),
            scale: Scale { x: 128.0, y: 128.0 }
        }
    }

    pub fn draw(&self, text: String, width: u32, height: u32) -> Image {
        let mut image = ImageBuffer::new(width, height);

        let size = text_size(self.scale, &self.font, &text);
    
        draw_text_mut(&mut image, Rgba([192u8, 192u8, 192u8, 64u8]), (width as i32 - size.0) / 2, (height as i32 - size.1) / 2, self.scale, &self.font, &text);

        let projection = Projection::translate(0.5 * width as f32, 0.5 * height as f32) * Projection::scale(1.0, -1.0) * Projection::rotate(-0.45) * Projection::translate(-0.5 * width as f32, -0.5 * height as f32);

        Image {
            width, height,
            data: warp(&image, &projection, Interpolation::Nearest, Rgba([0u8, 0u8, 0u8, 0u8])).as_raw().to_vec()
        }
    }
}
