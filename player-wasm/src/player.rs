use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use demuxer::{detect_format, ContainerFormat, Demuxer, EncodedChunk, MkvDemuxer, Mp4Demuxer};
use m3u_core::{parse as parse_m3u, Playlist};
use player_core::{MediaInfo, PlaybackStatus, PlayerEvent, PlayerState};

use crate::audio::AudioPipeline;
use crate::buffer::{BufferConfig, SharedDownload};
use crate::decoder::VideoDecoderWrapper;
use crate::fetch::{self, StreamReader};
use crate::renderer::CanvasRenderer;
use crate::sync::{AVSync, SyncAction};

/// The main Player struct — headless, framework-agnostic.
/// Receives a canvas from the consumer, never creates DOM elements.
///
/// **Streaming architecture**: download happens in the background via `spawn_local`.
/// Data flows into a `SharedDownload` buffer. The `render_tick()` method, called by
/// JS `requestAnimationFrame`, progressively demuxes, decodes, and renders frames.
#[wasm_bindgen]
pub struct Player {
    renderer: CanvasRenderer,
    video_decoder: VideoDecoderWrapper,
    audio_pipeline: AudioPipeline,
    av_sync: AVSync,
    state: PlayerState,
    event_callback: Option<js_sys::Function>,
    config: BufferConfig,

    // --- Streaming download ---
    download: Rc<RefCell<SharedDownload>>,

    // --- Demuxer state ---
    header_parsed: bool,
    demuxer_format: Option<ContainerFormat>,
    /// Queue of demuxed encoded chunks ready for decoding.
    chunk_queue: VecDeque<EncodedChunk>,
    /// MP4 demuxer resume cursors (track_id, sample_index).
    mp4_cursors: Option<Vec<(u32, u32)>>,
    /// MKV demuxer resume position (number of frames already read).
    mkv_frames_read: usize,
    /// Data length at last demux session (avoid re-demuxing same data).
    last_demux_data_len: usize,

    // --- Playback timing ---
    /// `performance.now()` at the moment play() is called.
    playback_start_time: f64,

    // --- Playlist ---
    playlist: Option<Playlist>,
    playlist_index: usize,
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
            config: BufferConfig::default(),
            download: SharedDownload::new(),
            header_parsed: false,
            demuxer_format: None,
            chunk_queue: VecDeque::new(),
            mp4_cursors: None,
            mkv_frames_read: 0,
            last_demux_data_len: 0,
            playback_start_time: 0.0,
            playlist: None,
            playlist_index: 0,
        })
    }

    /// Set buffer configuration. Must be called before `load()`.
    pub fn set_config(&mut self, config: BufferConfig) {
        self.config = config;
    }

    /// Register an event callback.
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

    /// Load a video from a URL (streaming).
    ///
    /// This method:
    /// 1. Opens a streaming HTTP connection
    /// 2. Reads data until the container header can be parsed
    /// 3. Configures decoders
    /// 4. Spawns a background task for the remaining download
    /// 5. Returns — caller should then call `play()` and start a `render_tick()` loop
    pub async fn load(&mut self, url: String) -> Result<(), JsValue> {
        self.state.status = PlaybackStatus::Loading;
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Loading,
        });

        // Reset download state
        self.download = SharedDownload::new();
        self.header_parsed = false;
        self.chunk_queue.clear();
        self.mp4_cursors = None;
        self.mkv_frames_read = 0;
        self.last_demux_data_len = 0;

        // Open streaming connection
        let stream = StreamReader::open(&url).await?;
        {
            let mut dl = self.download.borrow_mut();
            dl.content_length = stream.content_length;
            if stream.content_length > 0 {
                dl.data.reserve(stream.content_length as usize);
            }
        }

        // Read chunks until header can be parsed
        loop {
            match stream.read_chunk().await? {
                Some(chunk_data) => {
                    {
                        let mut dl = self.download.borrow_mut();
                        dl.data.extend_from_slice(&chunk_data);
                    }
                    self.emit_download_progress();

                    if self.try_parse_header()? {
                        break;
                    }
                }
                None => {
                    self.download.borrow_mut().complete = true;
                    if !self.header_parsed {
                        self.state.status = PlaybackStatus::Error;
                        return Err(JsValue::from_str(
                            "Download complete but container header could not be parsed",
                        ));
                    }
                    // Entire file downloaded during header parsing
                    return Ok(());
                }
            }
        }

        // Header parsed — do an initial demux batch so we have frames ready
        self.try_demux_more();

        // Spawn background download for the remaining data
        if !self.download.borrow().complete {
            self.spawn_background_download(stream);
        }

        Ok(())
    }

    /// Start playback. Must call `load()` first.
    /// After calling `play()`, start a `requestAnimationFrame` loop calling `render_tick()`.
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

        self.playback_start_time = now_ms();
        self.av_sync.set_start_offset(0.0);

        self.state.status = PlaybackStatus::Playing;
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Playing,
        });

        Ok(())
    }

    /// Main render loop method — call this from `requestAnimationFrame`.
    ///
    /// Returns `true` if playback should continue (call again next rAF),
    /// `false` if playback has ended or been stopped.
    pub fn render_tick(&mut self) -> bool {
        if self.state.status != PlaybackStatus::Playing
            && self.state.status != PlaybackStatus::Buffering
        {
            return false;
        }

        // 1. Demux more chunks if queue is low
        if self.chunk_queue.len() < self.config.min_chunk_queue {
            self.try_demux_more();
        }

        // 2. Feed encoded chunks to decoders (batch)
        let mut decoded = 0;
        while decoded < self.config.decode_batch_size {
            if let Some(chunk) = self.chunk_queue.pop_front() {
                if chunk.is_video {
                    if let Err(e) = self.video_decoder.decode(&chunk) {
                        self.emit_event(&PlayerEvent::Error {
                            message: format!("Video decode error: {:?}", e),
                            recoverable: true,
                        });
                    }
                } else if self.audio_pipeline.is_configured() {
                    if let Err(e) = self.audio_pipeline.decode(&chunk) {
                        self.emit_event(&PlayerEvent::Error {
                            message: format!("Audio decode error: {:?}", e),
                            recoverable: true,
                        });
                    }
                }
                decoded += 1;
            } else {
                break;
            }
        }

        // 3. Get the master clock
        let clock_ms = self.clock_ms();

        // 4. Render video frames with A/V sync
        loop {
            if let Some(pts_us) = self.video_decoder.peek_timestamp_us() {
                let pts_ms = pts_us / 1000.0;
                match self.av_sync.should_render_frame(pts_ms, clock_ms) {
                    SyncAction::Render => {
                        if let Some(frame) = self.video_decoder.take_frame() {
                            let _ = self.renderer.render_frame(&frame);
                            frame.close();
                        }
                        break; // One rendered frame per tick
                    }
                    SyncAction::Drop => {
                        if let Some(frame) = self.video_decoder.take_frame() {
                            frame.close();
                        }
                        // Continue — try next frame
                    }
                    SyncAction::Hold => {
                        break; // Too early — wait for next tick
                    }
                }
            } else {
                break; // No frames available
            }
        }

        // 5. Pump decoded audio to Web Audio output
        let _ = self.audio_pipeline.pump_audio();

        // 6. Update time
        self.state.current_time_ms = clock_ms as u64;
        self.emit_event(&PlayerEvent::TimeUpdate {
            current_ms: clock_ms as u64,
        });

        // 7. Buffer state management
        let download_complete = self.download.borrow().complete;
        let has_video_frames = self.video_decoder.queue_len() > 0;
        let has_chunks = !self.chunk_queue.is_empty();
        let can_demux_more = self.download.borrow().data.len() > self.last_demux_data_len;

        if !has_video_frames && !has_chunks && !can_demux_more {
            if download_complete {
                // All data downloaded and consumed — playback ended
                self.state.status = PlaybackStatus::Stopped;
                self.emit_event(&PlayerEvent::Ended);
                self.emit_event(&PlayerEvent::StatusChanged {
                    status: PlaybackStatus::Stopped,
                });
                return false;
            } else {
                // Waiting for more data — buffering
                if self.state.status != PlaybackStatus::Buffering {
                    self.state.status = PlaybackStatus::Buffering;
                    self.emit_event(&PlayerEvent::StatusChanged {
                        status: PlaybackStatus::Buffering,
                    });
                }
            }
        } else if self.state.status == PlaybackStatus::Buffering {
            // We have data again — resume playing
            self.state.status = PlaybackStatus::Playing;
            self.emit_event(&PlayerEvent::StatusChanged {
                status: PlaybackStatus::Playing,
            });
        }

        // 8. Back-pressure on download
        let video_queue_len = self.video_decoder.queue_len();
        {
            let mut dl = self.download.borrow_mut();
            if video_queue_len > self.config.max_video_queue {
                dl.paused = true;
            } else if video_queue_len < self.config.resume_video_queue {
                dl.paused = false;
            }
        }

        // 9. Emit buffer update
        let buffered_bytes = self.download.borrow().data.len() as u64;
        self.state.buffered_ms = buffered_bytes; // Approximate — real ms requires demux
        self.emit_event(&PlayerEvent::BufferUpdate {
            buffered_ms: buffered_bytes,
        });

        true
    }

    /// Pause playback.
    pub fn pause(&mut self) {
        if self.state.status == PlaybackStatus::Playing
            || self.state.status == PlaybackStatus::Buffering
        {
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
        self.chunk_queue.clear();
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Stopped,
        });
    }

    /// Seek to a position in milliseconds.
    pub async fn seek(&mut self, _time_ms: u64) -> Result<(), JsValue> {
        // TODO: implement seeking
        Err(JsValue::from_str("Seeking not yet implemented"))
    }

    /// Load an M3U playlist from a URL, then load the first track.
    pub async fn load_playlist(&mut self, url: String) -> Result<(), JsValue> {
        self.state.status = PlaybackStatus::Loading;
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Loading,
        });

        // Fetch playlist text (small file — read all at once)
        let stream = StreamReader::open(&url).await?;
        let mut data = Vec::new();
        loop {
            match stream.read_chunk().await? {
                Some(chunk) => data.extend_from_slice(&chunk),
                None => break,
            }
        }

        let text = String::from_utf8(data)
            .map_err(|_| JsValue::from_str("Playlist is not valid UTF-8"))?;

        let playlist =
            parse_m3u(&text).map_err(|e| JsValue::from_str(&format!("M3U parse error: {}", e)))?;

        if playlist.entries.is_empty() {
            return Err(JsValue::from_str("Playlist has no entries"));
        }

        self.playlist = Some(playlist);
        self.playlist_index = 0;

        self.load_current_track().await
    }

    /// Get the current playlist as a JS value.
    pub fn get_playlist(&self) -> JsValue {
        match &self.playlist {
            Some(pl) => serde_wasm_bindgen::to_value(pl).unwrap_or(JsValue::NULL),
            None => JsValue::NULL,
        }
    }

    /// Get the current playlist track index.
    pub fn get_playlist_index(&self) -> usize {
        self.playlist_index
    }

    /// Skip to the next track in the playlist.
    pub async fn next_track(&mut self) -> Result<(), JsValue> {
        let len = self.playlist_len();
        if len == 0 {
            return Err(JsValue::from_str("No playlist loaded"));
        }
        if self.playlist_index + 1 >= len {
            return Err(JsValue::from_str("Already at last track"));
        }
        self.playlist_index += 1;
        self.reset_for_track();
        self.load_current_track().await
    }

    /// Go back to the previous track in the playlist.
    pub async fn previous_track(&mut self) -> Result<(), JsValue> {
        if self.playlist.is_none() {
            return Err(JsValue::from_str("No playlist loaded"));
        }
        if self.playlist_index == 0 {
            return Err(JsValue::from_str("Already at first track"));
        }
        self.playlist_index -= 1;
        self.reset_for_track();
        self.load_current_track().await
    }

    /// Jump to a specific track by index.
    pub async fn play_track(&mut self, index: usize) -> Result<(), JsValue> {
        let len = self.playlist_len();
        if index >= len {
            return Err(JsValue::from_str("Track index out of bounds"));
        }
        self.playlist_index = index;
        self.reset_for_track();
        self.load_current_track().await
    }

    /// Destroy the player and release all resources.
    pub fn destroy(&mut self) {
        self.video_decoder.close();
        self.audio_pipeline.close();
        self.renderer.clear();
        self.chunk_queue.clear();
        self.download = SharedDownload::new();
        self.event_callback = None;
        self.state = PlayerState::default();
        self.playlist = None;
        self.playlist_index = 0;
        self.header_parsed = false;
        self.demuxer_format = None;
        self.mp4_cursors = None;
        self.mkv_frames_read = 0;
        self.last_demux_data_len = 0;
    }
}

// ============================================================
// Private methods (not exposed to JS)
// ============================================================
impl Player {
    /// Emit a PlayerEvent to the registered callback.
    fn emit_event(&self, event: &PlayerEvent) {
        if let Some(callback) = &self.event_callback {
            if let Ok(js_event) = serde_wasm_bindgen::to_value(event) {
                let _ = callback.call1(&JsValue::NULL, &js_event);
            }
        }
    }

    /// Emit a download progress event from current SharedDownload state.
    fn emit_download_progress(&self) {
        let (received, total) = {
            let dl = self.download.borrow();
            (dl.data.len() as u64, dl.content_length)
        };
        self.emit_event(&PlayerEvent::DownloadProgress {
            received_bytes: received,
            total_bytes: total,
        });
    }

    /// Try to parse the container header from current download data.
    /// Returns `true` if header was successfully parsed, `false` if more data is needed.
    fn try_parse_header(&mut self) -> Result<bool, JsValue> {
        if self.header_parsed {
            return Ok(true);
        }

        let data = self.download.borrow().data.clone();

        // Need at least 12 bytes to probe format
        if data.len() < 12 {
            return Ok(false);
        }

        let format = detect_format(&data);
        if format == ContainerFormat::Unknown {
            // Not enough data or truly unsupported — keep trying
            return Ok(false);
        }

        // Try to parse the header with current data
        let media_info = match format {
            ContainerFormat::Mp4 => {
                let mut demuxer = Mp4Demuxer::new();
                match demuxer.parse_header(&data) {
                    Ok(info) => info,
                    Err(_) => return Ok(false), // Not enough data yet (e.g. moov at end)
                }
            }
            ContainerFormat::Mkv | ContainerFormat::WebM => {
                let mut demuxer = MkvDemuxer::new();
                match demuxer.parse_header(&data) {
                    Ok(info) => info,
                    Err(_) => return Ok(false),
                }
            }
            _ => {
                self.state.status = PlaybackStatus::Error;
                return Err(JsValue::from_str("Unsupported container format"));
            }
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
            video_codec: media_info
                .video_tracks
                .first()
                .map(|t| t.codec_string.clone()),
            audio_codec: media_info
                .audio_tracks
                .first()
                .map(|t| t.codec_string.clone()),
            width: media_info
                .video_tracks
                .first()
                .map(|t| t.width)
                .unwrap_or(0),
            height: media_info
                .video_tracks
                .first()
                .map(|t| t.height)
                .unwrap_or(0),
            fps: media_info.video_tracks.first().and_then(|t| t.fps),
            sample_rate: media_info.audio_tracks.first().map(|t| t.sample_rate),
            channels: media_info.audio_tracks.first().map(|t| t.channels),
        };

        self.state.duration_ms = player_info.duration_ms;
        self.state.media_info = Some(player_info.clone());
        self.state.status = PlaybackStatus::Ready;

        self.header_parsed = true;
        self.demuxer_format = Some(format);

        self.emit_event(&PlayerEvent::MediaLoaded { info: player_info });
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Ready,
        });

        Ok(true)
    }

    /// Demux more encoded chunks from the download buffer into chunk_queue.
    /// Re-creates the demuxer with a snapshot of current data and resumes from
    /// the last known position.
    fn try_demux_more(&mut self) {
        let data_len = self.download.borrow().data.len();

        // Only re-demux if we have new data since last session
        if data_len <= self.last_demux_data_len {
            return;
        }

        let format = match self.demuxer_format {
            Some(f) => f,
            None => return,
        };

        // Clone current data snapshot
        let data = self.download.borrow().data.clone();
        let target = self.config.demux_batch_size;
        let mut count = 0;

        match format {
            ContainerFormat::Mp4 => {
                let mut demuxer = Mp4Demuxer::new();
                if demuxer.parse_header(&data).is_err() {
                    return;
                }
                // Resume from last position
                if let Some(ref cursors) = self.mp4_cursors {
                    demuxer.set_sample_positions(cursors.clone());
                }
                while count < target {
                    match demuxer.next_chunk() {
                        Ok(Some(chunk)) => {
                            self.chunk_queue.push_back(chunk);
                            count += 1;
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
                self.mp4_cursors = Some(demuxer.sample_positions());
            }
            ContainerFormat::Mkv | ContainerFormat::WebM => {
                let mut demuxer = MkvDemuxer::new();
                if demuxer.parse_header(&data).is_err() {
                    return;
                }
                // Skip to resume position
                if self.mkv_frames_read > 0 {
                    if demuxer.skip_frames(self.mkv_frames_read).is_err() {
                        return;
                    }
                }
                while count < target {
                    match demuxer.next_chunk() {
                        Ok(Some(chunk)) => {
                            self.chunk_queue.push_back(chunk);
                            count += 1;
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
                self.mkv_frames_read = demuxer.frames_read();
            }
            _ => {}
        }

        self.last_demux_data_len = data_len;
    }

    /// Get the current master clock in milliseconds.
    /// Uses AudioContext.currentTime when audio is available,
    /// falls back to performance.now() offset.
    fn clock_ms(&self) -> f64 {
        if self.audio_pipeline.is_configured() {
            self.audio_pipeline.current_time_ms()
        } else {
            now_ms() - self.playback_start_time
        }
    }

    /// Spawn a background task to continue downloading remaining data.
    fn spawn_background_download(&self, stream: StreamReader) {
        let download = self.download.clone();
        let event_cb = self.event_callback.clone();
        let max_rate = self.config.max_download_rate;

        wasm_bindgen_futures::spawn_local(async move {
            let mut bytes_this_window: u64 = 0;
            let mut window_start = js_sys::Date::now();

            loop {
                // Check pause (back-pressure from decoder queue)
                {
                    let dl = download.borrow();
                    if dl.paused {
                        drop(dl);
                        fetch::sleep_ms(50).await;
                        continue;
                    }
                }

                match stream.read_chunk().await {
                    Ok(Some(chunk)) => {
                        let chunk_len = chunk.len() as u64;
                        {
                            let mut dl = download.borrow_mut();
                            dl.data.extend_from_slice(&chunk);
                        }

                        // Rate limiting
                        if max_rate > 0 {
                            bytes_this_window += chunk_len;
                            let now = js_sys::Date::now();
                            let elapsed_ms = now - window_start;

                            if elapsed_ms > 1000.0 {
                                // Reset window
                                bytes_this_window = chunk_len;
                                window_start = now;
                            } else {
                                let allowed =
                                    (max_rate as f64 * elapsed_ms / 1000.0) as u64;
                                if bytes_this_window > allowed {
                                    let sleep = ((bytes_this_window as f64
                                        / max_rate as f64)
                                        * 1000.0
                                        - elapsed_ms)
                                        as i32;
                                    if sleep > 0 {
                                        fetch::sleep_ms(sleep).await;
                                    }
                                }
                            }
                        }

                        // Emit progress
                        if let Some(ref cb) = event_cb {
                            let (received, total) = {
                                let dl = download.borrow();
                                (dl.data.len() as u64, dl.content_length)
                            };
                            let event = PlayerEvent::DownloadProgress {
                                received_bytes: received,
                                total_bytes: total,
                            };
                            if let Ok(js_event) = serde_wasm_bindgen::to_value(&event) {
                                let _ = cb.call1(&JsValue::NULL, &js_event);
                            }
                        }
                    }
                    Ok(None) => {
                        download.borrow_mut().complete = true;
                        break;
                    }
                    Err(e) => {
                        let msg = e
                            .as_string()
                            .unwrap_or_else(|| format!("{:?}", e));
                        download.borrow_mut().error = Some(msg);
                        break;
                    }
                }
            }
        });
    }

    /// Get playlist length (0 if no playlist).
    fn playlist_len(&self) -> usize {
        self.playlist
            .as_ref()
            .map(|p| p.entries.len())
            .unwrap_or(0)
    }

    /// Reset decoder/audio state before loading a new track.
    fn reset_for_track(&mut self) {
        self.video_decoder.close();
        self.audio_pipeline.close();
        self.renderer.clear();
        self.download = SharedDownload::new();
        self.header_parsed = false;
        self.demuxer_format = None;
        self.chunk_queue.clear();
        self.mp4_cursors = None;
        self.mkv_frames_read = 0;
        self.last_demux_data_len = 0;
        self.state.current_time_ms = 0;
        self.state.has_video = false;
        self.state.has_audio = false;
        self.state.media_info = None;
        self.state.duration_ms = None;
        self.video_decoder = VideoDecoderWrapper::new();
        self.audio_pipeline = AudioPipeline::new();
        self.av_sync = AVSync::new();
    }

    /// Load the track at the current playlist_index.
    async fn load_current_track(&mut self) -> Result<(), JsValue> {
        let url = {
            let playlist = self
                .playlist
                .as_ref()
                .ok_or_else(|| JsValue::from_str("No playlist loaded"))?;
            let entry = playlist
                .entries
                .get(self.playlist_index)
                .ok_or_else(|| JsValue::from_str("Track index out of bounds"))?;
            entry.url.clone()
        };

        self.emit_event(&PlayerEvent::PlaylistTrackChanged {
            index: self.playlist_index,
        });

        self.load(url).await
    }
}

/// Get the current time in milliseconds from `performance.now()`.
fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}
