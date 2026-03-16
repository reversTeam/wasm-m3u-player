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
    pub is_audio: bool,
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

/// A single entry in the seek index: maps a keyframe timestamp to its byte
/// offset in the file. Used for precise Range-based seeking.
#[derive(Debug, Clone, PartialEq)]
pub struct SeekEntry {
    /// Presentation timestamp of the keyframe in microseconds.
    pub timestamp_us: i64,
    /// Absolute byte offset in the file where this keyframe's data starts.
    /// For MP4: offset of the sample's chunk. For MKV: offset of the Cluster.
    pub byte_offset: u64,
}

/// Index of keyframe positions for fast seeking via HTTP Range requests.
///
/// Built from container metadata:
/// - MP4: stss (sync sample table) + stco/co64 (chunk offset table)
/// - MKV: Cues element (native seek index)
///
/// Entries are sorted by timestamp_us ascending.
#[derive(Debug, Clone)]
pub struct SeekIndex {
    pub entries: Vec<SeekEntry>,
}

impl SeekIndex {
    /// Create an empty seek index.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Create a seek index from a list of entries.
    /// Entries are sorted by timestamp on creation.
    pub fn from_entries(mut entries: Vec<SeekEntry>) -> Self {
        entries.sort_by_key(|e| e.timestamp_us);
        Self { entries }
    }

    /// Returns true if the index has any entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of entries in the index.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Find the keyframe entry at or just before the given timestamp.
    /// Returns None if the index is empty or all entries are after the target.
    ///
    /// Uses binary search for O(log n) lookup.
    pub fn lookup_keyframe(&self, timestamp_us: i64) -> Option<&SeekEntry> {
        if self.entries.is_empty() {
            return None;
        }

        // Binary search: find the rightmost entry with timestamp_us <= target
        match self.entries.binary_search_by_key(&timestamp_us, |e| e.timestamp_us) {
            // Exact match
            Ok(idx) => Some(&self.entries[idx]),
            // insertion point — the entry before it is the one we want
            Err(0) => None, // all entries are after target
            Err(idx) => Some(&self.entries[idx - 1]),
        }
    }

    /// Get the first entry (lowest timestamp).
    pub fn first(&self) -> Option<&SeekEntry> {
        self.entries.first()
    }

    /// Get the last entry (highest timestamp).
    pub fn last(&self) -> Option<&SeekEntry> {
        self.entries.last()
    }

    /// Merge entries from another SeekIndex, deduplicating by byte_offset.
    /// The result is sorted by byte_offset.
    pub fn merge(&mut self, other: SeekIndex) {
        if other.entries.is_empty() {
            return;
        }
        self.entries.extend(other.entries);
        self.entries.sort_by_key(|e| e.byte_offset);
        self.entries.dedup_by_key(|e| e.byte_offset);
    }
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

    /// Build a seek index mapping keyframe timestamps to byte offsets.
    ///
    /// Used for Range-based seeking: the byte offset allows fetching
    /// exactly the right portion of the file via HTTP Range requests.
    ///
    /// Must be called after `parse_header()`. Returns an empty index
    /// if the container metadata doesn't support it.
    fn build_seek_index(&self) -> SeekIndex;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entries(timestamps_offsets: &[(i64, u64)]) -> Vec<SeekEntry> {
        timestamps_offsets
            .iter()
            .map(|&(ts, off)| SeekEntry {
                timestamp_us: ts,
                byte_offset: off,
            })
            .collect()
    }

    // =============================================
    // SeekIndex construction
    // =============================================

    #[test]
    fn seek_index_empty() {
        let idx = SeekIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert!(idx.first().is_none());
        assert!(idx.last().is_none());
    }

    #[test]
    fn seek_index_from_entries_sorts() {
        // Out of order entries should be sorted by timestamp
        let entries = make_entries(&[(3_000_000, 300), (1_000_000, 100), (2_000_000, 200)]);
        let idx = SeekIndex::from_entries(entries);
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.entries[0].timestamp_us, 1_000_000);
        assert_eq!(idx.entries[1].timestamp_us, 2_000_000);
        assert_eq!(idx.entries[2].timestamp_us, 3_000_000);
    }

    #[test]
    fn seek_index_first_last() {
        let entries = make_entries(&[(1_000_000, 100), (5_000_000, 500)]);
        let idx = SeekIndex::from_entries(entries);
        assert_eq!(idx.first().unwrap().timestamp_us, 1_000_000);
        assert_eq!(idx.last().unwrap().timestamp_us, 5_000_000);
    }

    // =============================================
    // SeekIndex::lookup_keyframe
    // =============================================

    #[test]
    fn lookup_empty_index() {
        let idx = SeekIndex::new();
        assert!(idx.lookup_keyframe(0).is_none());
        assert!(idx.lookup_keyframe(1_000_000).is_none());
    }

    #[test]
    fn lookup_single_entry_exact() {
        let entries = make_entries(&[(2_000_000, 200)]);
        let idx = SeekIndex::from_entries(entries);
        let result = idx.lookup_keyframe(2_000_000).unwrap();
        assert_eq!(result.timestamp_us, 2_000_000);
        assert_eq!(result.byte_offset, 200);
    }

    #[test]
    fn lookup_single_entry_before() {
        let entries = make_entries(&[(2_000_000, 200)]);
        let idx = SeekIndex::from_entries(entries);
        // Before the only entry → None
        assert!(idx.lookup_keyframe(1_000_000).is_none());
    }

    #[test]
    fn lookup_single_entry_after() {
        let entries = make_entries(&[(2_000_000, 200)]);
        let idx = SeekIndex::from_entries(entries);
        // After the only entry → returns it
        let result = idx.lookup_keyframe(5_000_000).unwrap();
        assert_eq!(result.timestamp_us, 2_000_000);
    }

    #[test]
    fn lookup_exact_match() {
        let entries = make_entries(&[
            (0, 0),
            (2_000_000, 200),
            (4_000_000, 400),
            (6_000_000, 600),
        ]);
        let idx = SeekIndex::from_entries(entries);
        let result = idx.lookup_keyframe(4_000_000).unwrap();
        assert_eq!(result.timestamp_us, 4_000_000);
        assert_eq!(result.byte_offset, 400);
    }

    #[test]
    fn lookup_between_entries_returns_previous() {
        let entries = make_entries(&[
            (0, 0),
            (2_000_000, 200),
            (4_000_000, 400),
            (6_000_000, 600),
        ]);
        let idx = SeekIndex::from_entries(entries);

        // Between 2s and 4s → returns 2s entry
        let result = idx.lookup_keyframe(3_000_000).unwrap();
        assert_eq!(result.timestamp_us, 2_000_000);
        assert_eq!(result.byte_offset, 200);
    }

    #[test]
    fn lookup_before_first_entry() {
        let entries = make_entries(&[(1_000_000, 100), (3_000_000, 300)]);
        let idx = SeekIndex::from_entries(entries);
        assert!(idx.lookup_keyframe(500_000).is_none());
    }

    #[test]
    fn lookup_at_zero() {
        let entries = make_entries(&[(0, 0), (2_000_000, 200)]);
        let idx = SeekIndex::from_entries(entries);
        let result = idx.lookup_keyframe(0).unwrap();
        assert_eq!(result.timestamp_us, 0);
        assert_eq!(result.byte_offset, 0);
    }

    #[test]
    fn lookup_beyond_last_entry() {
        let entries = make_entries(&[(0, 0), (2_000_000, 200), (4_000_000, 400)]);
        let idx = SeekIndex::from_entries(entries);
        // Way past last entry → returns last
        let result = idx.lookup_keyframe(100_000_000).unwrap();
        assert_eq!(result.timestamp_us, 4_000_000);
    }

    #[test]
    fn lookup_just_before_second_entry() {
        let entries = make_entries(&[(0, 0), (2_000_000, 200)]);
        let idx = SeekIndex::from_entries(entries);
        let result = idx.lookup_keyframe(1_999_999).unwrap();
        assert_eq!(result.timestamp_us, 0);
    }

    #[test]
    fn lookup_negative_timestamp() {
        let entries = make_entries(&[(0, 0), (2_000_000, 200)]);
        let idx = SeekIndex::from_entries(entries);
        assert!(idx.lookup_keyframe(-1).is_none());
    }

    #[test]
    fn lookup_many_entries() {
        // 100 keyframes every 2 seconds
        let entries: Vec<SeekEntry> = (0..100)
            .map(|i| SeekEntry {
                timestamp_us: i * 2_000_000,
                byte_offset: i as u64 * 50_000,
            })
            .collect();
        let idx = SeekIndex::from_entries(entries);
        assert_eq!(idx.len(), 100);

        // Seek to 55s → should return 54s keyframe (entry 27)
        let result = idx.lookup_keyframe(55_000_000).unwrap();
        assert_eq!(result.timestamp_us, 54_000_000);
        assert_eq!(result.byte_offset, 27 * 50_000);
    }

    #[test]
    fn seek_entry_equality() {
        let a = SeekEntry { timestamp_us: 100, byte_offset: 200 };
        let b = SeekEntry { timestamp_us: 100, byte_offset: 200 };
        let c = SeekEntry { timestamp_us: 100, byte_offset: 300 };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
