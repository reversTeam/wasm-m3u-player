use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Supported container formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContainerFormat {
    Mp4,
    Mkv,
    WebM,
    Unknown,
}

/// Errors during demuxing.
#[derive(Debug, Error)]
pub enum DemuxError {
    #[error("unsupported format: {0:?}")]
    UnsupportedFormat(ContainerFormat),
    #[error("invalid data: {0}")]
    InvalidData(String),
    #[error("end of stream")]
    EndOfStream,
    #[error("io error: {0}")]
    IoError(String),
}

/// A single encoded chunk (video or audio sample).
#[derive(Debug, Clone)]
pub struct EncodedChunk {
    pub track_id: u32,
    pub is_video: bool,
    pub is_keyframe: bool,
    pub timestamp_us: i64,
    pub duration_us: i64,
    pub data: Vec<u8>,
}

/// Media information extracted from the container header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub container: ContainerFormat,
    pub duration_us: Option<i64>,
    pub video_tracks: Vec<VideoTrackInfo>,
    pub audio_tracks: Vec<AudioTrackInfo>,
}

/// Video track metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoTrackInfo {
    pub track_id: u32,
    /// WebCodecs-compatible codec string (e.g. "avc1.64001F").
    pub codec_string: String,
    pub width: u32,
    pub height: u32,
    pub fps: Option<f64>,
    /// Codec-specific configuration (SPS/PPS for H264, etc.)
    #[serde(skip)]
    pub codec_config: Vec<u8>,
}

/// Audio track metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTrackInfo {
    pub track_id: u32,
    /// WebCodecs-compatible codec string (e.g. "mp4a.40.2", "opus").
    pub codec_string: String,
    pub sample_rate: u32,
    pub channels: u32,
    /// Codec-specific configuration.
    #[serde(skip)]
    pub codec_config: Vec<u8>,
}

/// Trait for container format demuxers.
pub trait Demuxer {
    /// Check if this demuxer can handle the given data.
    fn probe(data: &[u8]) -> bool
    where
        Self: Sized;

    /// Parse the container header and extract media information.
    fn parse_header(&mut self, data: &[u8]) -> Result<MediaInfo, DemuxError>;

    /// Get the next encoded chunk (video or audio sample).
    fn next_chunk(&mut self) -> Result<Option<EncodedChunk>, DemuxError>;

    /// Seek to the nearest keyframe before the given timestamp.
    fn seek_to_keyframe(&mut self, timestamp_us: i64) -> Result<(), DemuxError>;
}
