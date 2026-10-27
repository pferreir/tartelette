//! The Tartelette logo: a texture for the window, and the native window icon.
use egui::{ColorImage, Context, TextureHandle, TextureOptions, Vec2};

const PNG: &[u8] = include_bytes!("../../../assets/tartelette_small.png");

fn decode() -> image::RgbaImage {
    image::load_from_memory_with_format(PNG, image::ImageFormat::Png)
        .expect("assets/tartelette.png")
        .to_rgba8()
}

pub struct Logo {
    texture: TextureHandle,
}

impl Logo {
    pub fn new(ctx: &Context) -> Self {
        let img = decode();
        let size = [img.width() as usize, img.height() as usize];
        let color = ColorImage::from_rgba_unmultiplied(size, img.as_raw());
        Self {
            texture: ctx.load_texture("logo", color, TextureOptions::LINEAR),
        }
    }

    /// The logo scaled to `height` points, keeping its aspect ratio.
    pub fn image(&self, height: f32) -> egui::Image<'_> {
        let s = self.texture.size_vec2();
        egui::Image::new(&self.texture).fit_to_exact_size(Vec2::new(height * s.x / s.y, height))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn window_icon() -> egui::IconData {
    let img = image::imageops::resize(&decode(), 256, 256, image::imageops::FilterType::Lanczos3);
    egui::IconData {
        rgba: img.into_raw(),
        width: 256,
        height: 256,
    }
}
