//! A player's artwork, decoded and encoded with the `image` crate: receiver
//! artwork as a JPEG ([`fit_jpeg`]) and the Now Playing card's square tile
//! ([`ARTWORK_SIDE`], [`cover_size`]), premultiplied as the card draws it.

use std::io::Cursor;
use std::sync::Arc;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use image::{ExtendedColorType, ImageReader, Limits, RgbaImage};

use crate::audio::now_playing::fit_jpeg;
use crate::debug_log::DebugLog;
use crate::now_playing_card::{ARTWORK_SIDE, CardArtwork, cover_size};

/// AudioSync's icon, which receivers show while no track's artwork does.
pub const ICON_PNG: &[u8] = include_bytes!("../../../res/audiosync-256.png");

/// The largest side decoded; cover art is a few thousand pixels at most.
const SIDE_LIMIT: u32 = 8192;

/// The most memory one decode may take.
const DECODE_LIMIT: u64 = 256 << 20;

/// AudioSync's icon fitted as receiver artwork; `None`, logged, when it
/// can't be.
pub fn fitted_icon(log: &DebugLog) -> Option<Arc<[u8]>> {
    match fit_image(ICON_PNG) {
        Ok(Some(jpeg)) => Some(Arc::from(jpeg)),
        Ok(None) => {
            log.log(|| "now playing: AudioSync's icon doesn't fit as artwork".to_owned());
            None
        }
        Err(error) => {
            log.log(|| format!("now playing: AudioSync's icon couldn't be encoded: {error}"));
            None
        }
    }
}

/// An encoded image as fitted receiver artwork.
pub fn fit_image(image: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let decoded = decode(image)?;
    let (width, height) = decoded.dimensions();
    let mut failure = None;
    let fitted = fit_jpeg(width, height, |width, height, quality| match encode_scaled(
        &decoded, width, height, quality,
    ) {
        Ok(jpeg) => Some(jpeg),
        Err(error) => {
            failure = Some(error);
            None
        }
    });
    match failure {
        Some(error) if fitted.is_none() => Err(error),
        _ => Ok(fitted),
    }
}

/// An encoded image scaled to cover the card's tile and cropped to it;
/// `None` for an empty image.
pub fn card_artwork(image: &[u8]) -> Result<Option<CardArtwork>, String> {
    let decoded = decode(image)?;
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return Ok(None);
    }
    let side = ARTWORK_SIDE;
    let (scaled_width, scaled_height) = cover_size(width, height, side);
    let scaled = imageops::resize(
        &decoded,
        scaled_width,
        scaled_height,
        FilterType::CatmullRom,
    );
    let tile = imageops::crop_imm(
        &scaled,
        (scaled_width - side) / 2,
        (scaled_height - side) / 2,
        side,
        side,
    )
    .to_image();
    let mut rgba = tile.into_raw();
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3];
        for channel in &mut pixel[..3] {
            *channel = over_black(*channel, alpha);
        }
    }
    Ok(Some(CardArtwork {
        side,
        rgba: Arc::from(rgba),
    }))
}

fn decode(image: &[u8]) -> Result<RgbaImage, String> {
    let mut reader = ImageReader::new(Cursor::new(image))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(SIDE_LIMIT);
    limits.max_image_height = Some(SIDE_LIMIT);
    limits.max_alloc = Some(DECODE_LIMIT);
    reader.limits(limits);
    reader
        .decode()
        .map(|decoded| decoded.to_rgba8())
        .map_err(|error| error.to_string())
}

/// The image scaled to `width` × `height`, drawn on black (JPEG has no
/// transparency) and encoded as a JPEG at `quality` (0 to 1).
fn encode_scaled(
    image: &RgbaImage,
    width: u32,
    height: u32,
    quality: f32,
) -> Result<Vec<u8>, String> {
    let scaled = imageops::resize(image, width, height, FilterType::CatmullRom);
    let rgb: Vec<u8> = scaled
        .pixels()
        .flat_map(|pixel| {
            let [red, green, blue, alpha] = pixel.0;
            [red, green, blue].map(|channel| over_black(channel, alpha))
        })
        .collect();
    let quality = (quality * 100.0).round().clamp(1.0, 100.0) as u8;
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, quality)
        .encode(&rgb, width, height, ExtendedColorType::Rgb8)
        .map_err(|error| error.to_string())?;
    Ok(jpeg)
}

fn over_black(channel: u8, alpha: u8) -> u8 {
    // At most 255 * 255 / 255, so it fits.
    (u16::from(channel) * u16::from(alpha) / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::now_playing::ARTWORK_LIMIT;

    #[test]
    fn the_icon_fits_as_a_jpeg_and_as_the_cards_tile() {
        let jpeg = fit_image(ICON_PNG).unwrap().expect("the icon fits");
        assert!(jpeg.len() <= ARTWORK_LIMIT && jpeg.starts_with(&[0xFF, 0xD8]));
        let tile = card_artwork(ICON_PNG).unwrap().expect("the icon fits");
        assert_eq!(tile.side, ARTWORK_SIDE);
        assert_eq!(tile.rgba.len(), (tile.side * tile.side * 4) as usize);
    }

    #[test]
    fn a_wide_image_is_cropped_to_a_square_tile() {
        let mut wide = RgbaImage::new(300, 100);
        for pixel in wide.pixels_mut() {
            pixel.0 = [200, 100, 50, 255];
        }
        let mut png = Vec::new();
        wide.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let tile = card_artwork(&png).unwrap().unwrap();
        assert_eq!(tile.rgba.len(), (ARTWORK_SIDE * ARTWORK_SIDE * 4) as usize);
        assert_eq!(&tile.rgba[..4], &[200, 100, 50, 255]);
    }

    #[test]
    fn transparency_is_drawn_on_black() {
        assert_eq!(over_black(200, 128), 100);
        assert_eq!(over_black(255, 255), 255);
        assert_eq!(over_black(255, 0), 0);
    }

    #[test]
    fn bytes_that_are_no_image_fail_to_decode() {
        assert!(fit_image(b"not an image").is_err());
        assert!(card_artwork(b"not an image").is_err());
    }
}
