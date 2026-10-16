//! Draws the sign-in QR code: dark modules on a light square, in the theme's own black and white.

use qrcode::{Color, EcLevel, QrCode};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Pixels per module. The image is scaled down on screen with nearest-neighbour sampling, so a few
/// pixels per module keep the modules even.
const SCALE: usize = 8;
/// Theme.bg and Theme.text.
const DARK: Rgba8Pixel = Rgba8Pixel { r: 0x0e, g: 0x0e, b: 0x10, a: 0xff };
const LIGHT: Rgba8Pixel = Rgba8Pixel { r: 0xed, g: 0xed, b: 0xef, a: 0xff };

/// The QR code for `text`, without a quiet zone (the screen draws one around it).
pub fn image(text: &str) -> Option<Image> {
    let (width, modules) = modules(text)?;
    let size = width * SCALE;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(size as u32, size as u32);
    let pixels = buffer.make_mut_slice();
    for (index, pixel) in pixels.iter_mut().enumerate() {
        let (x, y) = (index % size / SCALE, index / size / SCALE);
        *pixel = if modules[y * width + x] { DARK } else { LIGHT };
    }
    Some(Image::from_rgba8(buffer))
}

/// The side of the code in modules, and which modules are dark, row by row.
fn modules(text: &str) -> Option<(usize, Vec<bool>)> {
    let code = QrCode::with_error_correction_level(text, EcLevel::M).ok()?;
    let dark = code.to_colors().into_iter().map(|color| color == Color::Dark).collect();
    Some((code.width(), dark))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_a_square_code_with_finder_patterns() {
        let (width, dark) = modules("https://discord.com/ra/3o0wJ8vSZB2tFI-hoqKcj3fKUpWNk3ULUDt4TQBYLOs").unwrap();
        assert_eq!(dark.len(), width * width);
        // The top-left finder pattern: a dark ring, a light ring, a dark centre.
        assert!(dark[0] && dark[6] && dark[6 * width]);
        assert!(!dark[width + 1] && dark[2 * width + 2]);

        let image = image("https://discord.com/ra/abc").unwrap();
        assert_eq!(image.size().width % SCALE as u32, 0);
    }
}
