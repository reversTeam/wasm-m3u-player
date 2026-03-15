use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use demuxer::{detect_format, ContainerFormat, Demuxer, Mp4Demuxer, MkvDemuxer};
use player_core::{MediaInfo, PlaybackStatus, PlayerEvent, PlayerState};

use crate::audio::AudioPipeline;
use crate::decoder::VideoDecoderWrapper;
use crate::fetch::fetch_bytes;
use crate::renderer::CanvasRenderer;
use crate::sync::AVSync;

/// The main Player struct — headless, framework-agnostic.
/// Receives a canvas from the consumer, never creates DOM elements.
#[wasm_bindgen]
pub struct Player {
    renderer: CanvasRenderer,
    video_decoder: VideoDecoderWrapper,
    audio_pipeline: AudioPipeline,
    av_sync: AVSync,
    state: PlayerState,
    event_callback: Option<js_sys::Function>,
    /// Raw demuxed data buffer (MVP: full file in memory)
    data_buffer: Option<Vec<u8>>,
    /// Demuxer state
    demuxer_format: Option<ContainerFormat>,
}

#[wasm_bindgen]
impl Player {
    /// Create a new Player attached to a canvas element.
    #[wasm_bindgen(constructor)]
    pub fn new(canvas: HtmlCanvasElement) -> Result<Player, JsValue> {
        let renderer = CanvasRenderer::new(canvas)?;

        Ok(Player {
            renderer,
            video_decoder: VideoDecoderWrapper::new(),
            audio_pipeline: AudioPipeline::new(),
            av_sync: AVSync::new(),
            state: PlayerState::default(),
            event_callback: None,
            data_buffer: None,
            demuxer_format: None,
        })
    }

    /// Register an event callback. Events are PlayerEvent objects with a `type` field.
    pub fn on_event(&mut self, callback: js_sys::Function) {
        self.event_callback = Some(callback);
    }

    /// Remove the event callback.
    pub fn off_event(&mut self) {
        self.event_callback = None;
    }

    /// Get a snapshot of the current player state.
    pub fn get_state(&self) -> JsValue {
        serde_wasm_bindgen::to_value(&self.state).unwrap_or(JsValue::NULL)
    }

    /// Load a video from a URL.
    pub async fn load(&mut self, url: String) -> Result<(), JsValue> {
        self.state.status = PlaybackStatus::Loading;
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Loading,
        });

        // Fetch the entire file (MVP — progressive streaming later)
        let data = fetch_bytes(&url).await?;

        // Detect format
        let format = detect_format(&data);
        if format == ContainerFormat::Unknown {
            let err = PlayerEvent::Error {
                message: "Unsupported video format".into(),
                recoverable: false,
            };
            self.emit_event(&err);
            self.state.status = PlaybackStatus::Error;
            return Err(JsValue::from_str("Unsupported video format"));
        }

        // Parse header with appropriate demuxer
        let media_info = match format {
            ContainerFormat::Mp4 => {
                let mut demuxer = Mp4Demuxer::new();
                demuxer.parse_header(&data).map_err(|e| JsValue::from_str(&e.to_string()))?
            }
            ContainerFormat::Mkv | ContainerFormat::WebM => {
                let mut demuxer = MkvDemuxer::new();
                demuxer.parse_header(&data).map_err(|e| JsValue::from_str(&e.to_string()))?
            }
            _ => return Err(JsValue::from_str("Unsupported format")),
        };

        // Configure video decoder
        if let Some(video_track) = media_info.video_tracks.first() {
            self.video_decoder.configure(
                &video_track.codec_string,
                video_track.width,
                video_track.height,
                Some(&video_track.codec_config),
            )?;
            self.state.has_video = true;
            self.state.video_width = video_track.width;
            self.state.video_height = video_track.height;

            self.emit_event(&PlayerEvent::VideoResized {
                width: video_track.width,
                height: video_track.height,
            });
        }

        // Configure audio decoder
        if let Some(audio_track) = media_info.audio_tracks.first() {
            self.audio_pipeline.configure(
                &audio_track.codec_string,
                audio_track.sample_rate,
                audio_track.channels,
                Some(&audio_track.codec_config),
            )?;
            self.state.has_audio = true;
            self.av_sync.set_has_audio(true);
        }

        // Build player-core MediaInfo
        let player_info = MediaInfo {
            duration_ms: media_info.duration_us.map(|us| (us / 1000) as u64),
            video_codec: media_info.video_tracks.first().map(|t| t.codec_string.clone()),
            audio_codec: media_info.audio_tracks.first().map(|t| t.codec_string.clone()),
            width: media_info.video_tracks.first().map(|t| t.width).unwrap_or(0),
            height: media_info.video_tracks.first().map(|t| t.height).unwrap_or(0),
            fps: media_info.video_tracks.first().and_then(|t| t.fps),
            sample_rate: media_info.audio_tracks.first().map(|t| t.sample_rate),
            channels: media_info.audio_tracks.first().map(|t| t.channels),
        };

        self.state.duration_ms = player_info.duration_ms;
        self.state.media_info = Some(player_info.clone());
        self.state.status = PlaybackStatus::Ready;

        self.data_buffer = Some(data);
        self.demuxer_format = Some(format);

        self.emit_event(&PlayerEvent::MediaLoaded { info: player_info });
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Ready,
        });

        Ok(())
    }

    /// Start playback.
    pub async fn play(&mut self) -> Result<(), JsValue> {
        if self.state.status != PlaybackStatus::Ready
            && self.state.status != PlaybackStatus::Paused
        {
            return Err(JsValue::from_str("Cannot play in current state"));
        }

        // Resume AudioContext (required after user interaction)
        if self.audio_pipeline.is_configured() {
            self.audio_pipeline.resume().await?;
        }

        self.state.status = PlaybackStatus::Playing;
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Playing,
        });

        // Demux and decode all frames (MVP — batch processing)
        self.process_media()?;

        Ok(())
    }

    /// Pause playback.
    pub fn pause(&mut self) {
        if self.state.status == PlaybackStatus::Playing {
            self.state.status = PlaybackStatus::Paused;
            self.emit_event(&PlayerEvent::StatusChanged {
                status: PlaybackStatus::Paused,
            });
        }
    }

    /// Stop playback and reset.
    pub fn stop(&mut self) {
        self.state.status = PlaybackStatus::Stopped;
        self.state.current_time_ms = 0;
        self.renderer.clear();
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Stopped,
        });
    }

    /// Seek to a position in milliseconds.
    pub async fn seek(&mut self, _time_ms: u64) -> Result<(), JsValue> {
        // TODO: implement seeking (requires demuxer re-initialization)
        Err(JsValue::from_str("Seeking not yet implemented"))
    }

    /// Destroy the player and release all resources.
    pub fn destroy(&mut self) {
        self.video_decoder.close();
        self.audio_pipeline.close();
        self.renderer.clear();
        self.data_buffer = None;
        self.event_callback = None;
        self.state = PlayerState::default();
    }
}

// Private methods (not exposed to JS)
impl Player {
    /// Emit a PlayerEvent to the registered callback.
    fn emit_event(&self, event: &PlayerEvent) {
        if let Some(callback) = &self.event_callback {
            if let Ok(js_event) = serde_wasm_bindgen::to_value(event) {
                let _ = callback.call1(&JsValue::NULL, &js_event);
            }
        }
    }

    /// Process all media data: demux → decode → render (MVP batch).
    fn process_media(&mut self) -> Result<(), JsValue> {
        let data = match &self.data_buffer {
            Some(d) => d.clone(),
            None => return Err(JsValue::from_str("No media loaded")),
        };

        let format = self.demuxer_format.unwrap_or(ContainerFormat::Unknown);

        // Re-parse and iterate chunks
        match format {
            ContainerFormat::Mp4 => {
                let mut demuxer = Mp4Demuxer::new();
                demuxer
                    .parse_header(&data)
                    .map_err(|e| JsValue::from_str(&e.to_string()))?;
                self.feed_chunks(&mut demuxer)?;
            }
            ContainerFormat::Mkv | ContainerFormat::WebM => {
                let mut demuxer = MkvDemuxer::new();
                demuxer
                    .parse_header(&data)
                    .map_err(|e| JsValue::from_str(&e.to_string()))?;
                self.feed_chunks(&mut demuxer)?;
            }
            _ => {}
        }

        Ok(())
    }

    /// Feed chunks from a demuxer to the decoders.
    fn feed_chunks<D: Demuxer>(&mut self, demuxer: &mut D) -> Result<(), JsValue> {
        loop {
            match demuxer.next_chunk() {
                Ok(Some(chunk)) => {
                    if chunk.is_video {
                        self.video_decoder.decode(&chunk)?;
                    } else if self.audio_pipeline.is_configured() {
                        self.audio_pipeline.decode(&chunk)?;
                    }
                }
                Ok(None) => break, // EOF
                Err(e) => {
                    self.emit_event(&PlayerEvent::Error {
                        message: e.to_string(),
                        recoverable: true,
                    });
                    break;
                }
            }
        }

        Ok(())
    }
}
