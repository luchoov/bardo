//! Profile pictures and server icons for the list, fetched as they come on screen and kept for the
//! session. Fetching and decoding happen off the UI thread; only the finished pixels come back.

use std::collections::{HashMap, HashSet};

use bardo_core::{CdnFetcher, CdnImage};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

use crate::logging;

/// The size asked of the CDN: the largest picture is drawn 28 px wide, and 64 covers it at 2x.
pub const SIZE: u32 = 64;

/// Called on the UI thread with a picture's path and its pixels, or `None` if it could not be had.
pub type Deliver = fn(String, Option<SharedPixelBuffer<Rgba8Pixel>>);

#[derive(Default)]
pub struct Pictures {
    fetcher: Option<CdnFetcher>,
    /// By CDN path.
    images: HashMap<String, Image>,
    pending: HashSet<String>,
    /// Not on the CDN or not decodable; not asked for again this session.
    failed: HashSet<String>,
}

impl Pictures {
    /// The picture at `path` if it is here. Otherwise it is fetched, once, and handed to `deliver`.
    pub fn get(&mut self, path: &str, deliver: Deliver) -> Option<Image> {
        if let Some(image) = self.images.get(path) {
            return Some(image.clone());
        }
        if self.failed.contains(path) || !self.pending.insert(path.to_owned()) {
            return None;
        }
        let fetcher = self.fetcher.get_or_insert_with(|| {
            let dir = logging::dir().unwrap_or_else(std::env::temp_dir).join("cache");
            CdnFetcher::start(dir)
        });
        let image = CdnImage { path: path.to_owned() };
        let path = path.to_owned();
        fetcher.fetch(&image, SIZE, move |bytes| {
            let pixels = bytes.as_deref().and_then(decode);
            let _ = slint::invoke_from_event_loop(move || deliver(path, pixels));
        });
        None
    }

    /// A fetched picture is back. Returns it, ready to show.
    pub fn arrived(&mut self, path: String, pixels: Option<SharedPixelBuffer<Rgba8Pixel>>) -> Option<Image> {
        // Cleared meanwhile: the pictures were turned off.
        if !self.pending.remove(&path) {
            return None;
        }
        let Some(pixels) = pixels else {
            self.failed.insert(path);
            return None;
        };
        let image = Image::from_rgba8(pixels);
        self.images.insert(path, image.clone());
        Some(image)
    }

    /// Forgets every picture, e.g. when they are turned off. The disk cache stays.
    pub fn clear(&mut self) {
        self.images.clear();
        self.pending.clear();
        self.failed.clear();
    }
}

/// Decodes a picture to pixels, no larger than [`SIZE`] on a side.
fn decode(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let mut image = image::load_from_memory(bytes).ok()?;
    if image.width() > SIZE || image.height() > SIZE {
        image = image.thumbnail(SIZE, SIZE);
    }
    let rgba = image.into_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn webp(side: u32) -> Vec<u8> {
        let pixels = image::RgbaImage::from_pixel(side, side, image::Rgba([200, 30, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::WebP)
            .unwrap();
        bytes
    }

    #[test]
    fn decodes_webp_and_shrinks_large_pictures() {
        let small = decode(&webp(64)).unwrap();
        assert_eq!((small.width(), small.height()), (64, 64));
        assert_eq!(small.as_slice()[0], Rgba8Pixel { r: 200, g: 30, b: 30, a: 255 });
        let large = decode(&webp(512)).unwrap();
        assert_eq!((large.width(), large.height()), (SIZE, SIZE));
        assert!(decode(b"not a picture").is_none());
    }

    #[test]
    fn a_picture_that_never_came_is_not_asked_for_again() {
        let mut pictures = Pictures::default();
        pictures.pending.insert("avatars/1/abc".into());
        assert!(pictures.arrived("avatars/1/abc".into(), None).is_none());
        assert!(pictures.failed.contains("avatars/1/abc"));
        // Arriving after being cleared shows nothing.
        pictures.clear();
        assert!(pictures.arrived("avatars/1/abc".into(), decode(&webp(8))).is_none());
        assert!(pictures.images.is_empty());
    }
}
