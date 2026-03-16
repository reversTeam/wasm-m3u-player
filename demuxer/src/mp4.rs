use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::rc::Rc;
use std::cell::Cell;

use crate::types::*;

/// A cursor wrapper that limits reads to a configurable byte boundary.
///
/// During MP4 header parsing, the synthetic buffer is `[download_data][moov]`.
/// After parsing, we set the read limit to `download_len` so that sample reads
/// beyond the actual mdat data fail with EOF instead of reading moov bytes
/// as if they were sample data (which would produce corrupt frames).
///
/// The limit is stored in a shared `Rc<Cell<u64>>` so the caller can change it
/// after `Mp4Reader::read_header()` has taken ownership of the cursor.
///
/// ## Virtual offset (Range seek support)
///
/// For Range-based seeking, the physical buffer layout is:
///   `[header (ftyp + mdat_header_empty + moov)] [range_data]`
///
/// The mp4 crate reads stco/co64 absolute file offsets (e.g., 800MB).
/// The `virtual_base` field maps these to physical positions:
///   seek(800MB) → physical = header_end + (800MB - virtual_base)
///
/// During header parsing (positions 0..header_end), seeks work normally.
/// During sample reads (positions >= virtual_base), seeks are remapped.
pub struct LimitedCursor {
    inner: Cursor<Vec<u8>>,
    /// Shared read limit. Reads at positions >= this value return EOF.
    /// u64::MAX means unlimited (used during header parsing).
    limit: Rc<Cell<u64>>,
    /// File offset where range_data starts in the original file.
    /// 0 = disabled (normal linear mode). > 0 = virtual offset mode.
    virtual_base: u64,
    /// Physical byte offset in the buffer where range_data begins.
    /// Only used when virtual_base > 0.
    header_end: u64,
}

impl LimitedCursor {
    fn new(data: Vec<u8>) -> (Self, Rc<Cell<u64>>) {
        let limit = Rc::new(Cell::new(u64::MAX));
        let cursor = LimitedCursor {
            inner: Cursor::new(data),
            limit: limit.clone(),
            virtual_base: 0,
            header_end: 0,
        };
        (cursor, limit)
    }

    /// Create a cursor with virtual offset support for Range-seeked MP4 data.
    ///
    /// Buffer layout: `[header_data (ftyp+mdat_hdr+moov)][range_data]`
    /// - `virtual_base`: file offset where `range_data` starts in the original file
    /// - `header_end`: physical position in buffer where `range_data` begins
    ///
    /// When the mp4 crate seeks to an absolute stco offset >= `virtual_base`,
    /// the cursor remaps it to: `header_end + (offset - virtual_base)`.
    fn new_with_offset(data: Vec<u8>, virtual_base: u64, header_end: u64) -> (Self, Rc<Cell<u64>>) {
        let limit = Rc::new(Cell::new(u64::MAX));
        let cursor = LimitedCursor {
            inner: Cursor::new(data),
            limit: limit.clone(),
            virtual_base,
            header_end,
        };
        (cursor, limit)
    }
}

impl Read for LimitedCursor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let limit = self.limit.get();
        let pos = self.inner.position();
        if pos >= limit {
            // Beyond the data limit — mimic EOF so read_exact returns UnexpectedEof
            return Ok(0);
        }
        let remaining = (limit - pos) as usize;
        let capped_len = buf.len().min(remaining);
        self.inner.read(&mut buf[..capped_len])
    }
}

impl Seek for LimitedCursor {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match pos {
            SeekFrom::Start(offset) if self.virtual_base > 0 && offset >= self.virtual_base => {
                // Remap: absolute file offset → physical position in range_data region
                let physical = self.header_end + (offset - self.virtual_base);
                self.inner.seek(SeekFrom::Start(physical))
            }
            _ => self.inner.seek(pos),
        }
    }
}

/// MP4 demuxer using the `mp4` crate.
pub struct Mp4Demuxer {
    reader: Option<mp4::Mp4Reader<LimitedCursor>>,
    media_info: Option<MediaInfo>,
    /// Current sample indices per track (track_id -> next sample index, 1-based).
    sample_cursors: Vec<(u32, u32)>,
    /// Handle to control the read limit on the internal cursor.
    /// Set to download_len after parse_header to prevent reading moov as sample data.
    read_limit: Option<Rc<Cell<u64>>>,
}

/// A top-level MP4 box found during scanning.
#[derive(Debug, Clone)]
pub struct Mp4Box {
    /// 4-char box type (e.g. "ftyp", "moov", "mdat").
    pub box_type: [u8; 4],
    /// Byte offset of the box start in the file.
    pub offset: u64,
    /// Total box size (header + content). 0 if extends to EOF.
    pub size: u64,
}

impl Mp4Box {
    pub fn type_str(&self) -> &str {
        std::str::from_utf8(&self.box_type).unwrap_or("????")
    }
    pub fn is_type(&self, t: &[u8; 4]) -> bool {
        &self.box_type == t
    }
}

/// Result of scanning for moov box position.
#[derive(Debug)]
pub enum MoovLocation {
    /// moov was found in the scanned data — normal streaming works.
    Found { offset: u64, size: u64 },
    /// mdat found before moov — moov is likely at end of file.
    /// Includes the mdat offset+size and the expected moov offset.
    AtEnd { moov_offset: u64 },
    /// Not enough data to determine (no mdat or moov found yet).
    Unknown,
}

impl Mp4Demuxer {
    pub fn new() -> Self {
        Self {
            reader: None,
            media_info: None,
            sample_cursors: Vec::new(),
            read_limit: None,
        }
    }

    /// Set the read limit for sample data access.
    ///
    /// After calling `parse_header()` with a synthetic buffer that has moov
    /// appended after the download data, call this with `download_len` to
    /// prevent `read_sample()` from reading moov bytes as video/audio data.
    ///
    /// Reads at byte positions >= `limit` will return EOF, causing `read_sample()`
    /// to fail gracefully instead of returning corrupt data.
    pub fn set_data_limit(&mut self, limit: u64) {
        if let Some(ref handle) = self.read_limit {
            handle.set(limit);
        }
    }

    /// Parse an MP4 header from a Range-seeked buffer with virtual offset.
    ///
    /// Buffer layout: `[header_data (ftyp + mdat_header_empty + moov)][range_data]`
    ///
    /// - `virtual_base`: file offset where `range_data` starts in the original file
    /// - `header_end`: physical byte in the buffer where `range_data` begins
    ///
    /// The cursor remaps stco/co64 absolute offsets ≥ `virtual_base` to physical
    /// positions within the range_data region of the buffer.
    ///
    /// After parsing, `set_data_limit(header_end + range_data.len())` is set
    /// automatically to prevent reads past the range data.
    pub fn parse_header_range(
        &mut self,
        data: Vec<u8>,
        virtual_base: u64,
        header_end: u64,
    ) -> Result<MediaInfo, DemuxError> {
        let data_limit = data.len() as u64;
        // Tell the mp4 crate the "file size" is only the header region.
        // This prevents it from scanning range_data bytes as top-level boxes
        // (which would cause "box with a larger size than it" errors).
        // Sample reads still work because seek() remaps positions into range_data.
        let size = header_end;
        let (cursor, limit_handle) = LimitedCursor::new_with_offset(data, virtual_base, header_end);

        let reader = mp4::Mp4Reader::read_header(cursor, size)
            .map_err(|e| DemuxError::InvalidData(format!("MP4 Range parse error: {}", e)))?;

        // Set data limit to prevent reading past range_data into moov
        // (moov is in the header region, but stco seeks go to range_data region)
        limit_handle.set(data_limit);
        self.read_limit = Some(limit_handle);

        let mut video_tracks = Vec::new();
        let mut audio_tracks = Vec::new();
        let mut sample_cursors = Vec::new();

        for track_id in reader.tracks().keys().copied().collect::<Vec<_>>() {
            let track = reader.tracks().get(&track_id).unwrap();

            match track.track_type() {
                Ok(mp4::TrackType::Video) => {
                    let codec_string = Self::build_video_codec_string(track);
                    let codec_config = Self::extract_video_codec_config(track);

                    let fps = if track.duration().as_secs_f64() > 0.0 {
                        Some(track.sample_count() as f64 / track.duration().as_secs_f64())
                    } else {
                        None
                    };

                    video_tracks.push(VideoTrackInfo {
                        track_id,
                        codec_string,
                        width: track.width() as u32,
                        height: track.height() as u32,
                        fps,
                        codec_config,
                    });
                    sample_cursors.push((track_id, 1));
                }
                Ok(mp4::TrackType::Audio) => {
                    let codec_string = Self::build_audio_codec_string(track);
                    let codec_config = Self::extract_audio_codec_config(track);

                    let channel_count = track
                        .channel_config()
                        .map(|c| match c {
                            mp4::ChannelConfig::Mono => 1u32,
                            mp4::ChannelConfig::Stereo => 2,
                            mp4::ChannelConfig::Three => 3,
                            mp4::ChannelConfig::Four => 4,
                            mp4::ChannelConfig::Five => 5,
                            mp4::ChannelConfig::FiveOne => 6,
                            mp4::ChannelConfig::SevenOne => 8,
                        })
                        .unwrap_or(2);

                    audio_tracks.push(AudioTrackInfo {
                        track_id,
                        codec_string,
                        sample_rate: track
                            .sample_freq_index()
                            .map(|s| s.freq() as u32)
                            .unwrap_or(44100),
                        channels: channel_count,
                        codec_config,
                    });
                    sample_cursors.push((track_id, 1));
                }
                _ => {}
            }
        }

        let duration_us = reader.duration().as_micros().try_into().ok();

        let info = MediaInfo {
            container: ContainerFormat::Mp4,
            duration_us,
            video_tracks,
            audio_tracks,
        };

        self.media_info = Some(info.clone());
        self.sample_cursors = sample_cursors;
        self.reader = Some(reader);

        Ok(info)
    }

    /// Scan top-level MP4 boxes from raw data without parsing their content.
    /// Each box has an 8-byte header: [4 bytes size][4 bytes type].
    /// If size == 1, an extended 64-bit size follows (8 more bytes).
    /// If size == 0, the box extends to EOF.
    pub fn scan_top_level_boxes(data: &[u8]) -> Vec<Mp4Box> {
        let mut boxes = Vec::new();
        let mut pos: u64 = 0;
        let len = data.len() as u64;

        while pos + 8 <= len {
            let i = pos as usize;
            let size_u32 = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            let box_type: [u8; 4] = [data[i + 4], data[i + 5], data[i + 6], data[i + 7]];

            let size: u64 = if size_u32 == 1 {
                // Extended size (64-bit) follows the type
                if pos + 16 > len {
                    break; // Not enough data for extended header
                }
                let ext = &data[(i + 8)..(i + 16)];
                u64::from_be_bytes([ext[0], ext[1], ext[2], ext[3], ext[4], ext[5], ext[6], ext[7]])
            } else if size_u32 == 0 {
                // Box extends to EOF
                0
            } else {
                size_u32 as u64
            };

            boxes.push(Mp4Box {
                box_type,
                offset: pos,
                size,
            });

            // Advance to next box
            if size == 0 {
                break; // Rest of file is this box
            }
            pos += size;
        }

        boxes
    }

    /// Determine where the moov box is located.
    /// Call this with the first few KB of data to decide if Range request is needed.
    pub fn locate_moov(data: &[u8], file_size: u64) -> MoovLocation {
        let boxes = Self::scan_top_level_boxes(data);
        let mut found_mdat = false;
        let mut mdat_end: u64 = 0;

        for b in &boxes {
            if b.is_type(b"moov") {
                return MoovLocation::Found {
                    offset: b.offset,
                    size: b.size,
                };
            }
            if b.is_type(b"mdat") {
                found_mdat = true;
                if b.size > 0 {
                    mdat_end = b.offset + b.size;
                } else {
                    // mdat extends to EOF — moov must be before it (already found) or absent
                    return MoovLocation::Unknown;
                }
            }
        }

        if found_mdat && mdat_end > 0 {
            // mdat found without moov — moov is at mdat_end (or later)
            // If mdat_end >= file_size, something is wrong
            if mdat_end < file_size {
                return MoovLocation::AtEnd {
                    moov_offset: mdat_end,
                };
            }
        }

        MoovLocation::Unknown
    }

    /// Build a WebCodecs-compatible codec string from MP4 track info.
    fn build_video_codec_string(mp4_track: &mp4::Mp4Track) -> String {
        match mp4_track.media_type() {
            Ok(mp4::MediaType::H264) => {
                // Try to get avcC for precise codec string
                if let Some(avc1) = mp4_track
                    .trak
                    .mdia
                    .minf
                    .stbl
                    .stsd
                    .avc1
                    .as_ref()
                {
                    let avcc = &avc1.avcc;
                    format!(
                        "avc1.{:02X}{:02X}{:02X}",
                        avcc.avc_profile_indication,
                        avcc.profile_compatibility,
                        avcc.avc_level_indication
                    )
                } else {
                    "avc1.640029".to_string()
                }
            }
            Ok(mp4::MediaType::H265) => "hvc1.1.6.L93.B0".to_string(),
            Ok(mp4::MediaType::VP9) => "vp09.00.10.08".to_string(),
            _ => "avc1.640029".to_string(),
        }
    }

    fn build_audio_codec_string(mp4_track: &mp4::Mp4Track) -> String {
        match mp4_track.media_type() {
            Ok(mp4::MediaType::AAC) => {
                let object_type = mp4_track
                    .trak
                    .mdia
                    .minf
                    .stbl
                    .stsd
                    .mp4a
                    .as_ref()
                    .and_then(|mp4a| mp4a.esds.as_ref())
                    .map(|esds| esds.es_desc.dec_config.object_type_indication)
                    .unwrap_or(0x40);
                if object_type == 0x40 {
                    "mp4a.40.2".to_string() // AAC-LC
                } else {
                    format!("mp4a.{:02x}.2", object_type)
                }
            }
            _ => "mp4a.40.2".to_string(),
        }
    }

    /// Extract the full AVCDecoderConfigurationRecord for WebCodecs description.
    ///
    /// WebCodecs VideoDecoder.configure() requires the raw avcC record as
    /// the `description` field — not just SPS/PPS bytes.
    fn extract_video_codec_config(mp4_track: &mp4::Mp4Track) -> Vec<u8> {
        if let Some(avc1) = mp4_track.trak.mdia.minf.stbl.stsd.avc1.as_ref() {
            let avcc = &avc1.avcc;
            let mut config = Vec::new();

            // AVCDecoderConfigurationRecord header
            config.push(avcc.configuration_version);
            config.push(avcc.avc_profile_indication);
            config.push(avcc.profile_compatibility);
            config.push(avcc.avc_level_indication);
            config.push(avcc.length_size_minus_one | 0xFC); // upper 6 bits reserved = 1

            // SPS array
            config.push(avcc.sequence_parameter_sets.len() as u8 | 0xE0); // upper 3 bits reserved = 1
            for sps in &avcc.sequence_parameter_sets {
                config.extend_from_slice(&(sps.bytes.len() as u16).to_be_bytes());
                config.extend_from_slice(&sps.bytes);
            }

            // PPS array
            config.push(avcc.picture_parameter_sets.len() as u8);
            for pps in &avcc.picture_parameter_sets {
                config.extend_from_slice(&(pps.bytes.len() as u16).to_be_bytes());
                config.extend_from_slice(&pps.bytes);
            }

            config
        } else {
            Vec::new()
        }
    }

    /// Extract the AudioSpecificConfig (2 bytes) for WebCodecs AudioDecoder.
    fn extract_audio_codec_config(mp4_track: &mp4::Mp4Track) -> Vec<u8> {
        if let Some(mp4a) = mp4_track.trak.mdia.minf.stbl.stsd.mp4a.as_ref() {
            if let Some(esds) = &mp4a.esds {
                let dsc = &esds.es_desc.dec_config.dec_specific;
                // Rebuild the 2-byte AudioSpecificConfig from parsed fields
                let byte_a = (dsc.profile << 3) | (dsc.freq_index >> 1);
                let byte_b = (dsc.freq_index << 7) | (dsc.chan_conf << 3);
                return vec![byte_a, byte_b];
            }
        }
        Vec::new()
    }

    /// Get current sample cursor positions for resume after re-creation.
    pub fn sample_positions(&self) -> Vec<(u32, u32)> {
        self.sample_cursors.clone()
    }

    /// Set sample cursor positions to resume demuxing from a known point.
    pub fn set_sample_positions(&mut self, cursors: Vec<(u32, u32)>) {
        self.sample_cursors = cursors;
    }

    /// Build a seek index from the parsed MP4 header.
    ///
    /// Extracts sync sample positions (stss) and computes their timestamps
    /// (from stts) and absolute byte offsets (from stsc + stco/co64 + stsz).
    ///
    /// Must be called after `parse_header()`.
    pub fn build_seek_index(&self) -> SeekIndex {
        let reader = match &self.reader {
            Some(r) => r,
            None => return SeekIndex::new(),
        };

        let mut entries = Vec::new();

        for &track_id in reader.tracks().keys() {
            let track = &reader.tracks()[&track_id];

            // Only index video tracks
            if !matches!(track.track_type(), Ok(mp4::TrackType::Video)) {
                continue;
            }

            let timescale = track.timescale();
            if timescale == 0 {
                continue;
            }

            let stbl = &track.trak.mdia.minf.stbl;

            // Get sync sample IDs from stss. If no stss box, every sample
            // is a sync sample — skip indexing (too many entries).
            let sync_ids = match &stbl.stss {
                Some(stss) => &stss.entries,
                None => continue,
            };

            for &sample_id in sync_ids {
                if sample_id < 1 || sample_id > track.sample_count() {
                    continue;
                }

                // Compute timestamp in microseconds from stts
                let timestamp_us = Self::compute_sample_time_us(
                    track,
                    sample_id,
                    timescale,
                );

                // Compute absolute byte offset from stsc + stco/co64 + stsz
                if let Some(byte_offset) = Self::compute_sample_offset(track, sample_id) {
                    entries.push(SeekEntry {
                        timestamp_us,
                        byte_offset,
                    });
                }
            }

            // Only index the first video track
            break;
        }

        SeekIndex::from_entries(entries)
    }

    /// Compute the decode timestamp of a sample in microseconds.
    ///
    /// Walks the stts (decoding time-to-sample) table to accumulate
    /// elapsed time up to the given sample_id (1-based).
    fn compute_sample_time_us(
        track: &mp4::Mp4Track,
        sample_id: u32,
        timescale: u32,
    ) -> i64 {
        let mut elapsed: u64 = 0;
        let mut current_sample: u32 = 1;

        for entry in &track.trak.mdia.minf.stbl.stts.entries {
            let end_sample = current_sample + entry.sample_count;
            if sample_id < end_sample {
                // Target sample is within this stts entry
                elapsed += (sample_id - current_sample) as u64 * entry.sample_delta as u64;
                break;
            }
            elapsed += entry.sample_count as u64 * entry.sample_delta as u64;
            current_sample = end_sample;
        }

        // Convert from track timescale to microseconds
        (elapsed as i64 * 1_000_000) / timescale as i64
    }

    /// Compute the absolute byte offset of a sample in the file.
    ///
    /// Uses stsc (sample-to-chunk), stco/co64 (chunk offsets), and
    /// stsz (sample sizes) to locate the exact position.
    fn compute_sample_offset(track: &mp4::Mp4Track, sample_id: u32) -> Option<u64> {
        let stbl = &track.trak.mdia.minf.stbl;
        let stsc = &stbl.stsc;

        // Find the stsc entry covering this sample_id.
        // stsc entries have first_sample pre-computed by the mp4 crate.
        let stsc_idx = stsc
            .entries
            .iter()
            .rposition(|e| sample_id >= e.first_sample)?;

        let stsc_entry = &stsc.entries[stsc_idx];
        let first_chunk = stsc_entry.first_chunk;
        let first_sample = stsc_entry.first_sample;
        let samples_per_chunk = stsc_entry.samples_per_chunk;

        if samples_per_chunk == 0 {
            return None;
        }

        // Which chunk does this sample belong to?
        let chunk_id = first_chunk + (sample_id - first_sample) / samples_per_chunk;

        // Get chunk byte offset from stco or co64 (1-based chunk_id)
        let chunk_offset = if let Some(ref stco) = stbl.stco {
            stco.entries.get(chunk_id as usize - 1).map(|&o| o as u64)
        } else if let Some(ref co64) = stbl.co64 {
            co64.entries.get(chunk_id as usize - 1).copied()
        } else {
            None
        }?;

        // Compute intra-chunk offset: sum sizes of preceding samples in this chunk
        let first_sample_in_chunk =
            sample_id - (sample_id - first_sample) % samples_per_chunk;

        let stsz = &stbl.stsz;
        let mut intra_offset: u64 = 0;
        for i in first_sample_in_chunk..sample_id {
            let size = if stsz.sample_size > 0 {
                stsz.sample_size
            } else {
                *stsz.sample_sizes.get(i as usize - 1)?
            };
            intra_offset += size as u64;
        }

        Some(chunk_offset + intra_offset)
    }
}

impl Demuxer for Mp4Demuxer {
    fn probe(data: &[u8]) -> bool {
        if data.len() < 8 {
            return false;
        }
        &data[4..8] == b"ftyp"
    }

    fn parse_header(&mut self, data: &[u8]) -> Result<MediaInfo, DemuxError> {
        let (cursor, limit_handle) = LimitedCursor::new(data.to_vec());
        let size = data.len() as u64;

        // During read_header, the limit is u64::MAX (unlimited) so moov can be read.
        // After parsing, the caller should call set_data_limit(download_len) to
        // prevent read_sample from accessing the moov appendage as sample data.
        let reader = mp4::Mp4Reader::read_header(cursor, size)
            .map_err(|e| DemuxError::InvalidData(format!("MP4 parse error: {}", e)))?;
        self.read_limit = Some(limit_handle);

        let mut video_tracks = Vec::new();
        let mut audio_tracks = Vec::new();
        let mut sample_cursors = Vec::new();

        for track_id in reader.tracks().keys().copied().collect::<Vec<_>>() {
            let track = reader.tracks().get(&track_id).unwrap();

            match track.track_type() {
                Ok(mp4::TrackType::Video) => {
                    let codec_string = Self::build_video_codec_string(track);
                    let codec_config = Self::extract_video_codec_config(track);

                    let fps = if track.duration().as_secs_f64() > 0.0 {
                        Some(track.sample_count() as f64 / track.duration().as_secs_f64())
                    } else {
                        None
                    };

                    video_tracks.push(VideoTrackInfo {
                        track_id,
                        codec_string,
                        width: track.width() as u32,
                        height: track.height() as u32,
                        fps,
                        codec_config,
                    });
                    sample_cursors.push((track_id, 1));
                }
                Ok(mp4::TrackType::Audio) => {
                    let codec_string = Self::build_audio_codec_string(track);
                    let codec_config = Self::extract_audio_codec_config(track);

                    let channel_count = track
                        .channel_config()
                        .map(|c| match c {
                            mp4::ChannelConfig::Mono => 1u32,
                            mp4::ChannelConfig::Stereo => 2,
                            mp4::ChannelConfig::Three => 3,
                            mp4::ChannelConfig::Four => 4,
                            mp4::ChannelConfig::Five => 5,
                            mp4::ChannelConfig::FiveOne => 6,
                            mp4::ChannelConfig::SevenOne => 8,
                        })
                        .unwrap_or(2);

                    audio_tracks.push(AudioTrackInfo {
                        track_id,
                        codec_string,
                        sample_rate: track
                            .sample_freq_index()
                            .map(|s| s.freq() as u32)
                            .unwrap_or(44100),
                        channels: channel_count,
                        codec_config,
                    });
                    sample_cursors.push((track_id, 1));
                }
                _ => {}
            }
        }

        let duration_us = reader.duration().as_micros().try_into().ok();

        let info = MediaInfo {
            container: ContainerFormat::Mp4,
            duration_us,
            video_tracks,
            audio_tracks,
        };

        self.media_info = Some(info.clone());
        self.sample_cursors = sample_cursors;
        self.reader = Some(reader);

        Ok(info)
    }

    fn next_chunk(&mut self) -> Result<Option<EncodedChunk>, DemuxError> {
        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| DemuxError::InvalidData("No header parsed yet".into()))?;

        // Pick the chunk with the earliest timestamp across all tracks
        let mut best: Option<(usize, EncodedChunk)> = None;

        for (cursor_idx, (track_id, sample_idx)) in self.sample_cursors.iter().enumerate() {
            let track_id = *track_id;
            let sample_idx = *sample_idx;

            let track = match reader.tracks().get(&track_id) {
                Some(t) => t,
                None => continue,
            };

            if sample_idx > track.sample_count() {
                continue;
            }

            let is_video = track
                .track_type()
                .map(|t| t == mp4::TrackType::Video)
                .unwrap_or(false);
            let timescale = track.timescale();

            if let Ok(Some(sample)) = reader.read_sample(track_id, sample_idx) {
                let timestamp_us = if timescale > 0 {
                    (sample.start_time as i64 * 1_000_000) / timescale as i64
                } else {
                    0
                };
                let duration_us = if timescale > 0 {
                    (sample.duration as i64 * 1_000_000) / timescale as i64
                } else {
                    0
                };

                let chunk = EncodedChunk {
                    track_id,
                    is_video,
                    is_audio: !is_video,
                    is_keyframe: sample.is_sync,
                    timestamp_us,
                    duration_us,
                    data: sample.bytes.to_vec(),
                };

                let is_earlier = match &best {
                    Some((_, existing)) => chunk.timestamp_us < existing.timestamp_us,
                    None => true,
                };

                if is_earlier {
                    best = Some((cursor_idx, chunk));
                }
            }
        }

        match best {
            Some((cursor_idx, chunk)) => {
                self.sample_cursors[cursor_idx].1 += 1;
                Ok(Some(chunk))
            }
            None => Ok(None),
        }
    }

    fn seek_to_keyframe(&mut self, timestamp_us: i64) -> Result<(), DemuxError> {
        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| DemuxError::InvalidData("No header parsed yet".into()))?;

        for (track_id, sample_idx) in self.sample_cursors.iter_mut() {
            let track = match reader.tracks().get(track_id) {
                Some(t) => t,
                None => continue,
            };

            let timescale = track.timescale();
            let target_time = if timescale > 0 {
                (timestamp_us as u64 * timescale as u64) / 1_000_000
            } else {
                0
            };

            // Find nearest sync sample before target_time
            let mut best_sync = 1u32;
            for i in 1..=track.sample_count() {
                if let Ok(Some(sample)) = reader.read_sample(*track_id, i) {
                    if sample.is_sync && sample.start_time <= target_time {
                        best_sync = i;
                    }
                    if sample.start_time > target_time {
                        break;
                    }
                }
            }

            *sample_idx = best_sync;
        }

        Ok(())
    }

    fn build_seek_index(&self) -> SeekIndex {
        // Delegates to the inherent method on Mp4Demuxer
        Mp4Demuxer::build_seek_index(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =============================================
    // Helper: build a top-level MP4 box (8-byte header)
    // =============================================
    fn make_box(box_type: &[u8; 4], content_size: u32) -> Vec<u8> {
        let total_size = content_size + 8;
        let mut data = total_size.to_be_bytes().to_vec();
        data.extend_from_slice(box_type);
        data.extend(vec![0u8; content_size as usize]);
        data
    }

    /// Build a box with extended 64-bit size header (16 bytes)
    fn make_box_extended(box_type: &[u8; 4], total_size: u64) -> Vec<u8> {
        let mut data = 1u32.to_be_bytes().to_vec(); // size=1 signals extended
        data.extend_from_slice(box_type);
        data.extend_from_slice(&total_size.to_be_bytes());
        let content_size = total_size.saturating_sub(16) as usize;
        data.extend(vec![0u8; content_size]);
        data
    }

    // =============================================
    // scan_top_level_boxes tests
    // =============================================

    #[test]
    fn scan_empty_data() {
        let boxes = Mp4Demuxer::scan_top_level_boxes(&[]);
        assert!(boxes.is_empty());
    }

    #[test]
    fn scan_too_short() {
        let boxes = Mp4Demuxer::scan_top_level_boxes(&[0x00, 0x01, 0x02, 0x03]);
        assert!(boxes.is_empty());
    }

    #[test]
    fn scan_single_ftyp_box() {
        let data = make_box(b"ftyp", 12); // 20 bytes total
        let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
        assert_eq!(boxes.len(), 1);
        assert!(boxes[0].is_type(b"ftyp"));
        assert_eq!(boxes[0].offset, 0);
        assert_eq!(boxes[0].size, 20);
    }

    #[test]
    fn scan_multiple_boxes() {
        let mut data = make_box(b"ftyp", 12); // 20 bytes
        data.extend(make_box(b"moov", 100)); // 108 bytes
        data.extend(make_box(b"mdat", 50));  // 58 bytes
        let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
        assert_eq!(boxes.len(), 3);
        assert!(boxes[0].is_type(b"ftyp"));
        assert_eq!(boxes[0].offset, 0);
        assert!(boxes[1].is_type(b"moov"));
        assert_eq!(boxes[1].offset, 20);
        assert!(boxes[2].is_type(b"mdat"));
        assert_eq!(boxes[2].offset, 128);
    }

    #[test]
    fn scan_extended_size_box() {
        let data = make_box_extended(b"mdat", 32); // 32 bytes total
        let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
        assert_eq!(boxes.len(), 1);
        assert!(boxes[0].is_type(b"mdat"));
        assert_eq!(boxes[0].size, 32);
    }

    #[test]
    fn scan_size_zero_extends_to_eof() {
        // size=0 means "extends to end of file"
        let mut data = vec![0x00, 0x00, 0x00, 0x00]; // size = 0
        data.extend_from_slice(b"mdat");
        data.extend(vec![0xFF; 100]); // content
        let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
        assert_eq!(boxes.len(), 1);
        assert!(boxes[0].is_type(b"mdat"));
        assert_eq!(boxes[0].size, 0);
    }

    #[test]
    fn scan_size_zero_stops_scanning() {
        // After size=0 box, no more boxes should be scanned
        let mut data = vec![0x00, 0x00, 0x00, 0x00]; // size = 0
        data.extend_from_slice(b"mdat");
        data.extend(vec![0x00; 50]);
        // Put another "box" after — should not be found
        data.extend(make_box(b"moov", 10));
        let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
        assert_eq!(boxes.len(), 1);
    }

    #[test]
    fn scan_truncated_box_header() {
        // 7 bytes — not enough for a complete header
        let data = [0x00, 0x00, 0x00, 0x1C, b'f', b't', b'y'];
        let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
        assert!(boxes.is_empty());
    }

    #[test]
    fn scan_truncated_extended_header() {
        // Extended size header but not enough bytes for the 64-bit size
        let mut data = 1u32.to_be_bytes().to_vec(); // size=1 (extended)
        data.extend_from_slice(b"mdat");
        data.extend_from_slice(&[0x00, 0x00, 0x00]); // only 3 of 8 bytes
        let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
        assert!(boxes.is_empty());
    }

    #[test]
    fn scan_box_type_str() {
        let b = Mp4Box {
            box_type: *b"moov",
            offset: 0,
            size: 100,
        };
        assert_eq!(b.type_str(), "moov");
    }

    #[test]
    fn scan_recognizes_common_box_types() {
        let types = [b"ftyp", b"moov", b"mdat", b"moof", b"free", b"skip"];
        let mut data = Vec::new();
        for t in &types {
            data.extend(make_box(t, 0)); // 8 bytes each
        }
        let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
        assert_eq!(boxes.len(), types.len());
        for (i, t) in types.iter().enumerate() {
            assert!(boxes[i].is_type(t));
        }
    }

    // =============================================
    // locate_moov tests
    // =============================================

    #[test]
    fn locate_moov_found_before_mdat() {
        let mut data = make_box(b"ftyp", 12);
        data.extend(make_box(b"moov", 100));
        data.extend(make_box(b"mdat", 1000));
        let file_size = data.len() as u64;
        match Mp4Demuxer::locate_moov(&data, file_size) {
            MoovLocation::Found { offset, size } => {
                assert_eq!(offset, 20);
                assert_eq!(size, 108);
            }
            other => panic!("Expected Found, got {:?}", other),
        }
    }

    #[test]
    fn locate_moov_at_end() {
        // ftyp + mdat only — moov missing in scanned data
        let mut data = make_box(b"ftyp", 12);   // 20 bytes
        data.extend(make_box(b"mdat", 10000)); // 10008 bytes
        let file_size = 20000u64; // file is larger — moov at end
        match Mp4Demuxer::locate_moov(&data, file_size) {
            MoovLocation::AtEnd { moov_offset } => {
                // mdat ends at 20 + 10008 = 10028
                assert_eq!(moov_offset, 10028);
            }
            other => panic!("Expected AtEnd, got {:?}", other),
        }
    }

    #[test]
    fn locate_moov_unknown_no_boxes() {
        let data = [0x00; 100]; // No valid boxes
        match Mp4Demuxer::locate_moov(&data, 1000) {
            MoovLocation::Unknown => {}
            other => panic!("Expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn locate_moov_unknown_no_mdat_no_moov() {
        // Only ftyp, no mdat or moov
        let data = make_box(b"ftyp", 12);
        match Mp4Demuxer::locate_moov(&data, 1000) {
            MoovLocation::Unknown => {}
            other => panic!("Expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn locate_moov_mdat_size_zero() {
        // mdat with size=0 (extends to EOF) — should return Unknown
        let mut data = make_box(b"ftyp", 12);
        // Add mdat with size=0
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // size=0
        data.extend_from_slice(b"mdat");
        match Mp4Demuxer::locate_moov(&data, 10000) {
            MoovLocation::Unknown => {}
            other => panic!("Expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn locate_moov_mdat_end_equals_file_size() {
        // mdat fills the rest of the file — no room for moov
        let mut data = make_box(b"ftyp", 4);  // 12 bytes
        data.extend(make_box(b"mdat", 0));   // 8 bytes
        let file_size = 20u64;
        match Mp4Demuxer::locate_moov(&data, file_size) {
            MoovLocation::Unknown => {}
            other => panic!("Expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn locate_moov_empty_data() {
        match Mp4Demuxer::locate_moov(&[], 1000) {
            MoovLocation::Unknown => {}
            other => panic!("Expected Unknown, got {:?}", other),
        }
    }

    // =============================================
    // Mp4Demuxer::probe tests
    // =============================================

    #[test]
    fn mp4_probe_valid_ftyp() {
        let data = [
            0x00, 0x00, 0x00, 0x1C, b'f', b't', b'y', b'p',
            b'i', b's', b'o', b'm',
        ];
        assert!(Mp4Demuxer::probe(&data));
    }

    #[test]
    fn mp4_probe_empty() {
        assert!(!Mp4Demuxer::probe(&[]));
    }

    #[test]
    fn mp4_probe_too_short() {
        assert!(!Mp4Demuxer::probe(&[0x00, 0x00, 0x00, 0x1C, b'f', b't', b'y']));
    }

    #[test]
    fn mp4_probe_wrong_type() {
        let data = [
            0x00, 0x00, 0x00, 0x1C, b'm', b'o', b'o', b'v',
        ];
        assert!(!Mp4Demuxer::probe(&data));
    }

    #[test]
    fn mp4_probe_mkv_magic() {
        assert!(!Mp4Demuxer::probe(&[0x1A, 0x45, 0xDF, 0xA3, 0x93, 0x42, 0x86, 0x81]));
    }

    // =============================================
    // Mp4Demuxer state tests
    // =============================================

    #[test]
    fn mp4_demuxer_initial_state() {
        let d = Mp4Demuxer::new();
        assert!(d.reader.is_none());
        assert!(d.media_info.is_none());
        assert!(d.sample_cursors.is_empty());
    }

    #[test]
    fn mp4_demuxer_sample_positions_roundtrip() {
        let mut d = Mp4Demuxer::new();
        let positions = vec![(1, 42), (2, 100), (3, 1)];
        d.set_sample_positions(positions.clone());
        assert_eq!(d.sample_positions(), positions);
    }

    #[test]
    fn mp4_demuxer_sample_positions_empty() {
        let d = Mp4Demuxer::new();
        assert!(d.sample_positions().is_empty());
    }

    #[test]
    fn mp4_demuxer_next_chunk_without_parse_errors() {
        let mut d = Mp4Demuxer::new();
        let result = d.next_chunk();
        assert!(result.is_err());
    }

    #[test]
    fn mp4_demuxer_seek_without_parse_errors() {
        let mut d = Mp4Demuxer::new();
        let result = d.seek_to_keyframe(0);
        assert!(result.is_err());
    }

    #[test]
    fn mp4_demuxer_parse_invalid_data() {
        let mut d = Mp4Demuxer::new();
        let result = d.parse_header(&[0x00; 100]);
        assert!(result.is_err());
    }

    #[test]
    fn mp4_demuxer_parse_truncated_ftyp() {
        let mut d = Mp4Demuxer::new();
        // Valid ftyp header but truncated
        let data = [
            0x00, 0x00, 0x00, 0x1C, b'f', b't', b'y', b'p',
            b'i', b's', b'o', b'm',
        ];
        let result = d.parse_header(&data);
        assert!(result.is_err());
    }

    // =============================================
    // Edge cases: box scanning with large sizes
    // =============================================

    #[test]
    fn scan_box_with_max_u32_size() {
        // A box claiming to be 4GB — scanning should handle it (advance past)
        let mut data = (0xFFFFFFFFu32).to_be_bytes().to_vec();
        data.extend_from_slice(b"mdat");
        // No content (we don't have 4GB), but scan should find this one box
        let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
        assert_eq!(boxes.len(), 1);
        assert!(boxes[0].is_type(b"mdat"));
        assert_eq!(boxes[0].size, 0xFFFFFFFF);
    }

    #[test]
    fn scan_adjacent_minimal_boxes() {
        // Multiple 8-byte boxes (minimum size = 8, no content)
        let mut data = Vec::new();
        for _ in 0..10 {
            data.extend(make_box(b"free", 0)); // 8 bytes each
        }
        let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
        assert_eq!(boxes.len(), 10);
        for (i, b) in boxes.iter().enumerate() {
            assert_eq!(b.offset, (i * 8) as u64);
            assert_eq!(b.size, 8);
        }
    }

    // =============================================
    // locate_moov — additional edge cases
    // =============================================

    #[test]
    fn locate_moov_multiple_mdat_before_moov() {
        // ftyp + mdat + free + moov (moov found despite mdat before it)
        let mut data = make_box(b"ftyp", 12);  // 20 bytes
        data.extend(make_box(b"mdat", 100));    // 108 bytes
        data.extend(make_box(b"free", 8));      // 16 bytes
        let moov_offset = data.len();
        data.extend(make_box(b"moov", 200));    // 208 bytes
        let file_size = data.len() as u64;
        match Mp4Demuxer::locate_moov(&data, file_size) {
            MoovLocation::Found { offset, .. } => {
                assert_eq!(offset, moov_offset as u64);
            }
            other => panic!("Expected Found, got {:?}", other),
        }
    }

    #[test]
    fn locate_moov_with_extended_mdat() {
        // ftyp + extended-size mdat — moov at end
        let mut data = make_box(b"ftyp", 12); // 20 bytes
        // mdat with extended 64-bit size
        data.extend(make_box_extended(b"mdat", 64)); // 64 bytes
        let file_size = 1000u64;
        match Mp4Demuxer::locate_moov(&data, file_size) {
            MoovLocation::AtEnd { moov_offset } => {
                // mdat starts at 20, size=64 → ends at 84
                assert_eq!(moov_offset, 84);
            }
            other => panic!("Expected AtEnd, got {:?}", other),
        }
    }

    // =============================================
    // Mp4Demuxer — build_seek_index without parse
    // =============================================

    #[test]
    fn mp4_build_seek_index_without_parse() {
        let d = Mp4Demuxer::new();
        let idx = d.build_seek_index();
        assert!(idx.is_empty());
    }

    // =============================================
    // Mp4Box utilities
    // =============================================

    #[test]
    fn mp4_box_type_non_ascii() {
        let b = Mp4Box {
            box_type: [0xFF, 0xFE, 0xFD, 0xFC],
            offset: 0,
            size: 8,
        };
        assert_eq!(b.type_str(), "????");
    }

    #[test]
    fn mp4_box_is_type_mismatch() {
        let b = Mp4Box {
            box_type: *b"moov",
            offset: 0,
            size: 100,
        };
        assert!(!b.is_type(b"mdat"));
        assert!(b.is_type(b"moov"));
    }

    // =============================================
    // scan_top_level_boxes — stress/edge cases
    // =============================================

    #[test]
    fn scan_box_chain_with_extended_in_middle() {
        let mut data = make_box(b"ftyp", 4);           // 12 bytes
        data.extend(make_box_extended(b"mdat", 32));     // 32 bytes
        data.extend(make_box(b"moov", 8));               // 16 bytes
        let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
        assert_eq!(boxes.len(), 3);
        assert!(boxes[0].is_type(b"ftyp"));
        assert_eq!(boxes[0].offset, 0);
        assert!(boxes[1].is_type(b"mdat"));
        assert_eq!(boxes[1].offset, 12);
        assert_eq!(boxes[1].size, 32);
        assert!(boxes[2].is_type(b"moov"));
        assert_eq!(boxes[2].offset, 44);
    }
}
