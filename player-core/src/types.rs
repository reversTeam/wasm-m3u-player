use serde::{Deserialize, Serialize};

/// Current playback status of the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    Idle,
    Loading,
    Ready,
    Playing,
    Paused,
    Stopped,
    Seeking,
    Error,
}

/// Snapshot of the player state, serializable to JS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerState {
    pub status: PlaybackStatus,
    pub current_time_ms: u64,
    pub duration_ms: Option<u64>,
    pub video_width: u32,
    pub video_height: u32,
    pub media_info: Option<MediaInfo>,
    pub has_audio: bool,
    pub has_video: bool,
    pub buffered_ms: u64,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Idle,
            current_time_ms: 0,
            duration_ms: None,
            video_width: 0,
            video_height: 0,
            media_info: None,
            has_audio: false,
            has_video: false,
            buffered_ms: 0,
        }
    }
}

/// High-level media information exposed to consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub duration_ms: Option<u64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: Option<f64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
}
