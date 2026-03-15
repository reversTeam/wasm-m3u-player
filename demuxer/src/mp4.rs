use std::io::Cursor;

use crate::types::*;

/// MP4 demuxer using the `mp4` crate.
pub struct Mp4Demuxer {
    reader: Option<mp4::Mp4Reader<Cursor<Vec<u8>>>>,
    media_info: Option<MediaInfo>,
    /// Current sample indices per track (track_id -> next sample index, 1-based).
    sample_cursors: Vec<(u32, u32)>,
}

impl Mp4Demuxer {
    pub fn new() -> Self {
        Self {
            reader: None,
            media_info: None,
            sample_cursors: Vec::new(),
        }
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

    /// Extract codec config bytes (SPS+PPS for H264).
    fn extract_video_codec_config(mp4_track: &mp4::Mp4Track) -> Vec<u8> {
        if let Some(avc1) = mp4_track.trak.mdia.minf.stbl.stsd.avc1.as_ref() {
            let avcc = &avc1.avcc;
            let mut config = Vec::new();

            for sps in &avcc.sequence_parameter_sets {
                config.extend_from_slice(&(sps.bytes.len() as u16).to_be_bytes());
                config.extend_from_slice(&sps.bytes);
            }
            for pps in &avcc.picture_parameter_sets {
                config.extend_from_slice(&(pps.bytes.len() as u16).to_be_bytes());
                config.extend_from_slice(&pps.bytes);
            }
            config
        } else {
            Vec::new()
        }
    }

    fn extract_audio_codec_config(mp4_track: &mp4::Mp4Track) -> Vec<u8> {
        if let Some(mp4a) = mp4_track.trak.mdia.minf.stbl.stsd.mp4a.as_ref() {
            if let Some(esds) = &mp4a.esds {
                return esds
                    .es_desc
                    .dec_config
                    .dec_specific
                    .profile
                    .to_be_bytes()
                    .to_vec();
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
}

impl Demuxer for Mp4Demuxer {
    fn probe(data: &[u8]) -> bool {
        if data.len() < 8 {
            return false;
        }
        &data[4..8] == b"ftyp"
    }

    fn parse_header(&mut self, data: &[u8]) -> Result<MediaInfo, DemuxError> {
        let cursor = Cursor::new(data.to_vec());
        let size = data.len() as u64;

        let reader = mp4::Mp4Reader::read_header(cursor, size)
            .map_err(|e| DemuxError::InvalidData(format!("MP4 parse error: {}", e)))?;

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
}
