//! What the media sessions reader ([`super::media_sessions`]) makes of a
//! session: its description for track info, and its thumbnail decoded
//! through the WinRT imaging codecs, as a JPEG for receivers ([`fit_jpeg`])
//! or as the Now Playing card's square tile ([`ARTWORK_SIDE`],
//! [`cover_size`]). Decoding works on bytes already read, so it never waits
//! on the app behind a session.

use std::sync::Arc;

use windows::Foundation::PropertyType;
use windows::Foundation::PropertyValue;
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapBounds, BitmapDecoder, BitmapEncoder, BitmapInterpolationMode,
    BitmapPixelFormat, BitmapPropertySet, BitmapTransform, BitmapTypedValue, ColorManagementMode,
    ExifOrientationMode,
};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionMediaProperties as MediaProperties,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Storage::Streams::{
    Buffer, DataReader, DataWriter, IRandomAccessStream, InMemoryRandomAccessStream,
    InputStreamOptions,
};
use windows::core::{HSTRING, Interface};

use crate::audio::now_playing::{MediaSession, fit_jpeg};
use crate::debug_log::DebugLog;
use crate::now_playing_card::{ARTWORK_SIDE, CardArtwork, cover_size};

/// AudioSync's icon, which receivers show while no track's artwork does.
pub(super) const ICON_PNG: &[u8] = include_bytes!("../../../res/audiosync-256.png");

/// The JPEG encoder's quality option.
const IMAGE_QUALITY: &str = "ImageQuality";

/// A session as track info sees it, from its media properties.
pub(super) fn describe(
    session: &Session,
    properties: &MediaProperties,
) -> windows::core::Result<MediaSession> {
    let playing = session.GetPlaybackInfo()?.PlaybackStatus()? == PlaybackStatus::Playing;
    Ok(MediaSession {
        app_id: session.SourceAppUserModelId()?.to_string(),
        title: properties.Title()?.to_string(),
        artist: properties.Artist()?.to_string(),
        album: properties.AlbumTitle()?.to_string(),
        playing,
    })
}

/// AudioSync's icon fitted as receiver artwork; `None`, logged, when it
/// can't be.
pub(super) fn fitted_icon(log: &DebugLog) -> Option<Arc<[u8]>> {
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
pub(super) fn fit_image(image: &[u8]) -> windows::core::Result<Option<Vec<u8>>> {
    let decoder = BitmapDecoder::CreateAsync(&memory_stream(image)?)?.join()?;
    let (width, height) = (decoder.PixelWidth()?, decoder.PixelHeight()?);
    let mut failure = None;
    let fitted = fit_jpeg(width, height, |width, height, quality| match encode_scaled(
        &decoder, width, height, quality,
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
pub(super) fn card_artwork(image: &[u8]) -> windows::core::Result<Option<CardArtwork>> {
    let decoder = BitmapDecoder::CreateAsync(&memory_stream(image)?)?.join()?;
    let (width, height) = (decoder.PixelWidth()?, decoder.PixelHeight()?);
    if width == 0 || height == 0 {
        return Ok(None);
    }
    let side = ARTWORK_SIDE;
    let (scaled_width, scaled_height) = cover_size(width, height, side);
    let transform = BitmapTransform::new()?;
    transform.SetScaledWidth(scaled_width)?;
    transform.SetScaledHeight(scaled_height)?;
    transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    transform.SetBounds(BitmapBounds {
        X: (scaled_width - side) / 2,
        Y: (scaled_height - side) / 2,
        Width: side,
        Height: side,
    })?;
    let pixels = decoder
        .GetPixelDataTransformedAsync(
            BitmapPixelFormat::Rgba8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::IgnoreExifOrientation,
            ColorManagementMode::ColorManageToSRgb,
        )?
        .join()?
        .DetachPixelData()?;
    if pixels.len() != (side * side * 4) as usize {
        return Ok(None);
    }
    Ok(Some(CardArtwork {
        side,
        rgba: Arc::from(&pixels[..]),
    }))
}

/// `bytes` as a stream the imaging codecs can read.
pub(super) fn memory_stream(bytes: &[u8]) -> windows::core::Result<IRandomAccessStream> {
    let stream = InMemoryRandomAccessStream::new()?;
    let writer = DataWriter::new()?;
    writer.WriteBytes(bytes)?;
    stream.WriteAsync(&writer.DetachBuffer()?)?.join()?;
    stream.Seek(0)?;
    stream.cast()
}

/// The decoded image scaled to `width` × `height`, drawn on black (JPEG has
/// no transparency) and encoded as a JPEG at `quality`.
fn encode_scaled(
    decoder: &BitmapDecoder,
    width: u32,
    height: u32,
    quality: f32,
) -> windows::core::Result<Vec<u8>> {
    let transform = BitmapTransform::new()?;
    transform.SetScaledWidth(width)?;
    transform.SetScaledHeight(height)?;
    transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    // Premultiplied pixels are the image over black once alpha is dropped.
    let pixels = decoder
        .GetPixelDataTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::IgnoreExifOrientation,
            ColorManagementMode::ColorManageToSRgb,
        )?
        .join()?
        .DetachPixelData()?;
    let output = InMemoryRandomAccessStream::new()?;
    let options = BitmapPropertySet::new()?;
    options.Insert(
        &HSTRING::from(IMAGE_QUALITY),
        &BitmapTypedValue::Create(&PropertyValue::CreateSingle(quality)?, PropertyType::Single)?,
    )?;
    let encoder = BitmapEncoder::CreateWithEncodingOptionsAsync(
        BitmapEncoder::JpegEncoderId()?,
        &output,
        &options,
    )?
    .join()?;
    encoder.SetPixelData(
        BitmapPixelFormat::Bgra8,
        BitmapAlphaMode::Ignore,
        width,
        height,
        96.0,
        96.0,
        &pixels,
    )?;
    encoder.FlushAsync()?.join()?;
    read_all(&output)
}

fn read_all(stream: &InMemoryRandomAccessStream) -> windows::core::Result<Vec<u8>> {
    let size = u32::try_from(stream.Size()?).unwrap_or(u32::MAX);
    stream.Seek(0)?;
    let buffer = Buffer::Create(size)?;
    let filled = stream
        .ReadAsync(&buffer, size, InputStreamOptions::None)?
        .join()?;
    let mut data = vec![0; filled.Length()? as usize];
    DataReader::FromBuffer(&filled)?.ReadBytes(&mut data)?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::now_playing::ARTWORK_LIMIT;

    #[test]
    fn the_icon_fits_as_a_jpeg_and_as_the_cards_tile() {
        let _apartment = super::super::com::Apartment::enter().unwrap();
        let jpeg = fit_image(ICON_PNG).unwrap().expect("the icon fits");
        assert!(jpeg.len() <= ARTWORK_LIMIT && jpeg.starts_with(&[0xFF, 0xD8]));
        let tile = card_artwork(ICON_PNG).unwrap().expect("the icon fits");
        assert_eq!(tile.rgba.len(), (tile.side * tile.side * 4) as usize);
    }
}
