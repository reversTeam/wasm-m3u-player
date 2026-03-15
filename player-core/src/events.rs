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
    Ended,
}
