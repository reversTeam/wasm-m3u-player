use std::cell::RefCell;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

/// Configuration for streaming buffer management.
/// Exposed to JS so consumers can tune buffering behavior.
#[wasm_bindgen]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferConfig {
    /// Maximum chunks to feed to decoders per render_tick. Default: 8
    pub decode_batch_size: usize,
    /// Maximum download rate in bytes/sec. 0 = unlimited. Default: 0
    pub max_download_rate: u64,
    /// Pause download when decoded video frame queue exceeds this. Default: 120
    pub max_video_queue: usize,
    /// Resume download when video frame queue drops below this. Default: 30
    pub resume_video_queue: usize,
    /// Minimum demuxed chunk queue size before trying to demux more. Default: 24
    pub min_chunk_queue: usize,
    /// Number of chunks to demux in one batch. Default: 32
    pub demux_batch_size: usize,
}

#[wasm_bindgen]
impl BufferConfig {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            decode_batch_size: 32,
            max_download_rate: 0,
            max_video_queue: 120,
            resume_video_queue: 30,
            min_chunk_queue: 128,
            demux_batch_size: 128,
        }
    }
}

/// Shared download state — written by background download task, read by player.
pub struct SharedDownload {
    pub data: Vec<u8>,
    pub content_length: u64,
    pub complete: bool,
    pub error: Option<String>,
    /// Back-pressure flag: signals download task to pause.
    pub paused: bool,
    /// Cancellation flag: signals background download task to stop permanently.
    pub cancelled: bool,
}

impl SharedDownload {
    pub fn new() -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            data: Vec::new(),
            content_length: 0,
            complete: false,
            error: None,
            paused: false,
            cancelled: false,
        }))
    }
}

/// Shared state for background Range prefetch tasks.
///
/// The `render_tick()` synchronous method can't do async fetches, so it
/// spawns a `spawn_local` task that writes fetched data here. On the next
/// tick, the player drains `pending_data` into both `SharedDownload` and
/// `RangeBuffer`.
pub struct PrefetchState {
    /// Data fetched by background task, waiting to be drained into player buffers.
    /// Each entry is (byte_offset, data).
    pub pending_data: Vec<(u64, Vec<u8>)>,
    /// Whether a prefetch is currently in flight (prevents duplicate fetches).
    pub in_flight: bool,
    /// Cancellation flag.
    pub cancelled: bool,
}

impl PrefetchState {
    pub fn new() -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            pending_data: Vec::new(),
            in_flight: false,
            cancelled: false,
        }))
    }
}

/// A non-contiguous buffer for Range-based streaming.
///
/// Stores data in sorted, non-overlapping segments. Segments are merged
/// when they are adjacent or overlapping. Old segments are evicted when
/// total memory exceeds `max_bytes`.
///
/// This replaces the linear `SharedDownload` for Range-first playback:
/// instead of downloading the entire file, we fetch windows on demand
/// and keep only what's needed in memory.
pub struct RangeBuffer {
    /// Sorted, non-overlapping segments: (byte_offset, data).
    segments: Vec<(u64, Vec<u8>)>,
    /// Total file size (from HEAD request).
    pub file_size: u64,
    /// Maximum total bytes before eviction. Default: 64MB.
    pub max_bytes: usize,
}

impl RangeBuffer {
    /// Create a new empty RangeBuffer.
    pub fn new(file_size: u64) -> Self {
        Self {
            segments: Vec::new(),
            file_size,
            max_bytes: 64 * 1024 * 1024, // 64MB default
        }
    }

    /// Total bytes currently stored across all segments.
    pub fn total_bytes(&self) -> usize {
        self.segments.iter().map(|(_, d)| d.len()).sum()
    }

    /// Number of segments.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Check if the buffer contains data for the entire range [start, end).
    pub fn has_range(&self, start: u64, end: u64) -> bool {
        if start >= end {
            return true;
        }
        for (seg_offset, seg_data) in &self.segments {
            let seg_end = *seg_offset + seg_data.len() as u64;
            if *seg_offset <= start && seg_end >= end {
                return true;
            }
        }
        false
    }

    /// Get contiguous data starting at `offset`.
    /// Returns as much contiguous data as available from that offset,
    /// or None if no segment covers that offset.
    pub fn get_contiguous_from(&self, offset: u64) -> Option<&[u8]> {
        for (seg_offset, seg_data) in &self.segments {
            let seg_end = *seg_offset + seg_data.len() as u64;
            if *seg_offset <= offset && offset < seg_end {
                let start_within = (offset - *seg_offset) as usize;
                return Some(&seg_data[start_within..]);
            }
        }
        None
    }

    /// Get data for the exact range [start, end).
    /// Returns None if any part of the range is missing.
    pub fn get_range(&self, start: u64, end: u64) -> Option<Vec<u8>> {
        if start >= end {
            return Some(Vec::new());
        }
        for (seg_offset, seg_data) in &self.segments {
            let seg_end = *seg_offset + seg_data.len() as u64;
            if *seg_offset <= start && seg_end >= end {
                let s = (start - *seg_offset) as usize;
                let e = (end - *seg_offset) as usize;
                return Some(seg_data[s..e].to_vec());
            }
        }
        None
    }

    /// Insert a data segment. Merges with adjacent/overlapping segments.
    pub fn insert(&mut self, offset: u64, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }

        // Find insertion point (segments are sorted by offset)
        let insert_idx = self
            .segments
            .binary_search_by_key(&offset, |(o, _)| *o)
            .unwrap_or_else(|i| i);

        self.segments.insert(insert_idx, (offset, data));

        // Merge overlapping/adjacent segments
        self.merge_segments();

        // Evict if over memory limit
        self.evict_if_needed();
    }

    /// Merge overlapping or adjacent segments.
    fn merge_segments(&mut self) {
        if self.segments.len() <= 1 {
            return;
        }

        let mut merged: Vec<(u64, Vec<u8>)> = Vec::with_capacity(self.segments.len());

        for (offset, data) in self.segments.drain(..) {
            if let Some((last_offset, last_data)) = merged.last_mut() {
                let last_end = *last_offset + last_data.len() as u64;

                if offset <= last_end {
                    // Overlapping or adjacent — merge
                    let new_end = offset + data.len() as u64;
                    if new_end > last_end {
                        // Extend the last segment
                        let overlap_start = (last_end - offset) as usize;
                        if overlap_start < data.len() {
                            last_data.extend_from_slice(&data[overlap_start..]);
                        }
                    }
                    // If new_end <= last_end, the new segment is fully contained — skip
                } else {
                    // Gap — push as new segment
                    merged.push((offset, data));
                }
            } else {
                merged.push((offset, data));
            }
        }

        self.segments = merged;
    }

    /// Evict oldest segments (lowest offsets) when total exceeds max_bytes.
    /// Keeps the most recently inserted segments (highest offsets).
    fn evict_if_needed(&mut self) {
        while self.total_bytes() > self.max_bytes && self.segments.len() > 1 {
            self.segments.remove(0);
        }
    }

    /// How many contiguous bytes are available starting at `offset`.
    pub fn contiguous_bytes_from(&self, offset: u64) -> u64 {
        for (seg_offset, seg_data) in &self.segments {
            let seg_end = *seg_offset + seg_data.len() as u64;
            if *seg_offset <= offset && offset < seg_end {
                return seg_end - offset;
            }
        }
        0
    }

    /// Clear all segments.
    pub fn clear(&mut self) {
        self.segments.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =============================================
    // RangeBuffer construction
    // =============================================

    #[test]
    fn new_buffer_is_empty() {
        let buf = RangeBuffer::new(1000);
        assert_eq!(buf.total_bytes(), 0);
        assert_eq!(buf.segment_count(), 0);
        assert_eq!(buf.file_size, 1000);
    }

    // =============================================
    // insert + has_range
    // =============================================

    #[test]
    fn insert_single_segment() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(0, vec![1, 2, 3, 4, 5]);
        assert_eq!(buf.total_bytes(), 5);
        assert_eq!(buf.segment_count(), 1);
        assert!(buf.has_range(0, 5));
        assert!(!buf.has_range(0, 6));
    }

    #[test]
    fn insert_non_overlapping_segments() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(0, vec![1, 2, 3]);
        buf.insert(10, vec![4, 5, 6]);
        assert_eq!(buf.segment_count(), 2);
        assert!(buf.has_range(0, 3));
        assert!(buf.has_range(10, 13));
        assert!(!buf.has_range(0, 13)); // gap between 3 and 10
    }

    #[test]
    fn insert_adjacent_segments_merge() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(0, vec![1, 2, 3]);
        buf.insert(3, vec![4, 5, 6]);
        assert_eq!(buf.segment_count(), 1);
        assert_eq!(buf.total_bytes(), 6);
        assert!(buf.has_range(0, 6));
    }

    #[test]
    fn insert_overlapping_segments_merge() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(0, vec![1, 2, 3, 4, 5]);
        buf.insert(3, vec![6, 7, 8]);
        assert_eq!(buf.segment_count(), 1);
        assert_eq!(buf.total_bytes(), 6); // 0..6
        assert!(buf.has_range(0, 6));
    }

    #[test]
    fn insert_fully_contained_segment() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(0, vec![1, 2, 3, 4, 5]);
        buf.insert(1, vec![9, 9]); // fully inside [0..5]
        assert_eq!(buf.segment_count(), 1);
        assert_eq!(buf.total_bytes(), 5); // no growth
    }

    #[test]
    fn insert_three_segments_merge_cascade() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(0, vec![1, 2, 3]);
        buf.insert(6, vec![7, 8, 9]);
        assert_eq!(buf.segment_count(), 2);
        // Insert bridging segment
        buf.insert(3, vec![4, 5, 6]);
        assert_eq!(buf.segment_count(), 1);
        assert_eq!(buf.total_bytes(), 9);
        assert!(buf.has_range(0, 9));
    }

    #[test]
    fn insert_empty_data_noop() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(0, vec![]);
        assert_eq!(buf.segment_count(), 0);
    }

    // =============================================
    // get_contiguous_from
    // =============================================

    #[test]
    fn get_contiguous_from_start() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(10, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        let data = buf.get_contiguous_from(10).unwrap();
        assert_eq!(data, &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn get_contiguous_from_middle() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(10, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        let data = buf.get_contiguous_from(12).unwrap();
        assert_eq!(data, &[0xCC, 0xDD]);
    }

    #[test]
    fn get_contiguous_from_no_data() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(10, vec![0xAA, 0xBB]);
        assert!(buf.get_contiguous_from(5).is_none());
        assert!(buf.get_contiguous_from(12).is_none());
    }

    // =============================================
    // get_range
    // =============================================

    #[test]
    fn get_range_exact() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(100, vec![1, 2, 3, 4, 5]);
        let data = buf.get_range(101, 104).unwrap();
        assert_eq!(data, vec![2, 3, 4]);
    }

    #[test]
    fn get_range_missing() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(100, vec![1, 2, 3]);
        assert!(buf.get_range(100, 110).is_none()); // extends beyond segment
    }

    #[test]
    fn get_range_empty() {
        let buf = RangeBuffer::new(1000);
        let data = buf.get_range(0, 0).unwrap();
        assert!(data.is_empty());
    }

    // =============================================
    // contiguous_bytes_from
    // =============================================

    #[test]
    fn contiguous_bytes_from_start() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(0, vec![0; 100]);
        assert_eq!(buf.contiguous_bytes_from(0), 100);
        assert_eq!(buf.contiguous_bytes_from(50), 50);
        assert_eq!(buf.contiguous_bytes_from(100), 0);
    }

    #[test]
    fn contiguous_bytes_from_gap() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(100, vec![0; 50]);
        assert_eq!(buf.contiguous_bytes_from(0), 0);
        assert_eq!(buf.contiguous_bytes_from(100), 50);
    }

    // =============================================
    // eviction
    // =============================================

    #[test]
    fn eviction_removes_oldest_segments() {
        let mut buf = RangeBuffer::new(10000);
        buf.max_bytes = 100; // very small limit

        // Insert segments totaling 150 bytes
        buf.insert(0, vec![0; 50]);
        buf.insert(100, vec![0; 50]);
        buf.insert(200, vec![0; 50]);

        // After eviction, oldest segment should be removed
        assert!(buf.total_bytes() <= 100);
        // The first segment (offset=0) should have been evicted
        assert!(buf.get_contiguous_from(0).is_none());
        // Latest segments should still be there
        assert!(buf.get_contiguous_from(200).is_some());
    }

    // =============================================
    // clear
    // =============================================

    #[test]
    fn clear_removes_everything() {
        let mut buf = RangeBuffer::new(1000);
        buf.insert(0, vec![1, 2, 3]);
        buf.insert(100, vec![4, 5, 6]);
        buf.clear();
        assert_eq!(buf.total_bytes(), 0);
        assert_eq!(buf.segment_count(), 0);
    }

    // =============================================
    // has_range edge cases
    // =============================================

    #[test]
    fn has_range_start_equals_end() {
        let buf = RangeBuffer::new(1000);
        assert!(buf.has_range(5, 5)); // empty range
    }

    #[test]
    fn has_range_start_greater_than_end() {
        let buf = RangeBuffer::new(1000);
        assert!(buf.has_range(10, 5)); // inverted range = empty
    }
}
