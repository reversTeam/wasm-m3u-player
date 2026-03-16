use serde::{Deserialize, Serialize};

use crate::types::{MediaInfo, PlaybackStatus};

/// Events emitted by the player via on_event callback.
/// Uses serde tagged union for clean JS interop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PlayerEvent {
    MediaLoaded {
        info: MediaInfo,
    },
    StatusChanged {
        status: PlaybackStatus,
    },
    TimeUpdate {
        current_ms: u64,
    },
    Seeking {
        target_ms: u64,
    },
    Seeked {
        actual_ms: u64,
    },
    BufferUpdate {
        buffered_ms: u64,
    },
    DownloadProgress {
        received_bytes: u64,
        total_bytes: u64,
    },
    Error {
        message: String,
        recoverable: bool,
    },
    VideoResized {
        width: u32,
        height: u32,
    },
    PlaylistTrackChanged {
        index: usize,
    },
    SyncStats {
        rendered: u64,
        dropped: u64,
        held: u64,
        skipped: u64,
    },
    Ended,
}

#[cfg(test)]
mod tests {
    use super::*;

    // =============================================
    // PlayerEvent serialization roundtrip tests
    // =============================================

    #[test]
    fn serialize_media_loaded() {
        let event = PlayerEvent::MediaLoaded {
            info: MediaInfo {
                duration_ms: Some(120000),
                video_codec: Some("avc1.640029".into()),
                audio_codec: Some("mp4a.40.2".into()),
                width: 1920,
                height: 1080,
                fps: Some(24.0),
                sample_rate: Some(44100),
                channels: Some(2),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"MediaLoaded\""));
        assert!(json.contains("\"width\":1920"));
        assert!(json.contains("\"avc1.640029\""));

        let deserialized: PlayerEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlayerEvent::MediaLoaded { info } => {
                assert_eq!(info.width, 1920);
                assert_eq!(info.height, 1080);
                assert_eq!(info.duration_ms, Some(120000));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn serialize_status_changed() {
        let event = PlayerEvent::StatusChanged {
            status: PlaybackStatus::Playing,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"StatusChanged\""));
        assert!(json.contains("\"Playing\""));

        let deserialized: PlayerEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlayerEvent::StatusChanged { status } => assert_eq!(status, PlaybackStatus::Playing),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn serialize_time_update() {
        let event = PlayerEvent::TimeUpdate { current_ms: 42000 };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("42000"));
        let _: PlayerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn serialize_seeking() {
        let event = PlayerEvent::Seeking { target_ms: 60000 };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"Seeking\""));
        let _: PlayerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn serialize_seeked() {
        let event = PlayerEvent::Seeked { actual_ms: 59500 };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"Seeked\""));
        let _: PlayerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn serialize_buffer_update() {
        let event = PlayerEvent::BufferUpdate { buffered_ms: 5000 };
        let json = serde_json::to_string(&event).unwrap();
        let _: PlayerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn serialize_download_progress() {
        let event = PlayerEvent::DownloadProgress {
            received_bytes: 1_000_000,
            total_bytes: 10_000_000,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("1000000"));
        assert!(json.contains("10000000"));
        let _: PlayerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn serialize_error_recoverable() {
        let event = PlayerEvent::Error {
            message: "Network timeout".into(),
            recoverable: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"recoverable\":true"));
        let _: PlayerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn serialize_error_fatal() {
        let event = PlayerEvent::Error {
            message: "Unsupported codec".into(),
            recoverable: false,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"recoverable\":false"));
        let _: PlayerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn serialize_video_resized() {
        let event = PlayerEvent::VideoResized {
            width: 3840,
            height: 2160,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("3840"));
        let _: PlayerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn serialize_playlist_track_changed() {
        let event = PlayerEvent::PlaylistTrackChanged { index: 5 };
        let json = serde_json::to_string(&event).unwrap();
        let _: PlayerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn serialize_sync_stats() {
        let event = PlayerEvent::SyncStats {
            rendered: 1000,
            dropped: 50,
            held: 30,
            skipped: 2,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"SyncStats\""));
        assert!(json.contains("\"rendered\":1000"));

        let deserialized: PlayerEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlayerEvent::SyncStats {
                rendered,
                dropped,
                held,
                skipped,
            } => {
                assert_eq!(rendered, 1000);
                assert_eq!(dropped, 50);
                assert_eq!(held, 30);
                assert_eq!(skipped, 2);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn serialize_ended() {
        let event = PlayerEvent::Ended;
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"Ended\""));
        let _: PlayerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn serialize_all_statuses() {
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
            let event = PlayerEvent::StatusChanged { status };
            let json = serde_json::to_string(&event).unwrap();
            let deserialized: PlayerEvent = serde_json::from_str(&json).unwrap();
            match deserialized {
                PlayerEvent::StatusChanged { status: s } => assert_eq!(s, status),
                _ => panic!("Roundtrip failed for {:?}", status),
            }
        }
    }

    // =============================================
    // Edge cases
    // =============================================

    #[test]
    fn serialize_media_info_no_optional_fields() {
        let event = PlayerEvent::MediaLoaded {
            info: MediaInfo {
                duration_ms: None,
                video_codec: None,
                audio_codec: None,
                width: 0,
                height: 0,
                fps: None,
                sample_rate: None,
                channels: None,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: PlayerEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlayerEvent::MediaLoaded { info } => {
                assert_eq!(info.duration_ms, None);
                assert_eq!(info.video_codec, None);
                assert_eq!(info.width, 0);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn serialize_error_with_special_characters() {
        let event = PlayerEvent::Error {
            message: "Error: \"quotes\" & <brackets> 中文".into(),
            recoverable: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: PlayerEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlayerEvent::Error { message, .. } => {
                assert!(message.contains("quotes"));
                assert!(message.contains("中文"));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn serialize_max_u64_values() {
        let event = PlayerEvent::TimeUpdate {
            current_ms: u64::MAX,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: PlayerEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            PlayerEvent::TimeUpdate { current_ms } => assert_eq!(current_ms, u64::MAX),
            _ => panic!("Wrong variant"),
        }
    }
}
