use std::io::Cursor;

use matroska_demuxer::{Frame, MatroskaFile, TrackType};

use crate::types::*;

/// MKV/WebM demuxer using the `matroska-demuxer` crate.
pub struct MkvDemuxer {
    mkv: Option<MatroskaFile<Cursor<Vec<u8>>>>,
    media_info: Option<MediaInfo>,
    video_track_ids: Vec<u64>,
    audio_track_ids: Vec<u64>,
}

impl MkvDemuxer {
    pub fn new() -> Self {
        Self {
            mkv: None,
            media_info: None,
            video_track_ids: Vec::new(),
            audio_track_ids: Vec::new(),
        }
    }

    /// Map Matroska codec ID to WebCodecs-compatible codec string.
    fn map_video_codec(codec_id: &str, codec_private: &Option<Vec<u8>>) -> String {
        match codec_id {
            "V_MPEG4/ISO/AVC" => {
                // Try to extract profile from codec_private (avcC box)
                if let Some(data) = codec_private {
                    if data.len() >= 4 {
                        return format!(
                            "avc1.{:02X}{:02X}{:02X}",
                            data[1], data[2], data[3]
                        );
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

    fn map_audio_codec(codec_id: &str) -> String {
        match codec_id {
            "A_AAC" | "A_AAC/MPEG2/LC" | "A_AAC/MPEG4/LC" => "mp4a.40.2".to_string(),
            "A_AAC/MPEG4/SBR" => "mp4a.40.5".to_string(),
            "A_OPUS" => "opus".to_string(),
            "A_VORBIS" => "vorbis".to_string(),
            "A_FLAC" => "flac".to_string(),
            "A_AC3" | "A_EAC3" => "ac-3".to_string(),
            _ => format!("unknown:{}", codec_id),
        }
    }
}

impl Demuxer for MkvDemuxer {
    fn probe(data: &[u8]) -> bool {
        data.len() >= 4 && data[0] == 0x1A && data[1] == 0x45 && data[2] == 0xDF && data[3] == 0xA3
    }

    fn parse_header(&mut self, data: &[u8]) -> Result<MediaInfo, DemuxError> {
        let cursor = Cursor::new(data.to_vec());

        let mkv = MatroskaFile::open(cursor)
            .map_err(|e| DemuxError::InvalidData(format!("MKV parse error: {}", e)))?;

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
                        fps: track.default_duration().map(|d| 1_000_000_000.0 / d.get() as f64),
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
            // duration is in scaled units, convert to microseconds
            ((d * timestamp_scale) / 1_000.0) as i64
        });

        // Detect WebM vs MKV — matroska-demuxer doesn't expose doc_type directly,
        // so we default to Mkv. WebM detection can be added via magic byte analysis.
        let container = ContainerFormat::Mkv;

        let info = MediaInfo {
            container,
            duration_us,
            video_tracks,
            audio_tracks,
        };

        self.media_info = Some(info.clone());
        self.video_track_ids = video_track_ids;
        self.audio_track_ids = audio_track_ids;
        self.mkv = Some(mkv);

        Ok(info)
    }

    fn next_chunk(&mut self) -> Result<Option<EncodedChunk>, DemuxError> {
        let mkv = self
            .mkv
            .as_mut()
            .ok_or_else(|| DemuxError::InvalidData("No header parsed yet".into()))?;

        let mut frame = Frame::default();
        match mkv.next_frame(&mut frame) {
            Ok(true) => {
                let track_number = frame.track;
                let is_video = self.video_track_ids.contains(&track_number);

                let timestamp_us = frame.timestamp as i64 / 1_000; // ns -> us

                Ok(Some(EncodedChunk {
                    track_id: track_number as u32,
                    is_video,
                    is_keyframe: frame.is_keyframe.unwrap_or(false),
                    timestamp_us,
                    duration_us: frame.duration.map(|d| d as i64 / 1_000).unwrap_or(0),
                    data: frame.data,
                }))
            }
            Ok(false) => Ok(None), // EOF
            Err(e) => Err(DemuxError::InvalidData(format!("MKV frame error: {}", e))),
        }
    }

    fn seek_to_keyframe(&mut self, _timestamp_us: i64) -> Result<(), DemuxError> {
        // matroska-demuxer doesn't support seeking natively.
        // For now, return an error. A full implementation would re-parse
        // and skip frames until the target keyframe.
        Err(DemuxError::InvalidData(
            "MKV seeking not yet implemented — requires re-parsing from cues".into(),
        ))
    }
}
