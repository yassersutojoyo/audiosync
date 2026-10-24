//! Now-playing metadata over SET_PARAMETER (P123 to P128).
//!
//! Metadata is optional: the session sends only the types a receiver lists,
//! after any pending volume request, and a failure is only a warning. The
//! binary-plist now-playing format of features bit 50 has no public
//! description and is never sent (P128).

use std::fmt;

use crate::discovery::Features;
use crate::error::Warning;

/// Largest artwork the sender sends (P125); the host scales or drops larger
/// images.
pub const MAX_ARTWORK_BYTES: usize = 512 * 1024;

/// Track title, artist and album, as UTF-8 text (P124).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
}

/// One metadata request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Metadata {
    /// Track information that applies from RTP timestamp `rtptime` (P124).
    Track { info: TrackInfo, rtptime: u32 },
    /// A JPEG image that applies from RTP timestamp `rtptime` (P125).
    Artwork { jpeg: Vec<u8>, rtptime: u32 },
    /// Track progress as three absolute RTP timestamps (P126). The host sends
    /// it only when it knows a track's start and length.
    Progress { start: u32, current: u32, end: u32 },
}

impl Metadata {
    pub fn kind(&self) -> MetadataKind {
        match self {
            Metadata::Track { .. } => MetadataKind::Text,
            Metadata::Artwork { .. } => MetadataKind::Artwork,
            Metadata::Progress { .. } => MetadataKind::Progress,
        }
    }

    /// The `Content-Type` of the request (P124, P125, P126).
    pub fn content_type(&self) -> &'static str {
        match self {
            Metadata::Track { .. } => "application/x-dmap-tagged",
            Metadata::Artwork { .. } => "image/jpeg",
            Metadata::Progress { .. } => "text/parameters",
        }
    }

    /// The RTP timestamp of the `RTP-Info: rtptime=<t>` header that metadata
    /// carries (P33). Progress names the current position.
    pub fn rtptime(&self) -> u32 {
        match self {
            Metadata::Track { rtptime, .. } | Metadata::Artwork { rtptime, .. } => *rtptime,
            Metadata::Progress { current, .. } => *current,
        }
    }

    /// The request body.
    pub fn body(&self) -> Vec<u8> {
        match self {
            Metadata::Track { info, .. } => dmap_track(info),
            Metadata::Artwork { jpeg, .. } => jpeg.clone(),
            Metadata::Progress {
                start,
                current,
                end,
            } => format!("progress: {start}/{current}/{end}\r\n").into_bytes(),
        }
    }

    /// Checks what the sender can check before queueing: artwork must be a
    /// JPEG of at most 512 KiB (P125).
    pub fn validate(&self) -> Result<(), MetadataError> {
        match self {
            Metadata::Artwork { jpeg, .. } => check_jpeg(jpeg),
            _ => Ok(()),
        }
    }
}

fn check_jpeg(jpeg: &[u8]) -> Result<(), MetadataError> {
    if !jpeg.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Err(MetadataError::NotJpeg);
    }
    if jpeg.len() > MAX_ARTWORK_BYTES {
        return Err(MetadataError::ArtworkTooLarge);
    }
    Ok(())
}

/// The two JPEG encodings of one picture that the host hands the sender
/// (P185): `large` for an AirPlay 2 session, `small` for an AirPlay 1
/// session and after a refusal. A picture within the small limits is the
/// same bytes in both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Artwork {
    pub large: Vec<u8>,
    pub small: Vec<u8>,
}

impl Artwork {
    /// One encoding for both paths.
    pub fn single(jpeg: Vec<u8>) -> Artwork {
        Artwork {
            small: jpeg.clone(),
            large: jpeg,
        }
    }

    /// Both images must pass P125's check.
    pub fn validate(&self) -> Result<(), MetadataError> {
        check_jpeg(&self.large)?;
        check_jpeg(&self.small)
    }
}

/// Which of an [`Artwork`]'s two images a request carries (P185).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtworkSize {
    Large,
    Small,
}

/// A metadata request waiting in a session's queue. Artwork picks its
/// image when it goes, so a refusal also changes what is still queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Queued {
    Item(Metadata),
    Artwork { artwork: Artwork, rtptime: u32 },
}

impl Queued {
    pub(crate) fn new(metadata: Metadata) -> Queued {
        match metadata {
            Metadata::Artwork { jpeg, rtptime } => Queued::Artwork {
                artwork: Artwork::single(jpeg),
                rtptime,
            },
            other => Queued::Item(other),
        }
    }
}

/// One session's place on P185's ladder: its path's image, then the small
/// one after a refusal, then none after a second; a new session starts
/// over.
#[derive(Debug, Clone)]
pub(crate) struct ArtworkLadder {
    next: Option<ArtworkSize>,
    warned: bool,
    in_flight: Option<(ArtworkSize, Artwork, u32)>,
}

impl ArtworkLadder {
    pub(crate) fn new(first: ArtworkSize) -> ArtworkLadder {
        ArtworkLadder {
            next: Some(first),
            warned: false,
            in_flight: None,
        }
    }

    /// The request that sends `artwork` now, kept until its answer; none
    /// once artwork has ended on this session.
    pub(crate) fn send(&mut self, artwork: Artwork, rtptime: u32) -> Option<Metadata> {
        let size = self.next?;
        let jpeg = match size {
            ArtworkSize::Large => artwork.large.clone(),
            ArtworkSize::Small => artwork.small.clone(),
        };
        self.in_flight = Some((size, artwork, rtptime));
        Some(Metadata::Artwork { jpeg, rtptime })
    }

    /// The artwork request in flight was answered, or given up on with
    /// `status` none. A refusal returns the small image to send at once
    /// with the same `RTP-Info`, if any, and the session's one warning.
    pub(crate) fn answered(
        &mut self,
        success: bool,
        status: Option<u16>,
    ) -> (Option<Queued>, Option<Warning>) {
        let Some((size, artwork, rtptime)) = self.in_flight.take() else {
            return (None, None);
        };
        if success {
            return (None, None);
        }
        let retry = size == ArtworkSize::Large && artwork.small != artwork.large;
        self.next = if retry {
            Some(ArtworkSize::Small)
        } else {
            None
        };
        let warning = (!self.warned).then(|| {
            self.warned = true;
            Warning::ArtworkRefused { status }
        });
        let resend = retry.then_some(Queued::Artwork { artwork, rtptime });
        (resend, warning)
    }

    /// The request in flight is gone with its connection.
    pub(crate) fn forget(&mut self) {
        self.in_flight = None;
    }
}

/// The metadata types of P123.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetadataKind {
    Text,
    Artwork,
    Progress,
}

/// Which metadata types a receiver lists (P123).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetadataSupport {
    pub text: bool,
    pub artwork: bool,
    pub progress: bool,
}

impl MetadataSupport {
    /// From RAOP `md`: 0 text, 1 artwork, 2 progress. Without `md`, none.
    pub fn from_raop_md(md: Option<&[u64]>) -> MetadataSupport {
        let listed = |value: u64| md.is_some_and(|list| list.contains(&value));
        MetadataSupport {
            text: listed(0),
            artwork: listed(1),
            progress: listed(2),
        }
    }

    /// On the AirPlay 2 path: features bits 17 (text), 15 (artwork) and 16
    /// (progress). Bit 50 isn't read (P128).
    pub fn from_features(features: Features) -> MetadataSupport {
        MetadataSupport {
            text: features.has(Features::METADATA_TEXT),
            artwork: features.has(Features::METADATA_ARTWORK),
            progress: features.has(Features::METADATA_PROGRESS),
        }
    }

    pub fn allows(self, kind: MetadataKind) -> bool {
        match kind {
            MetadataKind::Text => self.text,
            MetadataKind::Artwork => self.artwork,
            MetadataKind::Progress => self.progress,
        }
    }
}

/// Why a metadata request wasn't queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataError {
    /// The receiver doesn't list this type (P123).
    NotListed,
    /// Artwork that isn't a JPEG (P125).
    NotJpeg,
    /// Artwork over 512 KiB (P125).
    ArtworkTooLarge,
    /// The session has ended or is ending.
    SessionEnded,
}

impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MetadataError::NotListed => "the receiver doesn't list this metadata type",
            MetadataError::NotJpeg => "artwork must be a JPEG image",
            MetadataError::ArtworkTooLarge => "artwork is larger than 512 KiB",
            MetadataError::SessionEnded => "the session has ended",
        })
    }
}

impl std::error::Error for MetadataError {}

/// One DMAP chunk: a 4-character code, the 4-byte big-endian length of its
/// data, then the data (P124).
pub fn dmap_chunk(code: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
    let mut chunk = Vec::with_capacity(8 + data.len());
    chunk.extend_from_slice(code);
    chunk.extend_from_slice(&length.to_be_bytes());
    chunk.extend_from_slice(&data[..data.len().min(length as usize)]);
    chunk
}

/// Track information as DMAP: an `mlit` container holding `minm` (title),
/// `asar` (artist) and `asal` (album) (P124).
pub fn dmap_track(info: &TrackInfo) -> Vec<u8> {
    let mut items = dmap_chunk(b"minm", info.title.as_bytes());
    items.extend(dmap_chunk(b"asar", info.artist.as_bytes()));
    items.extend(dmap_chunk(b"asal", info.album.as_bytes()));
    dmap_chunk(b"mlit", &items)
}
