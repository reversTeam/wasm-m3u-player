use std::io::Cursor;

use bytes::Bytes;
use matroska_demuxer::{Frame, MatroskaFile, TrackType};

use crate::types::*;

/// Find the byte offset of the first valid MKV Cluster element (ID: 0x1F43B675).
/// Validates that the Cluster ID is followed by a plausible VINT size to reduce
/// false positives (the 4-byte sequence can appear in compressed frame data).
pub fn find_cluster_offset(data: &[u8]) -> Option<usize> {
    const CLUSTER_ID: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
    let mut pos = 0;
    while pos + 4 < data.len() {
        if let Some(rel) = data[pos..].windows(4).position(|w| w == CLUSTER_ID) {
            let abs = pos + rel;
            // Validate: byte after Cluster ID must be a valid VINT leading byte.
            // VINT leading byte has the width encoded in the leading 1-bit position:
            // 1xxxxxxx = 1 byte, 01xxxxxx = 2 bytes, etc. A zero byte is never valid.
            if abs + 4 < data.len() {
                let vint_first = data[abs + 4];
                if vint_first != 0 {
                    return Some(abs);
                }
            } else {
                // Not enough data after Cluster ID to validate — accept it anyway
                // since we're at the edge of available data
                return Some(abs);
            }
            // False positive — continue searching after this position
            pos = abs + 1;
        } else {
            break;
        }
    }
    None
}

/// Parse an EBML variable-length integer (VINT) from the start of `data`.
/// Returns `(value, bytes_consumed)` or None if data is too short.
///
/// VINT encoding: the number of leading zero bits determines the byte width.
/// - 1xxxxxxx = 1 byte (7 data bits)
/// - 01xxxxxx xxxxxxxx = 2 bytes (14 data bits)
/// - 001xxxxx xxxxxxxx xxxxxxxx = 3 bytes (21 data bits)
/// - etc.
fn parse_ebml_vint(data: &[u8]) -> Option<(u64, usize)> {
    if data.is_empty() {
        return None;
    }
    let first = data[0];
    if first == 0 {
        return None;
    }

    let width = first.leading_zeros() as usize + 1;
    if width > 8 || data.len() < width {
        return None;
    }

    // Strip the VINT_MARKER bit and read the value
    let mut value = (first & (0xFF >> width)) as u64;
    for i in 1..width {
        value = (value << 8) | data[i] as u64;
    }

    Some((value, width))
}

/// Find the Timestamp element (ID 0xE7) within a Cluster's content and return its value.
/// The Timestamp is typically the first child element of a Cluster.
fn find_cluster_timestamp(data: &[u8]) -> Option<u64> {
    let mut pos = 0;
    // Search within available data for Timestamp element ID (0xE7)
    while pos < data.len() {
        if data[pos] == 0xE7 {
            // Found Timestamp element ID (1-byte ID)
            let size_pos = pos + 1;
            if size_pos >= data.len() {
                return None;
            }
            let (size, vint_len) = parse_ebml_vint(&data[size_pos..])?;
            let value_start = size_pos + vint_len;
            let value_end = value_start + size as usize;
            if value_end > data.len() {
                return None;
            }
            // Read unsigned integer value (big-endian)
            let mut value: u64 = 0;
            for &byte in &data[value_start..value_end] {
                value = (value << 8) | byte as u64;
            }
            return Some(value);
        }
        // Skip unknown element: read its ID + size to advance
        let (_, id_len) = parse_ebml_vint(&data[pos..])?;
        let size_pos = pos + id_len;
        if size_pos >= data.len() {
            return None;
        }
        let (elem_size, size_len) = parse_ebml_vint(&data[size_pos..])?;
        pos = size_pos + size_len + elem_size as usize;
    }
    None
}

/// MKV/WebM demuxer using the `matroska-demuxer` crate.
pub struct MkvDemuxer {
    mkv: Option<MatroskaFile<Cursor<Vec<u8>>>>,
    media_info: Option<MediaInfo>,
    video_track_ids: Vec<u64>,
    audio_track_ids: Vec<u64>,
    /// Number of frames read so far (for resume after re-creation).
    frames_read: usize,
    /// Pre-computed seek index from Cluster scanning.
    seek_index: SeekIndex,
    /// TimestampScale in nanoseconds (default 1_000_000 = 1ms per tick).
    /// Used to convert frame.timestamp (ticks) to microseconds.
    timestamp_scale_ns: u64,
    /// Byte offset up to which scan_clusters has already scanned.
    /// Used for incremental scanning: only scan data[last_scanned_offset..] on rebuild.
    last_scanned_offset: usize,
    /// Raw buffer used during parse_header, kept for seek rewind.
    /// MatroskaFile has no rewind/into_inner, so we store the data
    /// to re-create the MatroskaFile when seeking.
    /// Stored as `Bytes` so that cloning (e.g. in seek_to_keyframe) is O(1)
    /// via atomic refcount instead of a full buffer copy.
    raw_data: Option<Bytes>,
}

impl MkvDemuxer {
    pub fn new() -> Self {
        Self {
            mkv: None,
            media_info: None,
            video_track_ids: Vec::new(),
            audio_track_ids: Vec::new(),
            frames_read: 0,
            seek_index: SeekIndex::new(),
            timestamp_scale_ns: 1_000_000, // default: 1ms per tick
            last_scanned_offset: 0,
            raw_data: None,
        }
    }

    /// Map Matroska codec ID to WebCodecs-compatible codec string.
    fn map_video_codec(codec_id: &str, codec_private: &Option<Vec<u8>>) -> String {
        match codec_id {
            "V_MPEG4/ISO/AVC" => {
                // Try to extract profile from codec_private (avcC box)
                if let Some(data) = codec_private {
                    if data.len() >= 4 {
                        return format!("avc1.{:02X}{:02X}{:02X}", data[1], data[2], data[3]);
                    }
                }
                "avc1.640029".to_string()
            }
            "V_MPEGH/ISO/HEVC" => "hvc1.1.6.L93.B0".to_string(),
            "V_VP8" => "vp8".to_string(),
            "V_VP9" => "vp09.00.10.08".to_string(),
            "V_AV1" => "av01.0.01M.08".to_string(),
            _ => format!("unknown:{}", codec_id),
        }
    }

    /// Get the number of frames read so far (for resume tracking).
    pub fn frames_read(&self) -> usize {
        self.frames_read
    }

    /// Get the pre-computed seek index (Cluster boundary offsets + timestamps).
    pub fn get_seek_index(&self) -> &SeekIndex {
        &self.seek_index
    }

    /// Transfer the seek index and scan offset from a previous demuxer instance.
    /// Used during rebuild to avoid re-scanning already-known Cluster entries.
    pub fn transfer_seek_state(&mut self, seek_index: SeekIndex, last_scanned_offset: usize) {
        self.seek_index = seek_index;
        self.last_scanned_offset = last_scanned_offset;
    }

    /// Get the byte offset up to which Clusters have been scanned.
    pub fn last_scanned_offset(&self) -> usize {
        self.last_scanned_offset
    }

    /// Find the last Cluster boundary (byte offset + timestamp_us) that starts
    /// at or before the given byte offset. Uses the seek index built during parse_header.
    ///
    /// Returns `None` if the seek index is empty or all entries are past the offset.
    pub fn find_cluster_before_offset(&self, byte_offset: u64) -> Option<&SeekEntry> {
        if self.seek_index.entries.is_empty() {
            return None;
        }
        // Binary search for the last entry with byte_offset <= target
        let mut best: Option<&SeekEntry> = None;
        for entry in &self.seek_index.entries {
            if entry.byte_offset <= byte_offset {
                best = Some(entry);
            } else {
                break; // entries are sorted by timestamp, but clusters are sequential
                       // so byte offsets are also monotonically increasing
            }
        }
        best
    }

    /// Skip N frames (used after re-creating demuxer to resume position).
    pub fn skip_frames(&mut self, count: usize) -> Result<(), DemuxError> {
        let mkv = self
            .mkv
            .as_mut()
            .ok_or_else(|| DemuxError::InvalidData("No header parsed yet".into()))?;
        let mut frame = Frame::default();
        for _ in 0..count {
            match mkv.next_frame(&mut frame) {
                Ok(true) => self.frames_read += 1,
                Ok(false) => break,
                Err(e) => return Err(DemuxError::InvalidData(format!("Skip error: {}", e))),
            }
        }
        Ok(())
    }

    /// Get the timestamp scale in nanoseconds (default 1_000_000 = 1ms per tick).
    /// Available after `parse_header` has been called.
    pub fn timestamp_scale_ns(&self) -> u64 {
        self.timestamp_scale_ns
    }

    /// Scan raw data for MKV Cluster elements and extract their timestamps + offsets.
    /// `start_offset` allows incremental scanning: only scan data[start_offset..].
    /// The returned SeekIndex contains entries with ABSOLUTE byte offsets in `data`.
    ///
    /// Each Cluster starts with ID 0x1F43B675, followed by a VINT size,
    /// then contains a Timestamp element (ID 0xE7) with the cluster's
    /// timestamp in TimestampScale units.
    pub fn scan_clusters_for_seek_index(
        data: &[u8],
        timestamp_scale_ns: u64,
        start_offset: usize,
    ) -> SeekIndex {
        let mut entries = Vec::new();
        let mut pos = start_offset;

        while pos + 4 < data.len() {
            // Search for Cluster ID: 0x1F43B675
            let remaining = &data[pos..];
            let cluster_rel = remaining
                .windows(4)
                .position(|w| w == [0x1F, 0x43, 0xB6, 0x75]);

            let cluster_start = match cluster_rel {
                Some(rel) => pos + rel,
                None => break,
            };

            // Parse VINT size after Cluster ID
            let after_id = cluster_start + 4;
            if after_id >= data.len() {
                break;
            }

            let (cluster_size, vint_len) = match parse_ebml_vint(&data[after_id..]) {
                Some(v) => v,
                None => {
                    pos = cluster_start + 1;
                    continue;
                }
            };

            // Validate VINT — reject obvious false positives
            if vint_len == 0 || cluster_size == 0 {
                pos = cluster_start + 1;
                continue;
            }

            // Look for Timestamp element (ID 0xE7) in the first ~32 bytes of cluster content
            let content_start = after_id + vint_len;
            let search_end = (content_start + 32).min(data.len());

            if content_start < search_end {
                if let Some(timestamp) = find_cluster_timestamp(&data[content_start..search_end]) {
                    // Convert from TimestampScale units to microseconds
                    let timestamp_us = (timestamp as i64 * timestamp_scale_ns as i64) / 1_000;

                    entries.push(SeekEntry {
                        timestamp_us,
                        byte_offset: cluster_start as u64,
                    });
                }
            }

            // Advance past this cluster
            let next_pos = if cluster_size < u64::MAX - after_id as u64 - vint_len as u64 {
                content_start as u64 + cluster_size
            } else {
                break;
            };
            pos = next_pos as usize;
        }

        SeekIndex::from_entries(entries)
    }

    fn map_audio_codec(codec_id: &str) -> String {
        match codec_id {
            "A_AAC" | "A_AAC/MPEG2/LC" | "A_AAC/MPEG4/LC" => "mp4a.40.2".to_string(),
            "A_AAC/MPEG4/SBR" => "mp4a.40.5".to_string(),
            "A_OPUS" => "opus".to_string(),
            "A_VORBIS" => "vorbis".to_string(),
            "A_FLAC" => "flac".to_string(),
            "A_AC3" => "ac-3".to_string(),
            "A_EAC3" => "ec-3".to_string(),
            _ => format!("unknown:{}", codec_id),
        }
    }
}

impl MkvDemuxer {
    /// Parse header from a potentially truncated buffer (Range-first streaming).
    ///
    /// `MatroskaFile::open()` follows SeekHead entries to reach elements like Cues,
    /// Chapters, Tags. In most MKV files, Cues are at the END of the file. When we
    /// only have the first N MB, seeking to the Cues offset causes an IoError that
    /// propagates up and aborts the entire parse — even though Info, Tracks, and
    /// the first Cluster are all within our buffer.
    ///
    /// Fix: neutralize the SeekHead element ID in a copy of the data so the parser
    /// falls back to `build_seek_head()` — a sequential scan that only finds elements
    /// within the buffer. Unknown elements are properly skipped (parse_location + seek).
    pub fn parse_header_streaming(&mut self, data: &[u8]) -> Result<MediaInfo, DemuxError> {
        let mut patched = data.to_vec();
        Self::neutralize_seekhead(&mut patched);
        self.parse_header_from_vec(patched)
    }

    /// Like `parse_header_streaming` but takes ownership of the Vec, avoiding
    /// the initial `data.to_vec()` copy. Use this when you already have an owned Vec
    /// (e.g. from `build_mkv_buffer()`).
    pub fn parse_header_streaming_owned(
        &mut self,
        mut data: Vec<u8>,
    ) -> Result<MediaInfo, DemuxError> {
        Self::neutralize_seekhead(&mut data);
        self.parse_header_from_vec(data)
    }

    /// Internal: parse header from an owned Vec. Stores a Bytes reference
    /// for raw_data (O(1) clone) and creates the MatroskaFile cursor.
    /// Total copies: 1 (Vec→Cursor needs its own copy since raw_data shares via Bytes).
    fn parse_header_from_vec(&mut self, data: Vec<u8>) -> Result<MediaInfo, DemuxError> {
        // Convert to Bytes (O(1) — takes ownership of the Vec's allocation)
        let bytes = Bytes::from(data);
        // Store for seek rewind — Bytes::clone is O(1) (atomic refcount)
        self.raw_data = Some(bytes.clone());
        // MatroskaFile<Cursor<Vec<u8>>> needs its own Vec — this is the ONE
        // unavoidable copy per parse. With Bytes, raw_data above was free.
        let vec_for_cursor = bytes.to_vec();
        let cursor = Cursor::new(vec_for_cursor);

        let mkv = MatroskaFile::open(cursor)
            .map_err(|e| DemuxError::InvalidData(format!("MKV parse error: {}", e)))?;

        self.finish_parse_header(mkv, &bytes)
    }

    /// Zero out the entire SeekHead element in raw EBML data.
    ///
    /// The SeekHead (ID: 0x114D9B74) contains offsets to elements like Cues, Tags,
    /// Chapters — which may be at the END of a multi-GB file. When parsing a truncated
    /// buffer, seeking to those offsets causes an IoError that aborts the parse.
    ///
    /// By zeroing the entire element (ID + VINT + data), the EBML parser skips the
    /// zero bytes (parse_variable_u32 treats 0x0_ bytes as padding) and falls back
    /// to `build_seek_head()` — a sequential scan that only finds elements within
    /// the buffer and gracefully handles EOF.
    fn neutralize_seekhead(data: &mut [u8]) {
        const SEEKHEAD_ID: [u8; 4] = [0x11, 0x4D, 0x9B, 0x74];
        let search_limit = data.len().min(256);

        for i in 0..search_limit.saturating_sub(3) {
            if data[i..i + 4] == SEEKHEAD_ID {
                let vint_start = i + 4;
                if vint_start >= data.len() {
                    // Can't read VINT, just zero the ID
                    data[i..i + 4].fill(0x00);
                    break;
                }

                // Decode EBML VINT to determine data size
                let (vint_len, data_size) = Self::decode_ebml_vint(&data[vint_start..]);
                if vint_len == 0 {
                    data[i..i + 4].fill(0x00);
                    break;
                }

                // Zero the entire element: [ID: 4][VINT: vint_len][data: data_size]
                let total = 4 + vint_len + data_size as usize;
                let end = (i + total).min(data.len());
                data[i..end].fill(0x00);
                break;
            }
        }
    }

    /// Decode an EBML variable-length unsigned integer (VINT).
    /// Returns (byte_length, value). (0, 0) on error.
    fn decode_ebml_vint(data: &[u8]) -> (usize, u64) {
        if data.is_empty() {
            return (0, 0);
        }
        let first = data[0];
        let len = if first & 0x80 != 0 {
            1
        } else if first & 0x40 != 0 {
            2
        } else if first & 0x20 != 0 {
            3
        } else if first & 0x10 != 0 {
            4
        } else if first & 0x08 != 0 {
            5
        } else if first & 0x04 != 0 {
            6
        } else if first & 0x02 != 0 {
            7
        } else if first & 0x01 != 0 {
            8
        } else {
            return (0, 0);
        };

        if data.len() < len {
            return (0, 0);
        }

        // Strip the leading marker bit
        let mask = first & !(0x80u8 >> (len - 1));
        let mut value = mask as u64;
        for j in 1..len {
            value = (value << 8) | data[j] as u64;
        }

        (len, value)
    }
}

impl MkvDemuxer {
    /// Common parse logic: extract tracks, build seek index, store the MatroskaFile.
    /// `raw_bytes` is used for the seek index scan (passed as &[u8] slice from Bytes).
    fn finish_parse_header(
        &mut self,
        mkv: MatroskaFile<Cursor<Vec<u8>>>,
        raw_bytes: &[u8],
    ) -> Result<MediaInfo, DemuxError> {
        let mut video_tracks = Vec::new();
        let mut audio_tracks = Vec::new();
        let mut video_track_ids = Vec::new();
        let mut audio_track_ids = Vec::new();

        for track in mkv.tracks() {
            let track_number = track.track_number().get();
            let codec_id = track.codec_id();
            let codec_private = track.codec_private().map(|d| d.to_vec());

            match track.track_type() {
                TrackType::Video => {
                    let video = track.video().ok_or_else(|| {
                        DemuxError::InvalidData("Video track without video settings".into())
                    })?;

                    let codec_string = Self::map_video_codec(codec_id, &codec_private);

                    video_tracks.push(VideoTrackInfo {
                        track_id: track_number as u32,
                        codec_string,
                        width: video.pixel_width().get() as u32,
                        height: video.pixel_height().get() as u32,
                        fps: track
                            .default_duration()
                            .map(|d| 1_000_000_000.0 / d.get() as f64),
                        codec_config: codec_private.unwrap_or_default(),
                    });
                    video_track_ids.push(track_number);
                }
                TrackType::Audio => {
                    let audio = track.audio().ok_or_else(|| {
                        DemuxError::InvalidData("Audio track without audio settings".into())
                    })?;

                    let codec_string = Self::map_audio_codec(codec_id);

                    audio_tracks.push(AudioTrackInfo {
                        track_id: track_number as u32,
                        codec_string,
                        sample_rate: audio.sampling_frequency() as u32,
                        channels: audio.channels().get() as u32,
                        codec_config: codec_private.unwrap_or_default(),
                    });
                    audio_track_ids.push(track_number);
                }
                _ => {}
            }
        }

        // Duration: info.duration is in nanoseconds scaled by TimestampScale
        let duration_us = mkv.info().duration().map(|d| {
            let timestamp_scale = mkv.info().timestamp_scale().get() as f64;
            ((d * timestamp_scale) / 1_000.0) as i64
        });

        let container = ContainerFormat::Mkv;

        let info = MediaInfo {
            container,
            duration_us,
            video_tracks,
            audio_tracks,
        };

        // Build seek index from Cluster scanning — incremental: only scan new bytes
        let timestamp_scale_ns = mkv.info().timestamp_scale().get();
        let new_entries = Self::scan_clusters_for_seek_index(
            raw_bytes,
            timestamp_scale_ns,
            self.last_scanned_offset,
        );

        // Merge new entries into existing seek index
        if !new_entries.entries.is_empty() {
            self.seek_index.entries.extend(new_entries.entries);
            // Deduplicate by byte_offset (keep first occurrence)
            self.seek_index.entries.sort_by_key(|e| e.byte_offset);
            self.seek_index.entries.dedup_by_key(|e| e.byte_offset);
        }
        self.last_scanned_offset = raw_bytes.len();

        self.media_info = Some(info.clone());
        self.video_track_ids = video_track_ids;
        self.audio_track_ids = audio_track_ids;
        self.timestamp_scale_ns = timestamp_scale_ns;
        self.mkv = Some(mkv);

        Ok(info)
    }
}

impl Demuxer for MkvDemuxer {
    fn probe(data: &[u8]) -> bool {
        data.len() >= 4 && data[0] == 0x1A && data[1] == 0x45 && data[2] == 0xDF && data[3] == 0xA3
    }

    fn parse_header(&mut self, data: &[u8]) -> Result<MediaInfo, DemuxError> {
        // Delegate to the owned-Vec path. This adds one copy (data.to_vec()),
        // but parse_header is called via the Demuxer trait with &[u8].
        // For MKV streaming, prefer parse_header_streaming_owned() which
        // takes Vec<u8> by value and avoids this copy.
        self.parse_header_from_vec(data.to_vec())
    }

    fn next_chunk(&mut self) -> Result<Option<EncodedChunk>, DemuxError> {
        let mkv = self
            .mkv
            .as_mut()
            .ok_or_else(|| DemuxError::InvalidData("No header parsed yet".into()))?;

        let mut frame = Frame::default();
        match mkv.next_frame(&mut frame) {
            Ok(true) => {
                self.frames_read += 1;
                let track_number = frame.track;
                let is_video = self.video_track_ids.contains(&track_number);
                let is_audio = self.audio_track_ids.contains(&track_number);

                // frame.timestamp is in ticks (1 tick = timestamp_scale_ns nanoseconds).
                // Default scale = 1,000,000 ns = 1ms per tick.
                // Convert: ticks * scale_ns / 1000 = microseconds
                let timestamp_us =
                    (frame.timestamp as i64).saturating_mul(self.timestamp_scale_ns as i64) / 1_000;

                // For audio tracks, default is_keyframe to true:
                // AAC, Opus, FLAC all produce independently-decodable frames.
                // MKV often leaves is_keyframe unset for audio — defaulting to
                // false would cause WebCodecs to reject the first chunk.
                let is_keyframe = if is_audio {
                    frame.is_keyframe.unwrap_or(true)
                } else {
                    frame.is_keyframe.unwrap_or(false)
                };

                Ok(Some(EncodedChunk {
                    track_id: track_number as u32,
                    is_video,
                    is_audio,
                    is_keyframe,
                    timestamp_us,
                    duration_us: frame
                        .duration
                        .map(|d| (d as i64).saturating_mul(self.timestamp_scale_ns as i64) / 1_000)
                        .unwrap_or(0),
                    data: frame.data,
                }))
            }
            Ok(false) => Ok(None), // EOF
            Err(e) => Err(DemuxError::InvalidData(format!("MKV frame error: {}", e))),
        }
    }

    fn seek_to_keyframe(&mut self, timestamp_us: i64) -> Result<(), DemuxError> {
        // matroska-demuxer doesn't support native seeking.
        // Strategy:
        // 1. Scan frames to find the last video keyframe before target (skip_count)
        // 2. Re-create the MatroskaFile from the same buffer (rewind)
        // 3. Skip exactly skip_count frames so next_chunk() returns the keyframe
        //
        // We MUST re-create because MatroskaFile has no rewind — scanning consumes
        // the internal cursor irreversibly.
        let mkv = self
            .mkv
            .as_mut()
            .ok_or_else(|| DemuxError::InvalidData("No header parsed yet".into()))?;

        // Phase 1: Scan all frames to find skip_count
        let mut frame = Frame::default();
        let mut skip_count: usize = 0;
        let mut count: usize = 0;

        loop {
            match mkv.next_frame(&mut frame) {
                Ok(true) => {
                    count += 1;
                    let frame_ts_us = (frame.timestamp as i64)
                        .saturating_mul(self.timestamp_scale_ns as i64)
                        / 1_000;

                    // Track last video keyframe before target
                    let is_video = self.video_track_ids.contains(&frame.track);
                    if is_video && frame.is_keyframe.unwrap_or(false) && frame_ts_us <= timestamp_us
                    {
                        skip_count = count - 1;
                    }

                    // Stop scanning once we're past the target
                    if frame_ts_us > timestamp_us {
                        break;
                    }
                }
                Ok(false) => break, // EOF
                Err(e) => {
                    return Err(DemuxError::InvalidData(format!(
                        "MKV seek scan error: {}",
                        e
                    )))
                }
            }
        }

        // Phase 2: Re-create MatroskaFile from stored raw data
        // MatroskaFile has no rewind/into_inner, so we use the copy saved during parse_header.
        // raw_data is Bytes (refcounted), so .to_vec() is the only copy needed here.
        let data_vec = self
            .raw_data
            .as_ref()
            .ok_or_else(|| DemuxError::InvalidData("No raw data stored for seek rewind".into()))?
            .to_vec();
        self.mkv.take(); // drop the consumed demuxer

        // Re-create MatroskaFile from the same data
        let fresh_cursor = Cursor::new(data_vec);
        let fresh_mkv = MatroskaFile::open(fresh_cursor)
            .map_err(|e| DemuxError::InvalidData(format!("MKV seek rewind error: {}", e)))?;
        self.mkv = Some(fresh_mkv);

        // Phase 3: Skip to the keyframe position
        self.frames_read = 0;
        self.skip_frames(skip_count)?;

        Ok(())
    }

    fn build_seek_index(&self) -> SeekIndex {
        self.seek_index.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =============================================
    // find_cluster_offset tests
    // =============================================

    #[test]
    fn cluster_offset_empty_data() {
        assert_eq!(find_cluster_offset(&[]), None);
    }

    #[test]
    fn cluster_offset_too_short() {
        // Less than 4 bytes — can't contain a Cluster ID
        assert_eq!(find_cluster_offset(&[0x1F, 0x43, 0xB6]), None);
    }

    #[test]
    fn cluster_offset_exactly_4_bytes_at_edge() {
        // Cluster ID at the very end, no VINT byte to validate
        // Should still be accepted (edge-of-data rule)
        let data = [0x1F, 0x43, 0xB6, 0x75];
        // pos + 4 < data.len() → 0 + 4 < 4 → false, so the while loop doesn't run
        assert_eq!(find_cluster_offset(&data), None);
    }

    #[test]
    fn cluster_offset_5_bytes_valid() {
        // Cluster ID followed by valid VINT (0x01 = smallest VINT)
        let data = [0x1F, 0x43, 0xB6, 0x75, 0x01];
        // pos + 4 < 5 → true, finds cluster at 0, abs + 4 < 5 → false
        // → edge of data → accept at offset 0
        assert_eq!(find_cluster_offset(&data), Some(0));
    }

    #[test]
    fn cluster_offset_valid_at_start() {
        let data = [0x1F, 0x43, 0xB6, 0x75, 0xA3, 0xFF, 0xFF];
        assert_eq!(find_cluster_offset(&data), Some(0));
    }

    #[test]
    fn cluster_offset_valid_with_prefix() {
        // Some data before the Cluster ID
        let mut data = vec![0x00, 0x00, 0x00, 0x00, 0x00];
        data.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75, 0xA3]);
        assert_eq!(find_cluster_offset(&data), Some(5));
    }

    #[test]
    fn cluster_offset_false_positive_zero_vint() {
        // Cluster ID bytes followed by 0x00 (invalid VINT) — should be skipped
        let mut data = vec![0x1F, 0x43, 0xB6, 0x75, 0x00];
        // Then a real Cluster with valid VINT
        data.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75, 0x81]);
        assert_eq!(find_cluster_offset(&data), Some(5));
    }

    #[test]
    fn cluster_offset_multiple_false_positives() {
        // Three false positives (VINT=0x00) then one real
        let mut data = Vec::new();
        for _ in 0..3 {
            data.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75, 0x00]);
        }
        data.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75, 0x42]);
        assert_eq!(find_cluster_offset(&data), Some(15));
    }

    #[test]
    fn cluster_offset_no_cluster_in_random_data() {
        let data = vec![0x42; 1000];
        assert_eq!(find_cluster_offset(&data), None);
    }

    #[test]
    fn cluster_offset_partial_cluster_id() {
        // Only 3 of 4 bytes of the Cluster ID
        let data = [0x1F, 0x43, 0xB6, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(find_cluster_offset(&data), None);
    }

    #[test]
    fn cluster_offset_all_vint_values_valid() {
        // Every non-zero VINT first byte should be accepted
        for vint in 1u8..=255 {
            let data = [0x1F, 0x43, 0xB6, 0x75, vint, 0x00];
            assert_eq!(
                find_cluster_offset(&data),
                Some(0),
                "VINT byte 0x{:02X} should be accepted",
                vint
            );
        }
    }

    #[test]
    fn cluster_offset_embedded_in_large_data() {
        // 10KB of random-ish data with a Cluster in the middle
        let mut data = vec![0xAA; 5000];
        data.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75, 0x81]);
        data.extend_from_slice(&[0xBB; 5000]);
        assert_eq!(find_cluster_offset(&data), Some(5000));
    }

    #[test]
    fn cluster_offset_at_end_with_vint() {
        // Cluster ID at the very end of data, just enough room for VINT
        let mut data = vec![0x00; 100];
        data.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75, 0x01]);
        assert_eq!(find_cluster_offset(&data), Some(100));
    }

    #[test]
    fn cluster_offset_overlapping_bytes() {
        // Bytes that partially overlap with Cluster ID
        let data = [0x1F, 0x43, 0x1F, 0x43, 0xB6, 0x75, 0x81];
        assert_eq!(find_cluster_offset(&data), Some(2));
    }

    // =============================================
    // MkvDemuxer::probe tests
    // =============================================

    #[test]
    fn mkv_probe_valid_ebml() {
        assert!(MkvDemuxer::probe(&[0x1A, 0x45, 0xDF, 0xA3, 0x93, 0x42]));
    }

    #[test]
    fn mkv_probe_empty() {
        assert!(!MkvDemuxer::probe(&[]));
    }

    #[test]
    fn mkv_probe_too_short() {
        assert!(!MkvDemuxer::probe(&[0x1A, 0x45, 0xDF]));
    }

    #[test]
    fn mkv_probe_wrong_magic() {
        assert!(!MkvDemuxer::probe(&[0x00, 0x00, 0x00, 0x00]));
    }

    #[test]
    fn mkv_probe_mp4_magic() {
        // ftyp — should NOT match MKV
        assert!(!MkvDemuxer::probe(&[
            0x00, 0x00, 0x00, 0x1C, b'f', b't', b'y', b'p'
        ]));
    }

    #[test]
    fn mkv_probe_exactly_4_bytes() {
        assert!(MkvDemuxer::probe(&[0x1A, 0x45, 0xDF, 0xA3]));
    }

    // =============================================
    // map_video_codec tests
    // =============================================

    #[test]
    fn map_video_codec_avc_with_config() {
        let config = vec![0x01, 0x64, 0x00, 0x1F]; // profile=100, compat=0, level=31
        let result = MkvDemuxer::map_video_codec("V_MPEG4/ISO/AVC", &Some(config));
        assert_eq!(result, "avc1.64001F");
    }

    #[test]
    fn map_video_codec_avc_without_config() {
        let result = MkvDemuxer::map_video_codec("V_MPEG4/ISO/AVC", &None);
        assert_eq!(result, "avc1.640029");
    }

    #[test]
    fn map_video_codec_avc_short_config() {
        let config = vec![0x01, 0x42]; // only 2 bytes — not enough
        let result = MkvDemuxer::map_video_codec("V_MPEG4/ISO/AVC", &Some(config));
        assert_eq!(result, "avc1.640029"); // fallback
    }

    #[test]
    fn map_video_codec_hevc() {
        let result = MkvDemuxer::map_video_codec("V_MPEGH/ISO/HEVC", &None);
        assert_eq!(result, "hvc1.1.6.L93.B0");
    }

    #[test]
    fn map_video_codec_vp8() {
        let result = MkvDemuxer::map_video_codec("V_VP8", &None);
        assert_eq!(result, "vp8");
    }

    #[test]
    fn map_video_codec_vp9() {
        let result = MkvDemuxer::map_video_codec("V_VP9", &None);
        assert_eq!(result, "vp09.00.10.08");
    }

    #[test]
    fn map_video_codec_av1() {
        let result = MkvDemuxer::map_video_codec("V_AV1", &None);
        assert_eq!(result, "av01.0.01M.08");
    }

    #[test]
    fn map_video_codec_unknown() {
        let result = MkvDemuxer::map_video_codec("V_THEORA", &None);
        assert_eq!(result, "unknown:V_THEORA");
    }

    // =============================================
    // map_audio_codec tests
    // =============================================

    #[test]
    fn map_audio_codec_aac() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_AAC"), "mp4a.40.2");
    }

    #[test]
    fn map_audio_codec_aac_mpeg2_lc() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_AAC/MPEG2/LC"), "mp4a.40.2");
    }

    #[test]
    fn map_audio_codec_aac_mpeg4_lc() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_AAC/MPEG4/LC"), "mp4a.40.2");
    }

    #[test]
    fn map_audio_codec_aac_sbr() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_AAC/MPEG4/SBR"), "mp4a.40.5");
    }

    #[test]
    fn map_audio_codec_opus() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_OPUS"), "opus");
    }

    #[test]
    fn map_audio_codec_vorbis() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_VORBIS"), "vorbis");
    }

    #[test]
    fn map_audio_codec_flac() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_FLAC"), "flac");
    }

    #[test]
    fn map_audio_codec_ac3() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_AC3"), "ac-3");
    }

    #[test]
    fn map_audio_codec_eac3() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_EAC3"), "ec-3");
    }

    #[test]
    fn map_audio_codec_unknown() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_DTS"), "unknown:A_DTS");
    }

    // =============================================
    // MkvDemuxer state tests
    // =============================================

    #[test]
    fn mkv_demuxer_initial_state() {
        let d = MkvDemuxer::new();
        assert_eq!(d.frames_read(), 0);
        assert!(d.mkv.is_none());
        assert!(d.media_info.is_none());
    }

    #[test]
    fn mkv_demuxer_next_chunk_without_parse_errors() {
        let mut d = MkvDemuxer::new();
        let result = d.next_chunk();
        assert!(result.is_err());
        match result.unwrap_err() {
            DemuxError::InvalidData(msg) => assert!(msg.contains("No header parsed")),
            other => panic!("Expected InvalidData, got: {:?}", other),
        }
    }

    #[test]
    fn mkv_demuxer_seek_without_parse_errors() {
        let mut d = MkvDemuxer::new();
        let result = d.seek_to_keyframe(0);
        assert!(result.is_err());
        match result.unwrap_err() {
            DemuxError::InvalidData(msg) => assert!(msg.contains("No header parsed")),
            other => panic!("Expected InvalidData, got: {:?}", other),
        }
    }

    #[test]
    fn mkv_demuxer_skip_frames_without_parse_errors() {
        let mut d = MkvDemuxer::new();
        let result = d.skip_frames(10);
        assert!(result.is_err());
    }

    #[test]
    fn mkv_demuxer_skip_zero_frames_without_parse() {
        let mut d = MkvDemuxer::new();
        // skip_frames(0) should still error because mkv is None
        let result = d.skip_frames(0);
        assert!(result.is_err());
    }

    // =============================================
    // parse_ebml_vint tests
    // =============================================

    #[test]
    fn vint_empty_data() {
        assert!(parse_ebml_vint(&[]).is_none());
    }

    #[test]
    fn vint_zero_byte() {
        assert!(parse_ebml_vint(&[0x00]).is_none());
    }

    #[test]
    fn vint_1_byte() {
        // 0x81 = 1_0000001 → width=1, value=1
        let (val, len) = parse_ebml_vint(&[0x81]).unwrap();
        assert_eq!(len, 1);
        assert_eq!(val, 1);
    }

    #[test]
    fn vint_1_byte_max() {
        // 0xFE = 1_1111110 → width=1, value=126
        let (val, len) = parse_ebml_vint(&[0xFE]).unwrap();
        assert_eq!(len, 1);
        assert_eq!(val, 126);
    }

    #[test]
    fn vint_2_bytes() {
        // 0x40 0x02 = 01_000000 00000010 → width=2, value=2
        let (val, len) = parse_ebml_vint(&[0x40, 0x02]).unwrap();
        assert_eq!(len, 2);
        assert_eq!(val, 2);
    }

    #[test]
    fn vint_2_bytes_truncated() {
        // Only 1 byte but width=2 → None
        assert!(parse_ebml_vint(&[0x40]).is_none());
    }

    #[test]
    fn vint_3_bytes() {
        // 0x20 0x00 0x05 = 001_00000 00000000 00000101 → width=3, value=5
        let (val, len) = parse_ebml_vint(&[0x20, 0x00, 0x05]).unwrap();
        assert_eq!(len, 3);
        assert_eq!(val, 5);
    }

    #[test]
    fn vint_4_bytes() {
        // 0x10 0x00 0x00 0x0A → width=4, value=10
        let (val, len) = parse_ebml_vint(&[0x10, 0x00, 0x00, 0x0A]).unwrap();
        assert_eq!(len, 4);
        assert_eq!(val, 10);
    }

    // =============================================
    // find_cluster_timestamp tests
    // =============================================

    #[test]
    fn cluster_timestamp_not_found() {
        assert!(find_cluster_timestamp(&[]).is_none());
        assert!(find_cluster_timestamp(&[0x00, 0x01]).is_none());
    }

    #[test]
    fn cluster_timestamp_simple() {
        // Timestamp element: ID=0xE7, size=0x82 (VINT for 2), value=0x01F4 (500)
        let data = [0xE7, 0x82, 0x01, 0xF4];
        let ts = find_cluster_timestamp(&data).unwrap();
        assert_eq!(ts, 500);
    }

    #[test]
    fn cluster_timestamp_zero() {
        // Timestamp = 0
        let data = [0xE7, 0x81, 0x00];
        let ts = find_cluster_timestamp(&data).unwrap();
        assert_eq!(ts, 0);
    }

    #[test]
    fn cluster_timestamp_1_byte_value() {
        // Timestamp = 42
        let data = [0xE7, 0x81, 0x2A];
        let ts = find_cluster_timestamp(&data).unwrap();
        assert_eq!(ts, 42);
    }

    #[test]
    fn cluster_timestamp_truncated() {
        // Timestamp element with size=2 but only 1 byte of value
        let data = [0xE7, 0x82, 0x01];
        assert!(find_cluster_timestamp(&data).is_none());
    }

    // =============================================
    // scan_clusters_for_seek_index tests
    // =============================================

    /// Build a minimal MKV Cluster with a timestamp element.
    fn make_cluster(timestamp: u16, content_extra: usize) -> Vec<u8> {
        let mut data = Vec::new();
        // Cluster ID
        data.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75]);
        // Cluster size: Timestamp element (4 bytes) + extra content
        let content_size = 4 + content_extra;
        data.push(0x80 | content_size as u8); // VINT 1-byte size
                                              // Timestamp element: ID=0xE7, size=0x82 (2 bytes), value
        data.push(0xE7);
        data.push(0x82);
        data.extend_from_slice(&timestamp.to_be_bytes());
        // Extra content (filler)
        data.extend(vec![0x00; content_extra]);
        data
    }

    #[test]
    fn scan_clusters_empty_data() {
        let idx = MkvDemuxer::scan_clusters_for_seek_index(&[], 1_000_000, 0);
        assert!(idx.is_empty());
    }

    #[test]
    fn scan_clusters_no_clusters() {
        let data = vec![0x00; 100];
        let idx = MkvDemuxer::scan_clusters_for_seek_index(&data, 1_000_000, 0);
        assert!(idx.is_empty());
    }

    #[test]
    fn scan_clusters_single_cluster() {
        let data = make_cluster(0, 10);
        // timestamp_scale = 1_000_000 ns (1ms)
        let idx = MkvDemuxer::scan_clusters_for_seek_index(&data, 1_000_000, 0);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.entries[0].timestamp_us, 0);
        assert_eq!(idx.entries[0].byte_offset, 0);
    }

    #[test]
    fn scan_clusters_multiple_clusters() {
        let mut data = Vec::new();
        // Cluster at timestamp 0
        data.extend(make_cluster(0, 10));
        let offset1 = data.len();
        // Cluster at timestamp 1000 (1s with 1ms scale)
        data.extend(make_cluster(1000, 10));
        let offset2 = data.len();
        // Cluster at timestamp 2000
        data.extend(make_cluster(2000, 10));

        // timestamp_scale = 1_000_000 ns (1ms) → timestamp 1000 = 1_000_000 us
        let idx = MkvDemuxer::scan_clusters_for_seek_index(&data, 1_000_000, 0);
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.entries[0].timestamp_us, 0);
        assert_eq!(idx.entries[0].byte_offset, 0);
        assert_eq!(idx.entries[1].timestamp_us, 1_000_000);
        assert_eq!(idx.entries[1].byte_offset, offset1 as u64);
        assert_eq!(idx.entries[2].timestamp_us, 2_000_000);
        assert_eq!(idx.entries[2].byte_offset, offset2 as u64);
    }

    #[test]
    fn scan_clusters_with_prefix_data() {
        // Some non-cluster data before the first cluster
        let mut data = vec![0x00; 50];
        data.extend(make_cluster(42, 5));
        let idx = MkvDemuxer::scan_clusters_for_seek_index(&data, 1_000_000, 0);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.entries[0].byte_offset, 50);
        // timestamp 42 * scale 1_000_000ns / 1_000 = 42_000 us
        assert_eq!(idx.entries[0].timestamp_us, 42_000);
    }

    #[test]
    fn scan_clusters_timestamp_scale_conversion() {
        let data = make_cluster(500, 5);
        // Default MKV timestamp_scale = 1_000_000 ns (1ms)
        // Cluster timestamp = 500 → 500 * 1_000_000 / 1_000 = 500_000 us
        let idx = MkvDemuxer::scan_clusters_for_seek_index(&data, 1_000_000, 0);
        assert_eq!(idx.entries[0].timestamp_us, 500_000);
    }

    #[test]
    fn mkv_demuxer_build_seek_index_without_parse() {
        let d = MkvDemuxer::new();
        let idx = d.build_seek_index();
        assert!(idx.is_empty());
    }

    // =============================================
    // decode_ebml_vint (MkvDemuxer impl) tests
    // =============================================

    #[test]
    fn demuxer_vint_empty() {
        assert_eq!(MkvDemuxer::decode_ebml_vint(&[]), (0, 0));
    }

    #[test]
    fn demuxer_vint_zero_byte() {
        assert_eq!(MkvDemuxer::decode_ebml_vint(&[0x00]), (0, 0));
    }

    #[test]
    fn demuxer_vint_1_byte_min() {
        // 0x80 = 1_0000000 → len=1, value=0 (marker bit stripped)
        assert_eq!(MkvDemuxer::decode_ebml_vint(&[0x80]), (1, 0));
    }

    #[test]
    fn demuxer_vint_1_byte_value() {
        // 0x8F = 1_0001111 → len=1, value=15
        assert_eq!(MkvDemuxer::decode_ebml_vint(&[0x8F]), (1, 15));
    }

    #[test]
    fn demuxer_vint_1_byte_max() {
        // 0xFE = 1_1111110 → len=1, value=126
        assert_eq!(MkvDemuxer::decode_ebml_vint(&[0xFE]), (1, 126));
    }

    #[test]
    fn demuxer_vint_2_byte() {
        // 0x40 0x80 = 01_000000 10000000 → len=2, value=128
        assert_eq!(MkvDemuxer::decode_ebml_vint(&[0x40, 0x80]), (2, 128));
    }

    #[test]
    fn demuxer_vint_2_byte_real_seekhead() {
        // Real SeekHead VINT from test MKV: 0x4F = 01_001111 → len=2?
        // Actually 0x4F has bit6 set (0x40) → len=2, needs 2 bytes
        // But if we only have 1 byte → (0, 0)
        assert_eq!(MkvDemuxer::decode_ebml_vint(&[0x4F]), (0, 0));
        // With second byte: 0x4F 0x00 = 01_001111 00000000 → value = 0x0F00 = 3840
        assert_eq!(MkvDemuxer::decode_ebml_vint(&[0x4F, 0x00]), (2, 0x0F00));
    }

    #[test]
    fn demuxer_vint_1_byte_cf() {
        // 0xCF from real SeekHead = 1_1001111 → len=1, value=0x4F = 79
        assert_eq!(MkvDemuxer::decode_ebml_vint(&[0xCF]), (1, 79));
    }

    #[test]
    fn demuxer_vint_3_byte() {
        // 0x20 0x01 0x00 → len=3, value=256
        assert_eq!(MkvDemuxer::decode_ebml_vint(&[0x20, 0x01, 0x00]), (3, 256));
    }

    #[test]
    fn demuxer_vint_4_byte() {
        // 0x10 0x00 0x01 0x00 → len=4, value=256
        assert_eq!(
            MkvDemuxer::decode_ebml_vint(&[0x10, 0x00, 0x01, 0x00]),
            (4, 256)
        );
    }

    #[test]
    fn demuxer_vint_truncated() {
        // 0x20 needs 3 bytes but only 2 provided
        assert_eq!(MkvDemuxer::decode_ebml_vint(&[0x20, 0x00]), (0, 0));
    }

    #[test]
    fn demuxer_vint_8_byte() {
        // 0x01 + 7 zero bytes → len=8, value=0
        let data = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(MkvDemuxer::decode_ebml_vint(&data), (8, 0));
    }

    #[test]
    fn demuxer_vint_8_byte_with_value() {
        // 0x01 0x00 0x00 0x00 0x00 0x00 0x00 0x01 → len=8, value=1
        let data = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
        assert_eq!(MkvDemuxer::decode_ebml_vint(&data), (8, 1));
    }

    // =============================================
    // neutralize_seekhead tests
    // =============================================

    /// Build a minimal EBML header + SeekHead for testing.
    /// Returns (data, seekhead_offset, seekhead_total_size)
    fn make_ebml_with_seekhead(seekhead_data_size: u8) -> (Vec<u8>, usize, usize) {
        let mut data = Vec::new();
        // EBML header (simplified — just the magic)
        data.extend_from_slice(&[0x1A, 0x45, 0xDF, 0xA3]);
        // Some padding to simulate EBML header content
        data.extend_from_slice(&[0x93, 0x42, 0x86, 0x81, 0x01]);
        // More padding
        data.extend(vec![0x42; 30]);

        let seekhead_offset = data.len();
        // SeekHead ID: 0x11 0x4D 0x9B 0x74
        data.extend_from_slice(&[0x11, 0x4D, 0x9B, 0x74]);
        // VINT size (1-byte): 0x80 | size
        data.push(0x80 | seekhead_data_size);
        // SeekHead data
        data.extend(vec![0xAA; seekhead_data_size as usize]);

        let total_size = 4 + 1 + seekhead_data_size as usize;
        // Some data after (Info/Tracks elements)
        data.extend_from_slice(&[0x15, 0x49, 0xA9, 0x66]); // Info element ID
        data.extend(vec![0xBB; 20]);

        (data, seekhead_offset, total_size)
    }

    #[test]
    fn neutralize_seekhead_zeros_entire_element() {
        let (mut data, offset, total_size) = make_ebml_with_seekhead(20);
        let data_after_seekhead = data[offset + total_size..offset + total_size + 4].to_vec();

        MkvDemuxer::neutralize_seekhead(&mut data);

        // SeekHead region should be all zeros
        for i in offset..offset + total_size {
            assert_eq!(data[i], 0x00, "byte at {} should be zero", i);
        }
        // Data AFTER SeekHead should be untouched
        assert_eq!(
            &data[offset + total_size..offset + total_size + 4],
            &data_after_seekhead,
            "bytes after SeekHead should be untouched"
        );
    }

    #[test]
    fn neutralize_seekhead_no_seekhead() {
        // Data without any SeekHead — should be unchanged
        let mut data = vec![0x1A, 0x45, 0xDF, 0xA3, 0x93, 0x42, 0x86, 0x81];
        data.extend(vec![0xFF; 100]);
        let original = data.clone();
        MkvDemuxer::neutralize_seekhead(&mut data);
        assert_eq!(data, original);
    }

    #[test]
    fn neutralize_seekhead_at_very_start() {
        // SeekHead right at byte 0
        let mut data = vec![0x11, 0x4D, 0x9B, 0x74, 0x85]; // ID + VINT (size=5)
        data.extend(vec![0xAA; 5]); // 5 bytes of SeekHead data
        data.extend(vec![0xBB; 10]); // data after

        MkvDemuxer::neutralize_seekhead(&mut data);

        // First 10 bytes (4 ID + 1 VINT + 5 data) should be zero
        for i in 0..10 {
            assert_eq!(data[i], 0x00);
        }
        // Rest should be untouched
        for i in 10..20 {
            assert_eq!(data[i], 0xBB);
        }
    }

    #[test]
    fn neutralize_seekhead_real_world_size() {
        // Real MKV SeekHead: 0xCF = VINT for 79 bytes data
        // Total: 4 (ID) + 1 (VINT) + 79 (data) = 84 bytes
        let mut data = vec![0x00; 0x2E]; // offset 0x2E like real file
        data.extend_from_slice(&[0x11, 0x4D, 0x9B, 0x74]); // SeekHead ID
        data.push(0xCF); // VINT = 79
        data.extend(vec![0xAA; 79]); // SeekHead data
        data.extend(vec![0xBB; 50]); // data after

        let seekhead_start = 0x2E;
        MkvDemuxer::neutralize_seekhead(&mut data);

        for i in seekhead_start..seekhead_start + 84 {
            assert_eq!(data[i], 0x00, "byte at 0x{:02X} should be zero", i);
        }
        // Verify data after is untouched
        assert_eq!(data[seekhead_start + 84], 0xBB);
    }

    #[test]
    fn neutralize_seekhead_truncated_at_id() {
        // SeekHead ID right at the end of data, no VINT
        let mut data = vec![0x00; 10];
        data.extend_from_slice(&[0x11, 0x4D, 0x9B, 0x74]);

        MkvDemuxer::neutralize_seekhead(&mut data);

        // ID should be zeroed
        for i in 10..14 {
            assert_eq!(data[i], 0x00);
        }
    }

    #[test]
    fn neutralize_seekhead_truncated_data() {
        // SeekHead claims 50 bytes but data ends before that
        let mut data = vec![0x00; 5];
        data.extend_from_slice(&[0x11, 0x4D, 0x9B, 0x74]); // ID at offset 5
        data.push(0xB2); // VINT = 50
        data.extend(vec![0xAA; 10]); // Only 10 bytes, not 50

        MkvDemuxer::neutralize_seekhead(&mut data);

        // Should zero from offset 5 to end of data
        for i in 5..data.len() {
            assert_eq!(data[i], 0x00, "byte at {} should be zero", i);
        }
    }

    #[test]
    fn neutralize_seekhead_beyond_search_limit() {
        // SeekHead at offset 260 — beyond the 256-byte search limit
        let mut data = vec![0x00; 260];
        data.extend_from_slice(&[0x11, 0x4D, 0x9B, 0x74, 0x85]);
        data.extend(vec![0xAA; 5]);
        let original = data.clone();

        MkvDemuxer::neutralize_seekhead(&mut data);

        // Should NOT be neutralized (beyond search limit)
        assert_eq!(data, original);
    }

    // =============================================
    // parse_header_streaming tests
    // =============================================

    #[test]
    fn parse_header_streaming_invalid_data() {
        let mut d = MkvDemuxer::new();
        let result = d.parse_header_streaming(&[0x00; 100]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_header_streaming_empty() {
        let mut d = MkvDemuxer::new();
        let result = d.parse_header_streaming(&[]);
        assert!(result.is_err());
    }

    // =============================================
    // Codec string mapping — edge cases
    // =============================================

    #[test]
    fn map_video_codec_avc_exact_4_bytes() {
        // Minimum viable codec_private: exactly 4 bytes
        let config = vec![0x01, 0x42, 0xC0, 0x1E]; // Baseline, level 3.0
        let result = MkvDemuxer::map_video_codec("V_MPEG4/ISO/AVC", &Some(config));
        assert_eq!(result, "avc1.42C01E");
    }

    #[test]
    fn map_video_codec_avc_empty_config() {
        let result = MkvDemuxer::map_video_codec("V_MPEG4/ISO/AVC", &Some(vec![]));
        assert_eq!(result, "avc1.640029"); // fallback
    }

    #[test]
    fn map_video_codec_avc_high_profile() {
        // High Profile, Level 4.0
        let config = vec![0x01, 0x64, 0x00, 0x28, 0xFF, 0xE1]; // extra bytes OK
        let result = MkvDemuxer::map_video_codec("V_MPEG4/ISO/AVC", &Some(config));
        assert_eq!(result, "avc1.640028");
    }

    #[test]
    fn map_audio_codec_all_aac_variants() {
        // Verify all AAC Matroska codec IDs map correctly
        assert_eq!(MkvDemuxer::map_audio_codec("A_AAC"), "mp4a.40.2");
        assert_eq!(MkvDemuxer::map_audio_codec("A_AAC/MPEG2/LC"), "mp4a.40.2");
        assert_eq!(MkvDemuxer::map_audio_codec("A_AAC/MPEG4/LC"), "mp4a.40.2");
        assert_eq!(MkvDemuxer::map_audio_codec("A_AAC/MPEG4/SBR"), "mp4a.40.5");
    }

    #[test]
    fn map_audio_codec_ac3_vs_eac3() {
        // AC-3 and E-AC-3 have distinct WebCodecs codec strings
        assert_eq!(MkvDemuxer::map_audio_codec("A_AC3"), "ac-3");
        assert_eq!(MkvDemuxer::map_audio_codec("A_EAC3"), "ec-3");
    }

    #[test]
    fn map_audio_codec_dts_is_unknown() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_DTS"), "unknown:A_DTS");
        assert_eq!(
            MkvDemuxer::map_audio_codec("A_DTS/EXPRESS"),
            "unknown:A_DTS/EXPRESS"
        );
    }

    #[test]
    fn map_audio_codec_truehd_is_unknown() {
        assert_eq!(MkvDemuxer::map_audio_codec("A_TRUEHD"), "unknown:A_TRUEHD");
    }
}
