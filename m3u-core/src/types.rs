use serde::{Deserialize, Serialize};

/// A parsed M3U playlist containing video entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    pub entries: Vec<PlaylistEntry>,
}

/// A single entry in an M3U playlist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistEntry {
    pub url: String,
    pub title: Option<String>,
    pub duration_secs: Option<f64>,
}
