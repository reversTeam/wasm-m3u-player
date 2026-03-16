use serde::{Deserialize, Serialize};

/// Current playback status of the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackStatus {
    Idle,
    Loading,
    Ready,
    Playing,
    Paused,
    Buffering,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_state_default() {
        let state = PlayerState::default();
        assert_eq!(state.status, PlaybackStatus::Idle);
        assert_eq!(state.current_time_ms, 0);
        assert_eq!(state.duration_ms, None);
        assert_eq!(state.video_width, 0);
        assert_eq!(state.video_height, 0);
        assert!(state.media_info.is_none());
        assert!(!state.has_audio);
        assert!(!state.has_video);
        assert_eq!(state.buffered_ms, 0);
    }

    #[test]
    fn playback_status_serialization_roundtrip() {
        let statuses = vec![
            PlaybackStatus::Idle,
            PlaybackStatus::Loading,
            PlaybackStatus::Ready,
            PlaybackStatus::Playing,
            PlaybackStatus::Paused,
            PlaybackStatus::Buffering,
            PlaybackStatus::Stopped,
            PlaybackStatus::Seeking,
            PlaybackStatus::Error,
        ];
        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: PlaybackStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, status);
        }
    }

    #[test]
    fn player_state_serialization_roundtrip() {
        let state = PlayerState {
            status: PlaybackStatus::Playing,
            current_time_ms: 42000,
            duration_ms: Some(120000),
            video_width: 1920,
            video_height: 1080,
            media_info: Some(MediaInfo {
                duration_ms: Some(120000),
                video_codec: Some("avc1.640029".into()),
                audio_codec: Some("mp4a.40.2".into()),
                width: 1920,
                height: 1080,
                fps: Some(24.0),
                sample_rate: Some(44100),
                channels: Some(2),
            }),
            has_audio: true,
            has_video: true,
            buffered_ms: 60000,
        };
        let json = serde_json::to_string(&state).unwrap();
        let deserialized: PlayerState = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.status, PlaybackStatus::Playing);
        assert_eq!(deserialized.current_time_ms, 42000);
        assert_eq!(deserialized.duration_ms, Some(120000));
        assert_eq!(deserialized.video_width, 1920);
        assert!(deserialized.has_audio);
        assert!(deserialized.has_video);
    }

    #[test]
    fn media_info_optional_fields() {
        let info = MediaInfo {
            duration_ms: None,
            video_codec: None,
            audio_codec: None,
            width: 0,
            height: 0,
            fps: None,
            sample_rate: None,
            channels: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        let deserialized: MediaInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.duration_ms, None);
        assert_eq!(deserialized.width, 0);
    }

    #[test]
    fn playback_status_equality() {
        assert_eq!(PlaybackStatus::Idle, PlaybackStatus::Idle);
        assert_ne!(PlaybackStatus::Playing, PlaybackStatus::Paused);
        assert_ne!(PlaybackStatus::Seeking, PlaybackStatus::Buffering);
    }

    #[test]
    fn playback_status_copy() {
        let a = PlaybackStatus::Playing;
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn playback_status_debug() {
        // Ensure Debug impl works
        let s = format!("{:?}", PlaybackStatus::Seeking);
        assert_eq!(s, "Seeking");
    }
}
