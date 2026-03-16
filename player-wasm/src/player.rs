use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;
use web_sys::console;

use demuxer::{detect_format, find_cluster_offset, ContainerFormat, Demuxer, EncodedChunk, MkvDemuxer, MoovLocation, Mp4Demuxer, SeekIndex};
use m3u_core::{parse as parse_m3u, Playlist};
use player_core::{MediaInfo, PlaybackStatus, PlayerEvent, PlayerState};

use crate::audio::AudioPipeline;
use crate::buffer::{BufferConfig, PrefetchState, RangeBuffer, SharedDownload};
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
///
/// **Non-faststart MP4**: When the moov box is at the end of the file (common for
/// non-faststart MP4), a Range request fetches it early. A synthetic buffer is built
/// for demuxing: `[original bytes 0..N] + [modified mdat header] + [moov]`. Sample
/// offsets (stco/co64) in moov point to absolute file positions, which match our
/// download buffer since it starts at byte 0 of the original file.
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
    /// Cached MKV demuxer — avoids expensive re-parse + frame-skip on every try_demux_more.
    mkv_demuxer: Option<MkvDemuxer>,
    /// Saved MKV header bytes (EBML + Segment Info + Tracks, up to first Cluster).
    /// Used to build synthetic buffers for Range-based seeking.
    mkv_header_bytes: Option<Vec<u8>>,
    /// Data length when the cached MKV demuxer was created.
    /// Used to build incremental synthetic buffers (header + new data only) instead of
    /// expensive O(n) skip_frames when recreating the demuxer.
    mkv_cache_created_at: usize,
    /// Data length at last demux session (avoid re-demuxing same data).
    last_demux_data_len: usize,

    /// URL of the current media (kept for Range requests during seek).
    current_url: Option<String>,
    /// Whether the server supports HTTP Range requests.
    server_supports_range: bool,

    // --- Non-faststart MP4 support ---
    /// moov box data fetched via Range request (for moov-at-end MP4).
    /// When Some, `build_demux_buffer()` builds a synthetic buffer for the mp4 crate.
    moov_data: Option<Vec<u8>>,
    /// Byte offset of the mdat box in the original file.
    mdat_offset: usize,
    /// Size of the mdat box header (8 or 16 bytes).
    mdat_header_size: usize,

    // --- Range-first seeking ---
    /// Seek index mapping keyframe timestamps to byte offsets.
    /// Built during load() from container metadata (stss+stco for MP4, Clusters for MKV).
    seek_index: SeekIndex,
    /// Non-contiguous buffer for Range-based streaming.
    /// Replaces linear `SharedDownload.data` for on-demand window fetching.
    /// `None` when using linear fallback mode; `Some` when Range-first load succeeded.
    range_buffer: Option<RangeBuffer>,
    /// Shared state for background prefetch tasks (spawn_local writes here, render_tick drains).
    prefetch: Rc<RefCell<PrefetchState>>,

    // --- Event throttling (performance: reduce WASM→JS boundary crossings) ---
    last_time_update_ms: f64,
    last_download_progress_ms: f64,
    last_buffer_update_ms: f64,

    // --- Buffering timeout ---
    /// Timestamp (performance.now()) when we entered Buffering state.
    /// Used to emit a recoverable error if buffering lasts >10s.
    buffering_since_ms: Option<f64>,

    // --- Sync stats emission ---
    sync_stats_frame_counter: u64,

    // --- Seek guard ---
    /// Incremented on each seek() call. If a seek finds the generation has changed
    /// (another seek was requested), it aborts early to avoid conflicting state updates.
    seek_generation: u32,

    // --- Playback timing ---
    /// `performance.now()` at the moment play() is called, adjusted for seek.
    /// Clock = now_ms() - playback_start_time. After seek to T ms,
    /// playback_start_time = now_ms() - T so clock starts at T.
    playback_start_time: f64,
    /// Set once the clock has been re-anchored to the first decoded frame's PTS.
    /// Prevents the initial WebCodecs latency from causing mass frame drops.
    clock_synced_to_first_frame: bool,
    /// After SkipToKeyframe, ignore decoded frames with PTS below this threshold.
    /// The WebCodecs decoder pipeline may still contain frames from before the skip.
    /// Once a frame >= this PTS arrives, the filter is cleared.
    skip_frames_before_us: Option<f64>,
    /// Status before seek started (to restore after).
    pre_seek_status: Option<PlaybackStatus>,

    // --- Decoder config (stored for reconfiguration on seek/error recovery) ---
    demuxer_media_info: Option<demuxer::MediaInfo>,

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
            mkv_demuxer: None,
            mkv_header_bytes: None,
            mkv_cache_created_at: 0,
            last_demux_data_len: 0,
            current_url: None,
            server_supports_range: false,
            last_time_update_ms: 0.0,
            last_download_progress_ms: 0.0,
            last_buffer_update_ms: 0.0,
            buffering_since_ms: None,
            sync_stats_frame_counter: 0,
            seek_generation: 0,
            moov_data: None,
            mdat_offset: 0,
            mdat_header_size: 0,
            seek_index: SeekIndex::new(),
            range_buffer: None,
            prefetch: PrefetchState::new(),
            playback_start_time: 0.0,
            clock_synced_to_first_frame: false,
            skip_frames_before_us: None,
            pre_seek_status: None,
            demuxer_media_info: None,
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

    /// Load a video from a URL (Range-first streaming).
    ///
    /// Strategy:
    /// 1. HEAD request → file size + Range support
    /// 2. If Range supported → fetch only metadata (moov for MP4, header for MKV)
    /// 3. Parse header + build SeekIndex for precise byte-level seeking
    /// 4. Configure decoders
    /// 5. Fetch initial data window for playback start
    /// 6. If no Range support → fallback to linear streaming
    pub async fn load(&mut self, url: String) -> Result<(), JsValue> {
        self.state.status = PlaybackStatus::Loading;
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Loading,
        });

        // Reset state
        self.download = SharedDownload::new();
        self.header_parsed = false;
        self.chunk_queue.clear();
        self.mp4_cursors = None;
        self.mkv_frames_read = 0;
        self.mkv_demuxer = None;
        self.last_demux_data_len = 0;
        self.moov_data = None;
        self.mdat_offset = 0;
        self.mdat_header_size = 0;
        self.seek_index = SeekIndex::new();
        self.range_buffer = None;
        self.prefetch.borrow_mut().cancelled = true;
        self.prefetch = PrefetchState::new();
        self.current_url = Some(url.clone());

        // Step 1: Combined probe + format detection.
        // Instead of HEAD (which doesn't reliably report Accept-Ranges), we send a
        // single GET with Range: bytes=0-65535. This serves three purposes at once:
        //   a) Detect Range support: 206 → yes, 200 → no
        //   b) Get total file size from Content-Range header
        //   c) Get the first 64KB of data for format detection (MP4 ftyp/moov, MKV EBML)
        let probe_result = match StreamReader::fetch_range_ext(&url, 0, 65535).await {
            Ok(r) => r,
            Err(e) => {
                console::log_1(&format!(
                    "[player] probe failed ({:?}), linear fallback", e
                ).into());
                return self.load_linear(url).await;
            }
        };

        let supports_range = probe_result.is_partial;
        self.server_supports_range = supports_range;

        // Determine file size: prefer Content-Range total, then HEAD fallback.
        // When Content-Range is not exposed via CORS, total_size falls back to
        // Content-Length which for a 206 response equals the partial data size —
        // NOT the total file size. Detect this by checking if total_size ≤ data length.
        let probe_data_len = probe_result.data.len() as u64;
        let mut file_size = probe_result.total_size;
        let content_range_missing = supports_range
            && (file_size == 0 || file_size <= probe_data_len);

        if content_range_missing {
            console::log_1(&format!(
                "[player] 206 but Content-Range not accessible (total_size={}, data_len={}) — HEAD fallback",
                file_size, probe_data_len
            ).into());
            match StreamReader::head(&url).await {
                Ok(head) if head.content_length > 0 => {
                    file_size = head.content_length;
                    console::log_1(&format!(
                        "[player] HEAD: file_size={}", file_size
                    ).into());
                }
                _ => {
                    console::log_1(&"[player] HEAD failed too — cannot determine file size, linear fallback".into());
                    {
                        let mut dl = self.download.borrow_mut();
                        dl.data = probe_result.data;
                    }
                    return self.load_linear_continue(&url).await;
                }
            }
        }

        {
            let mut dl = self.download.borrow_mut();
            dl.content_length = file_size;
        }

        console::log_1(&format!(
            "[player] probe: {} bytes, status={}, file_size={}, range={}",
            probe_result.data.len(),
            if supports_range { "206" } else { "200" },
            file_size,
            supports_range
        ).into());

        // Step 2: Range-first or linear fallback
        if supports_range && file_size > 0 {
            console::log_1(&format!(
                "[player] ✓ Range supported — streaming (file={}MB)",
                file_size / (1024 * 1024)
            ).into());
            // Pass the already-fetched probe data to avoid re-fetching
            self.load_range_first_with_probe(&url, file_size, probe_result.data).await
        } else {
            console::log_1(&format!(
                "[player] ✗ No Range (range={}, size={}) — linear fallback",
                supports_range, file_size
            ).into());
            // Store probe data so linear path doesn't re-download it
            {
                let mut dl = self.download.borrow_mut();
                dl.data = probe_result.data;
                if !supports_range && file_size == 0 {
                    // 200 response: total_size from Content-Length = actual file size
                    dl.content_length = probe_result.total_size;
                }
            }
            self.load_linear_continue(&url).await
        }
    }

    /// Range-first load with already-fetched probe data (from the initial Range probe).
    /// `probe_data` is the first ~64KB fetched during the probe in load().
    async fn load_range_first_with_probe(
        &mut self,
        url: &str,
        file_size: u64,
        probe_data: Vec<u8>,
    ) -> Result<(), JsValue> {
        console::log_1(&format!(
            "[player] Range-first load: probe={} bytes, file={}",
            probe_data.len(), file_size
        ).into());

        if probe_data.len() < 12 {
            console::log_1(&format!(
                "[player] probe too small ({} bytes), linear fallback",
                probe_data.len()
            ).into());
            {
                let mut dl = self.download.borrow_mut();
                dl.data = probe_data;
            }
            return self.load_linear_continue(url).await;
        }

        let format = detect_format(&probe_data);
        match format {
            ContainerFormat::Mp4 => {
                self.load_mp4_range(url, file_size, &probe_data).await
            }
            ContainerFormat::Mkv | ContainerFormat::WebM => {
                self.load_mkv_range(url, file_size, &probe_data).await
            }
            _ => {
                console::log_1(&"[player] unknown format from probe, falling back to linear".into());
                // Store probe data in download buffer so linear path has it
                {
                    let mut dl = self.download.borrow_mut();
                    dl.data.extend_from_slice(&probe_data);
                }
                self.load_linear_continue(url).await
            }
        }
    }

    /// MP4 Range-first load: fetch moov box via targeted Range requests.
    async fn load_mp4_range(
        &mut self,
        url: &str,
        file_size: u64,
        probe_data: &[u8],
    ) -> Result<(), JsValue> {
        // Scan top-level boxes in probe data
        match Mp4Demuxer::locate_moov(probe_data, file_size) {
            MoovLocation::Found { offset, size } => {
                console::log_1(&format!(
                    "[player] MP4 moov found at offset={}, size={}", offset, size
                ).into());

                // moov is in the probe data or we need to fetch more
                let moov_end = offset + size;
                let header_data = if moov_end <= probe_data.len() as u64 {
                    // moov entirely in probe data
                    probe_data.to_vec()
                } else {
                    // Need to fetch the full moov
                    console::log_1(&format!(
                        "[player] fetching full moov: bytes 0-{}", moov_end - 1
                    ).into());
                    StreamReader::fetch_range(url, 0, moov_end - 1).await?
                };

                // Parse header
                let mut demuxer = Mp4Demuxer::new();
                let media_info = demuxer
                    .parse_header(&header_data)
                    .map_err(|e| JsValue::from_str(&format!("MP4 parse error: {:?}", e)))?;

                // Build seek index
                self.seek_index = demuxer.build_seek_index();
                console::log_1(&format!(
                    "[player] SeekIndex built: {} entries", self.seek_index.len()
                ).into());

                // Detect mdat position for build_demux_buffer mdat-patching
                let boxes = Mp4Demuxer::scan_top_level_boxes(&header_data);
                if let Some(mdat) = boxes.iter().find(|b| b.is_type(b"mdat")) {
                    self.mdat_offset = mdat.offset as usize;
                    let i = mdat.offset as usize;
                    if i + 4 <= header_data.len() {
                        let size_u32 = u32::from_be_bytes([
                            header_data[i], header_data[i + 1],
                            header_data[i + 2], header_data[i + 3],
                        ]);
                        self.mdat_header_size = if size_u32 == 1 { 16 } else { 8 };
                    } else {
                        self.mdat_header_size = 8;
                    }
                    console::log_1(&format!(
                        "[player] faststart mdat at offset={}, header_size={}",
                        self.mdat_offset, self.mdat_header_size
                    ).into());
                }

                // Store header data in download buffer for demuxing
                {
                    let mut dl = self.download.borrow_mut();
                    dl.data = header_data;
                }

                // Initialize RangeBuffer for on-demand window fetching
                self.range_buffer = Some(RangeBuffer::new(file_size));

                self.configure_decoders(&media_info)?;
                self.header_parsed = true;
                self.demuxer_format = Some(ContainerFormat::Mp4);

                // Fetch initial window + spawn streaming download
                self.fetch_initial_and_stream(url, file_size).await?;

                console::log_1(&"[player] MP4 Range-first load complete".into());
                Ok(())
            }
            MoovLocation::AtEnd { moov_offset } => {
                console::log_1(&format!(
                    "[player] MP4 moov-at-end, offset={}", moov_offset
                ).into());

                // Store probe data as download buffer (preserves byte offsets)
                {
                    let mut dl = self.download.borrow_mut();
                    dl.data = probe_data.to_vec();
                }

                // Initialize RangeBuffer
                self.range_buffer = Some(RangeBuffer::new(file_size));

                // Fetch moov from end of file
                if self.try_fetch_moov_at_end(url, moov_offset, file_size, probe_data).await? {
                    // Build seek index from the parsed moov
                    // Re-parse to get the demuxer for build_seek_index
                    let synthetic = self.build_demux_buffer();
                    let mut demuxer = Mp4Demuxer::new();
                    if demuxer.parse_header(&synthetic).is_ok() {
                        self.seek_index = demuxer.build_seek_index();
                        console::log_1(&format!(
                            "[player] SeekIndex built (moov-at-end): {} entries",
                            self.seek_index.len()
                        ).into());
                    }

                    // Fetch initial data window for playback start
                    self.fetch_initial_and_stream(url, file_size).await?;

                    console::log_1(&"[player] MP4 moov-at-end Range-first load complete".into());
                    Ok(())
                } else {
                    console::log_1(&"[player] moov-at-end fetch failed, linear fallback".into());
                    self.load_linear_continue(url).await
                }
            }
            MoovLocation::Unknown => {
                console::log_1(&"[player] moov location unknown, fetching tail".into());
                // Try fetching from end of file (moov might be there)
                let tail_size: u64 = (65536 as u64).min(file_size);
                let tail_start = file_size - tail_size;
                let tail_data = StreamReader::fetch_range(url, tail_start, file_size - 1).await?;

                // Scan tail for moov
                let tail_boxes = Mp4Demuxer::scan_top_level_boxes(&tail_data);
                let has_moov = tail_boxes.iter().any(|b| b.is_type(b"moov"));

                if has_moov {
                    console::log_1(&"[player] moov found in file tail".into());
                    {
                        let mut dl = self.download.borrow_mut();
                        dl.data = probe_data.to_vec();
                    }
                    self.range_buffer = Some(RangeBuffer::new(file_size));

                    if self.try_fetch_moov_at_end(url, tail_start, file_size, probe_data).await? {
                        let synthetic = self.build_demux_buffer();
                        let mut demuxer = Mp4Demuxer::new();
                        if demuxer.parse_header(&synthetic).is_ok() {
                            self.seek_index = demuxer.build_seek_index();
                            console::log_1(&format!(
                                "[player] SeekIndex built (tail): {} entries",
                                self.seek_index.len()
                            ).into());
                        }
                        self.fetch_initial_and_stream(url, file_size).await?;
                        console::log_1(&"[player] MP4 tail-moov Range-first load complete".into());
                        return Ok(());
                    }
                }

                // Last resort: linear fallback
                {
                    let mut dl = self.download.borrow_mut();
                    if dl.data.is_empty() {
                        dl.data = probe_data.to_vec();
                    }
                }
                self.load_linear_continue(url).await
            }
        }
    }

    /// MKV Range-first load: fetch header + track info via Range request.
    async fn load_mkv_range(
        &mut self,
        url: &str,
        file_size: u64,
        probe_data: &[u8],
    ) -> Result<(), JsValue> {
        // MKV header parsing needs enough data to cover EBML + Segment children
        // (SeekHead, Info, Tracks, possibly Cues/Tags) up to the first Cluster.
        // Some files have large Cues or attachments before Clusters, so we use
        // exponential backoff: 256KB → 1MB → 4MB → 16MB → linear fallback.
        const INITIAL_SIZE: u64 = 256 * 1024;
        const MAX_RANGE_SIZE: u64 = 16 * 1024 * 1024; // 16MB cap before linear fallback

        let mut fetch_size = INITIAL_SIZE;
        let mut demuxer;
        let media_info;
        let mut header_data: Vec<u8>;

        loop {
            let size = fetch_size.min(file_size);
            header_data = if probe_data.len() as u64 >= size {
                probe_data[..size as usize].to_vec()
            } else {
                console::log_1(&format!(
                    "[player] MKV: fetching header range 0-{} ({}KB)",
                    size - 1, size / 1024
                ).into());
                StreamReader::fetch_range(url, 0, size - 1).await?
            };

            demuxer = MkvDemuxer::new();
            match demuxer.parse_header_streaming(&header_data) {
                Ok(info) => {
                    console::log_1(&format!(
                        "[player] MKV header parsed OK with {}KB", header_data.len() / 1024
                    ).into());
                    media_info = info;
                    break;
                }
                Err(e) => {
                    console::log_1(&format!(
                        "[player] MKV parse failed with {}KB: {:?}",
                        header_data.len() / 1024, e
                    ).into());

                    fetch_size *= 4; // exponential backoff: ×4 each time
                    if fetch_size > MAX_RANGE_SIZE || fetch_size > file_size {
                        console::log_1(&format!(
                            "[player] MKV header too large (tried up to {}KB), linear fallback",
                            size / 1024
                        ).into());
                        {
                            let mut dl = self.download.borrow_mut();
                            dl.data = header_data;
                        }
                        return self.load_linear_continue(url).await;
                    }
                }
            }
        }

        // Store in download buffer
        {
            let mut dl = self.download.borrow_mut();
            dl.data = header_data.clone();
        }

        // Save MKV header bytes (everything before first Cluster)
        let dl_data = self.download.borrow().data.clone();
        if let Some(cluster_pos) = find_cluster_offset(&dl_data) {
            self.mkv_header_bytes = Some(dl_data[..cluster_pos].to_vec());
            console::log_1(&format!(
                "[player] MKV header saved: {} bytes (first Cluster at {})",
                cluster_pos, cluster_pos
            ).into());
        }

        // Build seek index (from Cluster scanning done during parse_header)
        self.seek_index = demuxer.build_seek_index();
        console::log_1(&format!(
            "[player] MKV SeekIndex: {} entries", self.seek_index.len()
        ).into());

        // Initialize RangeBuffer for on-demand window fetching
        self.range_buffer = Some(RangeBuffer::new(file_size));

        self.configure_decoders(&media_info)?;
        self.header_parsed = true;
        self.demuxer_format = Some(ContainerFormat::Mkv);

        // Initial demux from the data we already have
        self.try_demux_more();

        // Fetch a small initial window (2MB) for quick start, then spawn
        // a continuous streaming download for the rest of the file.
        // This avoids the "lock" effect where a single big Range request
        // blocks all data flow until it completes.
        self.fetch_initial_and_stream(url, file_size).await?;

        console::log_1(&"[player] MKV Range-first load complete".into());
        Ok(())
    }

    /// Spawn a continuous streaming download from `start_byte` to EOF using
    /// `StreamReader::open_range`. Data arrives chunk-by-chunk (~64KB) instead of
    /// waiting for a giant Range response to complete. This keeps the player fed
    /// with data continuously, avoiding stalls between prefetch batches.
    fn spawn_streaming_download(&self, url: &str, start_byte: u64, file_size: u64) {
        if start_byte >= file_size {
            self.download.borrow_mut().complete = true;
            return;
        }

        let download = self.download.clone();
        let event_cb = self.event_callback.clone();
        let max_rate = self.config.max_download_rate;
        let url = url.to_string();

        console::log_1(&format!(
            "[streaming] starting continuous download from byte {} ({} MB remaining)",
            start_byte, (file_size - start_byte) / (1024 * 1024)
        ).into());

        wasm_bindgen_futures::spawn_local(async move {
            let stream = match StreamReader::open_range(&url, start_byte).await {
                Ok(s) => s,
                Err(e) => {
                    console::log_1(&format!("[streaming] open_range failed: {:?}", e).into());
                    download.borrow_mut().error = Some(format!("{:?}", e));
                    return;
                }
            };

            let mut bytes_this_window: u64 = 0;
            let mut window_start = js_sys::Date::now();

            loop {
                // Check cancellation or pause
                {
                    let dl = download.borrow();
                    if dl.cancelled {
                        break;
                    }
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
                        console::log_1(&"[streaming] download complete (EOF)".into());
                        download.borrow_mut().complete = true;
                        break;
                    }
                    Err(e) => {
                        let msg = e
                            .as_string()
                            .unwrap_or_else(|| format!("{:?}", e));
                        console::log_1(&format!("[streaming] error: {}", msg).into());
                        download.borrow_mut().error = Some(msg);
                        break;
                    }
                }
            }
        });
    }

    /// Fetch a small initial data window for playback start, then spawn a
    /// continuous streaming download for the rest of the file.
    async fn fetch_initial_and_stream(
        &mut self,
        url: &str,
        file_size: u64,
    ) -> Result<(), JsValue> {
        let current_len = self.download.borrow().data.len() as u64;
        // Fetch small initial window (2MB) for quick start, streaming handles the rest.
        let initial_window = 2 * 1024 * 1024_u64;
        let window_end = (current_len + initial_window).min(file_size);

        if current_len < window_end {
            console::log_1(&format!(
                "[player] fetching initial window: bytes {}-{} ({} KB)",
                current_len, window_end - 1, (window_end - current_len) / 1024
            ).into());
            let window_data = StreamReader::fetch_range(url, current_len, window_end - 1).await?;
            // Also insert into RangeBuffer if active
            if let Some(rb) = &mut self.range_buffer {
                rb.insert(current_len, window_data.clone());
            }
            {
                let mut dl = self.download.borrow_mut();
                dl.data.extend_from_slice(&window_data);
            }
        }

        // Demux initial chunks from the window
        self.try_demux_more();

        // Spawn continuous streaming download from where we left off
        let stream_start = self.download.borrow().data.len() as u64;
        self.spawn_streaming_download(url, stream_start, file_size);

        Ok(())
    }

    /// Fallback: linear streaming load (original behavior).
    /// Used when the server doesn't support Range requests.
    async fn load_linear(&mut self, url: String) -> Result<(), JsValue> {
        let stream = StreamReader::open(&url).await?;
        self.server_supports_range = stream.supports_range;
        let file_size = stream.content_length;
        {
            let mut dl = self.download.borrow_mut();
            dl.content_length = file_size;
            if file_size > 0 {
                let reserve = (file_size as usize).min(256 * 1024 * 1024);
                dl.data.reserve(reserve);
            }
        }

        console::log_1(&format!(
            "[player] linear load: file_size={}", file_size
        ).into());

        self.load_linear_stream(stream).await
    }

    /// Continue linear streaming with data already in download buffer.
    async fn load_linear_continue(&mut self, url: &str) -> Result<(), JsValue> {
        // Try parsing with what we have first
        if self.try_parse_header()? {
            console::log_1(&"[player] header parsed from existing probe data".into());
            self.try_demux_more();
            // Spawn background download for remaining data
            let current_len = self.download.borrow().data.len() as u64;
            if self.server_supports_range && current_len > 0 {
                let stream = StreamReader::open_range(url, current_len).await?;
                if !self.download.borrow().complete {
                    self.spawn_background_download(stream);
                }
            } else {
                let stream = StreamReader::open(url).await?;
                if !self.download.borrow().complete {
                    self.spawn_background_download(stream);
                }
            }
            return Ok(());
        }

        // Need more data — open streaming connection
        let current_len = self.download.borrow().data.len() as u64;
        let stream = if current_len > 0 && self.server_supports_range {
            StreamReader::open_range(url, current_len).await?
        } else {
            self.download.borrow_mut().data.clear();
            StreamReader::open(url).await?
        };

        self.load_linear_stream(stream).await
    }

    /// Core linear streaming loop: reads chunks until header is parsed,
    /// then spawns background download.
    async fn load_linear_stream(&mut self, stream: StreamReader) -> Result<(), JsValue> {
        loop {
            match stream.read_chunk().await? {
                Some(chunk_data) => {
                    {
                        let mut dl = self.download.borrow_mut();
                        dl.data.extend_from_slice(&chunk_data);
                    }
                    self.emit_download_progress();

                    if self.try_parse_header()? {
                        console::log_1(&"[player] header parsed (linear path)".into());
                        break;
                    }
                }
                None => {
                    self.download.borrow_mut().complete = true;
                    if !self.header_parsed {
                        console::log_1(&"[player] download complete, last parse attempt".into());
                        if !self.try_parse_header()? {
                            self.state.status = PlaybackStatus::Error;
                            return Err(JsValue::from_str(
                                "Download complete but container header could not be parsed",
                            ));
                        }
                    }
                    break;
                }
            }
        }

        // Header parsed — do an initial demux batch
        self.try_demux_more();

        // Spawn background download for remaining data
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

        if self.state.status == PlaybackStatus::Paused {
            // Resume from pause: adjust clock so it continues from current_time_ms
            self.playback_start_time = now_ms() - self.state.current_time_ms as f64;
            // Reset audio schedule so new buffers are scheduled from AudioContext's
            // current time (which was frozen during suspend)
            if self.audio_pipeline.is_configured() {
                self.audio_pipeline.reset_schedule();
            }
        } else {
            // Fresh start from Ready state
            self.playback_start_time = now_ms();
            self.av_sync.set_start_offset(0.0);
            self.clock_synced_to_first_frame = false;
            self.skip_frames_before_us = None;
        }

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
            && self.state.status != PlaybackStatus::Seeking
        {
            return false;
        }

        // Don't render during seek — just keep the loop alive
        if self.state.status == PlaybackStatus::Seeking {
            return true;
        }

        // 0. Drain any pending prefetch data into buffers
        if self.range_buffer.is_some() {
            let drained = self.drain_prefetch();
            if drained > 0 {
                // New data available — force a demux attempt
                self.try_demux_more();
            }
        }

        // 1. Demux more chunks if queue is low
        if self.chunk_queue.len() < self.config.min_chunk_queue {
            self.try_demux_more();
        }

        // 2. Feed encoded chunks to decoders (batch)
        //    Once a decoder enters an error state, stop feeding it to avoid
        //    flooding the UI with the same error on every frame.
        let mut decoded = 0;
        let video_dead = self.video_decoder.has_error().is_some();
        let audio_dead = self.audio_pipeline.has_error().is_some();
        while decoded < self.config.decode_batch_size {
            if let Some(chunk) = self.chunk_queue.pop_front() {
                if chunk.is_video {
                    if video_dead {
                        // Skip — decoder is dead
                    } else if let Err(e) = self.video_decoder.decode(&chunk) {
                        console::log_1(
                            &format!(
                                "[decode] Video FAIL: ts={}us, kf={}, data_len={}, err={:?}",
                                chunk.timestamp_us, chunk.is_keyframe, chunk.data.len(), e
                            )
                            .into(),
                        );
                        self.emit_event(&PlayerEvent::Error {
                            message: format!("Video decode error: {:?}", e),
                            recoverable: true,
                        });
                    }
                } else if chunk.is_audio && self.audio_pipeline.is_configured() {
                    if audio_dead {
                        // Skip — decoder is dead
                    } else if let Err(e) = self.audio_pipeline.decode(&chunk) {
                        console::log_1(
                            &format!(
                                "[decode] Audio FAIL: ts={}us, kf={}, data_len={}, err={:?}",
                                chunk.timestamp_us, chunk.is_keyframe, chunk.data.len(), e
                            )
                            .into(),
                        );
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
        let mut clock_ms = self.clock_ms();

        // 3a. Discard stale frames from the decoder pipeline after SkipToKeyframe.
        // WebCodecs decoding is async, so old frames may still emerge after we
        // flushed the queue and skipped to a new keyframe.
        if let Some(min_pts_us) = self.skip_frames_before_us {
            loop {
                if let Some(pts_us) = self.video_decoder.peek_timestamp_us() {
                    if pts_us < min_pts_us {
                        if let Some(frame) = self.video_decoder.take_frame() {
                            frame.close();
                        }
                        continue;
                    }
                }
                break;
            }
            // If the next frame is >= threshold, clear the filter
            if let Some(pts_us) = self.video_decoder.peek_timestamp_us() {
                if pts_us >= min_pts_us {
                    self.skip_frames_before_us = None;
                }
            }
        }

        // 3b. First-frame clock sync: WebCodecs decoding is async, so the first
        // decoded frame may arrive 50-200ms after play() starts the clock.
        // Without adjustment, all initial frames would be "late" → dropped.
        // Fix: on the very first decoded frame, re-anchor the clock to its PTS.
        if !self.clock_synced_to_first_frame {
            if let Some(pts_us) = self.video_decoder.peek_timestamp_us() {
                let pts_ms = pts_us / 1000.0;
                // Re-anchor: at this instant, the clock should equal pts_ms
                self.playback_start_time = now_ms() - pts_ms;
                clock_ms = pts_ms;
                self.clock_synced_to_first_frame = true;
                console::log_1(&format!(
                    "[sync] clock synced to first frame PTS={}ms (decoder latency={}ms)",
                    pts_ms, now_ms() - self.playback_start_time - pts_ms
                ).into());
            }
        }

        // 4. Render video frames with A/V sync
        loop {
            if let Some(pts_us) = self.video_decoder.peek_timestamp_us() {
                let pts_ms = pts_us / 1000.0;
                let action = self.av_sync.should_render_frame(pts_ms, clock_ms);

                // Diagnostic logging for first ~30 sync decisions
                let (r, d, h, _s) = self.av_sync.stats();
                let total = r + d + h;
                if total <= 30 {
                    console::log_1(&format!(
                        "[sync-dbg] #{}: pts={:.1}ms, clock={:.1}ms, diff={:.1}ms → {:?} (q={})",
                        total, pts_ms, clock_ms, pts_ms - clock_ms, action,
                        self.video_decoder.queue_len()
                    ).into());
                }

                match action {
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
                    SyncAction::SkipToKeyframe => {
                        // Too many consecutive drops — the clock is way ahead of
                        // frame PTS. Flush decoded frames AND re-anchor the clock
                        // to the next decoded frame to break the drop cascade.
                        console::log_1(&"[sync] SkipToKeyframe — flushing decoded frames + re-anchoring clock".into());
                        self.video_decoder.flush_queue();

                        // Also skip undecoded video chunks in the queue to the next
                        // keyframe. Audio chunks are kept (they don't depend on
                        // video keyframe boundaries).
                        let mut skipped_chunks = 0;
                        let mut next_kf_pts_us: Option<f64> = None;
                        let mut kept_audio = VecDeque::new();
                        while let Some(chunk) = self.chunk_queue.pop_front() {
                            if chunk.is_audio {
                                // Keep audio chunks (skip subtitles and other non-video tracks)
                                kept_audio.push_back(chunk);
                            } else if chunk.is_keyframe {
                                // Found next video keyframe — record its PTS and put it back
                                next_kf_pts_us = Some(chunk.timestamp_us as f64);
                                self.chunk_queue.push_front(chunk);
                                break;
                            } else {
                                skipped_chunks += 1;
                            }
                        }
                        // Re-insert audio chunks at the front
                        while let Some(audio) = kept_audio.pop_back() {
                            self.chunk_queue.push_front(audio);
                        }
                        if skipped_chunks > 0 {
                            console::log_1(&format!(
                                "[sync] skipped {} non-keyframe chunks in queue", skipped_chunks
                            ).into());
                        }

                        // Set a PTS filter to discard stale frames from the decoder's
                        // internal pipeline. WebCodecs decoding is async — the decoder
                        // may still produce frames from chunks fed BEFORE the skip.
                        // These old frames would contaminate the clock re-anchor.
                        if let Some(kf_pts) = next_kf_pts_us {
                            self.skip_frames_before_us = Some(kf_pts);
                            console::log_1(&format!(
                                "[sync] will discard decoded frames with PTS < {}us (next keyframe)",
                                kf_pts
                            ).into());
                        }

                        // Force clock re-anchor on the next decoded frame.
                        // Without this, the clock keeps advancing during the flush
                        // and all subsequent frames arrive "late" → infinite skip loop.
                        self.clock_synced_to_first_frame = false;
                        break;
                    }
                    SyncAction::Hold => {
                        break; // Too early — wait for next tick
                    }
                }
            } else {
                break; // No frames available
            }
        }

        // 5. Emit sync stats every ~60 frames (~1x/sec at 60fps)
        self.sync_stats_frame_counter += 1;
        if self.sync_stats_frame_counter >= 60 {
            self.sync_stats_frame_counter = 0;
            let (rendered, dropped, held, skipped) = self.av_sync.stats();
            self.emit_event(&PlayerEvent::SyncStats {
                rendered,
                dropped,
                held,
                skipped,
            });
        }

        // 6. Pump decoded audio to Web Audio output
        let _ = self.audio_pipeline.pump_audio();

        // 6. Update time (throttled to ~10Hz to reduce WASM→JS overhead)
        self.state.current_time_ms = clock_ms as u64;
        let wall_now = now_ms();
        if wall_now - self.last_time_update_ms >= 100.0 {
            self.last_time_update_ms = wall_now;
            self.emit_event(&PlayerEvent::TimeUpdate {
                current_ms: clock_ms as u64,
            });
        }

        // 7. Buffer state management with timeout
        let download_complete = {
            let dl = self.download.borrow();
            if let Some(rb) = &self.range_buffer {
                // Range-first mode: complete when buffer covers whole file OR stream ended
                dl.data.len() as u64 >= rb.file_size || dl.complete
            } else {
                dl.complete
            }
        };
        let has_video_frames = self.video_decoder.queue_len() > 0;
        let has_chunks = !self.chunk_queue.is_empty();
        let can_demux_more = self.download.borrow().data.len() > self.last_demux_data_len
            || !download_complete; // Streaming download still active = more data coming

        if !has_video_frames && !has_chunks && !can_demux_more {
            if download_complete {
                // All data downloaded and consumed — playback ended
                console::log_1(&"[state] Playing → Stopped (ended)".into());
                self.state.status = PlaybackStatus::Stopped;
                self.buffering_since_ms = None;
                self.emit_event(&PlayerEvent::Ended);
                self.emit_event(&PlayerEvent::StatusChanged {
                    status: PlaybackStatus::Stopped,
                });
                return false;
            } else {
                // Waiting for more data — buffering
                if self.state.status != PlaybackStatus::Buffering {
                    console::log_1(&format!(
                        "[state] {} → Buffering (no frames/chunks/data)",
                        format!("{:?}", self.state.status)
                    ).into());
                    self.state.status = PlaybackStatus::Buffering;
                    self.buffering_since_ms = Some(wall_now);
                    self.emit_event(&PlayerEvent::StatusChanged {
                        status: PlaybackStatus::Buffering,
                    });
                } else if let Some(since) = self.buffering_since_ms {
                    // Buffering timeout: emit recoverable error after 10s
                    if wall_now - since > 10_000.0 {
                        console::log_1(&"[state] Buffering timeout (10s) — emitting error".into());
                        self.emit_event(&PlayerEvent::Error {
                            message: "Buffering timeout: no data received for 10 seconds".into(),
                            recoverable: true,
                        });
                        // Reset timer to avoid spamming
                        self.buffering_since_ms = Some(wall_now);
                    }
                }
            }
        } else if self.state.status == PlaybackStatus::Buffering {
            // We have data again — resume playing
            if let Some(since) = self.buffering_since_ms {
                console::log_1(&format!(
                    "[state] Buffering → Playing (resumed after {:.0}ms)",
                    wall_now - since
                ).into());
            }
            self.state.status = PlaybackStatus::Playing;
            self.buffering_since_ms = None;
            self.emit_event(&PlayerEvent::StatusChanged {
                status: PlaybackStatus::Playing,
            });
        }

        // 8. Back-pressure on download
        let video_queue_len = self.video_decoder.queue_len();
        {
            let mut dl = self.download.borrow_mut();
            let buffer_bytes = dl.data.len();
            // Memory safety: hard-pause download if buffer exceeds 256MB
            // (WASM memory is limited to ~2-4GB, and we need room for demux copies)
            const MAX_BUFFER_BYTES: usize = 256 * 1024 * 1024;
            if buffer_bytes > MAX_BUFFER_BYTES {
                if !dl.paused {
                    console::log_1(&format!(
                        "[player] download paused: buffer={}MB exceeds limit",
                        buffer_bytes / (1024 * 1024)
                    ).into());
                }
                dl.paused = true;
            } else if video_queue_len > self.config.max_video_queue {
                dl.paused = true;
            } else if video_queue_len < self.config.resume_video_queue {
                dl.paused = false;
            }
        }

        // 9. Data flow: handled by spawn_streaming_download (continuous chunk-by-chunk).
        // No prefetch needed — streaming download keeps data flowing without gaps.

        // 10. Emit buffer update (throttled to ~4Hz)
        let buffered_bytes = self.download.borrow().data.len() as u64;
        self.state.buffered_ms = buffered_bytes; // Approximate — real ms requires demux
        if wall_now - self.last_buffer_update_ms >= 250.0 {
            self.last_buffer_update_ms = wall_now;
            self.emit_event(&PlayerEvent::BufferUpdate {
                buffered_ms: buffered_bytes,
            });
        }

        true
    }

    /// Pause playback.
    /// Suspends the AudioContext so already-scheduled audio buffers stop immediately.
    pub async fn pause(&mut self) -> Result<(), JsValue> {
        if self.state.status == PlaybackStatus::Playing
            || self.state.status == PlaybackStatus::Buffering
        {
            // Suspend audio first — stops all scheduled audio buffers immediately
            if self.audio_pipeline.is_configured() {
                self.audio_pipeline.suspend().await?;
            }

            self.state.status = PlaybackStatus::Paused;
            self.emit_event(&PlayerEvent::StatusChanged {
                status: PlaybackStatus::Paused,
            });
        }
        Ok(())
    }

    /// Set audio volume (0.0 = muted, 1.0 = full).
    pub fn set_volume(&self, volume: f64) {
        self.audio_pipeline.set_volume(volume.clamp(0.0, 1.0));
    }

    /// Stop playback and reset.
    pub fn stop(&mut self) {
        self.state.status = PlaybackStatus::Stopped;
        self.state.current_time_ms = 0;
        self.renderer.clear();
        self.chunk_queue.clear();
        // Close audio pipeline — stops all playback and releases resources
        self.audio_pipeline.close();
        self.video_decoder.flush_queue();
        self.mkv_demuxer = None;
        self.mkv_cache_created_at = 0;
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Stopped,
        });
    }

    /// Seek to a position in milliseconds.
    ///
    /// **Range-first strategy** (when SeekIndex is available):
    /// 1. SeekIndex.lookup_keyframe(timestamp_us) → exact byte_offset
    /// 2. Check RangeBuffer / download buffer for needed data
    /// 3. If missing → fetch via targeted Range request (no proportional estimation)
    /// 4. Build synthetic buffer → demuxer.seek_to_keyframe → resume playback
    ///
    /// **Fallback** (no SeekIndex or no Range support):
    /// Seeks within existing download buffer using proportional estimation.
    pub async fn seek(&mut self, time_ms: u64) -> Result<(), JsValue> {
        if !self.header_parsed {
            return Err(JsValue::from_str("Cannot seek before media is loaded"));
        }

        // Anti-double-seek: increment generation. If another seek comes in while
        // this one is running (during async Range fetch), we can detect it.
        self.seek_generation = self.seek_generation.wrapping_add(1);
        let my_generation = self.seek_generation;

        // Cancel any in-progress seek's background download and prefetch
        if self.state.status == PlaybackStatus::Seeking {
            self.download.borrow_mut().cancelled = true;
            self.prefetch.borrow_mut().cancelled = true;
            console::log_1(&"[seek] cancelling previous seek".into());
        }

        // Save pre-seek status to restore after
        let was_playing = self.state.status == PlaybackStatus::Playing
            || self.state.status == PlaybackStatus::Buffering
            || self.state.status == PlaybackStatus::Seeking;
        if self.state.status != PlaybackStatus::Seeking {
            self.pre_seek_status = Some(self.state.status);
        }

        self.state.status = PlaybackStatus::Seeking;
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Seeking,
        });
        self.emit_event(&PlayerEvent::Seeking { target_ms: time_ms });

        // 1. Clear chunk queue + flush decoders
        self.chunk_queue.clear();

        // 2. Close and reconfigure decoders — WebCodecs decoders can enter
        //    an unrecoverable error state; the safest approach is to recreate
        //    them from scratch on seek. This also flushes the internal pipeline.
        self.video_decoder.close();
        self.audio_pipeline.close();
        self.video_decoder = VideoDecoderWrapper::new();
        self.audio_pipeline = AudioPipeline::new();
        if let Some(ref media_info) = self.demuxer_media_info.clone() {
            self.reconfigure_decoders(media_info)?;
        }

        // 3. Determine seek strategy
        let timestamp_us = (time_ms as i64) * 1000;
        let actual_ms = if !self.seek_index.is_empty() && self.server_supports_range {
            // --- Range-first seek via SeekIndex ---
            let result = self.seek_range_first(timestamp_us).await;

            // Check if another seek was requested during async fetch
            if self.seek_generation != my_generation {
                console::log_1(&"[seek] aborted after Range fetch — superseded by newer seek".into());
                return Ok(());
            }

            result?
        } else if !self.seek_index.is_empty() {
            // SeekIndex available but no Range support — seek locally in download buffer
            console::log_1(&"[seek] SeekIndex local seek (no Range support)".into());
            self.seek_demuxer(timestamp_us)?
        } else {
            // No SeekIndex — legacy fallback
            console::log_1(&"[seek] legacy seek (no SeekIndex)".into());
            let needs_range = self.needs_range_seek(timestamp_us).await;
            if self.seek_generation != my_generation {
                console::log_1(&"[seek] aborted — superseded by newer seek".into());
                return Ok(());
            }
            if needs_range {
                let result = self.seek_via_range(timestamp_us).await;
                if self.seek_generation != my_generation {
                    console::log_1(&"[seek] aborted after Range fetch — superseded".into());
                    return Ok(());
                }
                result?
            } else {
                self.seek_demuxer(timestamp_us)?
            }
        };

        // 4. Re-demux a batch from the new position
        self.last_demux_data_len = 0; // Force re-demux
        self.try_demux_more();

        // 5. Resynchronize clock
        self.playback_start_time = now_ms() - actual_ms;
        self.av_sync.reset();
        self.av_sync.set_start_offset(0.0);
        self.clock_synced_to_first_frame = false;
        self.skip_frames_before_us = None;
        if self.audio_pipeline.is_configured() {
            self.audio_pipeline.reset_schedule();
        }
        self.state.current_time_ms = actual_ms as u64;

        // 6. Restore status
        let new_status = if was_playing {
            PlaybackStatus::Playing
        } else {
            PlaybackStatus::Paused
        };

        // Resume AudioContext if we were playing (new AudioContext starts suspended)
        if was_playing && self.audio_pipeline.is_configured() {
            self.audio_pipeline.resume().await?;
        }

        self.state.status = new_status;
        self.pre_seek_status = None;

        self.emit_event(&PlayerEvent::Seeked {
            actual_ms: actual_ms as u64,
        });
        self.emit_event(&PlayerEvent::StatusChanged {
            status: new_status,
        });

        Ok(())
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
        self.current_url = None;
        self.server_supports_range = false;
        self.moov_data = None;
        self.mdat_offset = 0;
        self.mdat_header_size = 0;
        self.pre_seek_status = None;
        self.demuxer_media_info = None;
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

    /// Fetch the moov box from the end of an MP4 file via Range request.
    ///
    /// For non-faststart MP4 files (moov after mdat), we:
    /// 1. Fetch moov via Range request and store it in `self.moov_data`
    /// 2. Record mdat box position/header size
    /// 3. Build a synthetic buffer (ftyp + modified-mdat + moov) to parse the header
    ///
    /// Returns true if header was successfully parsed.
    async fn try_fetch_moov_at_end(
        &mut self,
        url: &str,
        moov_offset: u64,
        file_size: u64,
        initial_data: &[u8],
    ) -> Result<bool, JsValue> {
        console::log_1(&format!(
            "[player] fetching moov: range bytes={}-{}", moov_offset, file_size - 1
        ).into());

        // Fetch from moov_offset to end of file
        let moov_data = StreamReader::fetch_range(url, moov_offset, file_size - 1).await?;

        console::log_1(&format!(
            "[player] moov fetched: {} bytes", moov_data.len()
        ).into());

        if moov_data.is_empty() {
            console::log_1(&"[player] moov data is empty, aborting".into());
            return Ok(false);
        }

        // Scan top-level boxes to find mdat position and header size
        let boxes = Mp4Demuxer::scan_top_level_boxes(initial_data);
        let mdat_box = boxes.iter().find(|b| b.is_type(b"mdat"));

        // Log all top-level boxes found
        for b in &boxes {
            console::log_1(&format!(
                "[player] box: {} offset={} size={}", b.type_str(), b.offset, b.size
            ).into());
        }

        match mdat_box {
            Some(mdat) => {
                self.mdat_offset = mdat.offset as usize;
                // Header size: 8 normally, 16 for extended (size == 1 in first 4 bytes)
                let i = mdat.offset as usize;
                if i + 4 <= initial_data.len() {
                    let size_u32 = u32::from_be_bytes([
                        initial_data[i],
                        initial_data[i + 1],
                        initial_data[i + 2],
                        initial_data[i + 3],
                    ]);
                    self.mdat_header_size = if size_u32 == 1 { 16 } else { 8 };
                } else {
                    self.mdat_header_size = 8;
                }
                console::log_1(&format!(
                    "[player] mdat at offset={}, header_size={}",
                    self.mdat_offset, self.mdat_header_size
                ).into());
            }
            None => {
                console::log_1(&"[player] WARNING: mdat box not found in initial data".into());
                // Without mdat position, we can't build a correct synthetic buffer.
                // Fall back to linear download.
                return Ok(false);
            }
        }

        // Store moov data for synthetic buffer building
        self.moov_data = Some(moov_data);

        // Build synthetic buffer and try to parse header
        let synthetic = self.build_demux_buffer();
        console::log_1(&format!(
            "[player] synthetic buffer: {} bytes (download={}, moov={})",
            synthetic.len(),
            self.download.borrow().data.len(),
            self.moov_data.as_ref().map_or(0, |m| m.len())
        ).into());

        if synthetic.is_empty() {
            console::log_1(&"[player] synthetic buffer empty, aborting".into());
            self.moov_data = None;
            return Ok(false);
        }

        // Parse header from the synthetic buffer
        let format = detect_format(&synthetic);
        if format != ContainerFormat::Mp4 {
            console::log_1(&format!(
                "[player] synthetic buffer format mismatch: {:?}", format
            ).into());
            self.moov_data = None;
            return Ok(false);
        }

        let mut demuxer = Mp4Demuxer::new();
        let media_info = match demuxer.parse_header(&synthetic) {
            Ok(info) => {
                console::log_1(&format!(
                    "[player] moov-at-end parse OK: {} video tracks, {} audio tracks",
                    info.video_tracks.len(), info.audio_tracks.len()
                ).into());
                info
            }
            Err(e) => {
                console::log_1(&format!(
                    "[player] moov-at-end parse FAILED: {:?}", e
                ).into());
                self.moov_data = None;
                return Ok(false);
            }
        };

        // Configure decoders (same as try_parse_header)
        self.configure_decoders(&media_info)?;

        self.header_parsed = true;
        self.demuxer_format = Some(format);

        Ok(true)
    }

    /// Build the buffer to pass to the mp4 demuxer.
    ///
    /// For non-faststart MP4 (moov_data is Some):
    /// The download buffer is `[ftyp][mdat data (partial)]` — the mdat box header
    /// claims a huge size (original file's mdat) but we only have partial data.
    /// The mp4 crate needs moov to parse the header, and moov is at the end.
    ///
    /// Strategy: copy download data as-is (preserving all byte offsets), then
    /// patch the mdat box header to claim only the downloaded size, and append
    /// moov right after. This way:
    /// - The mp4 crate scans boxes: ftyp → mdat(truncated) → moov ✓
    /// - Sample offsets in stbl are absolute file positions → they point to the
    ///   same bytes in our buffer because we didn't move anything ✓
    /// - Samples beyond downloaded range: read_sample fails → next_chunk returns None ✓
    ///
    /// For faststart MP4 / MKV / WebM:
    /// Returns a clone of the download buffer as-is.
    fn build_demux_buffer(&self) -> Vec<u8> {
        let dl = self.download.borrow();

        if let Some(moov_data) = &self.moov_data {
            let download_len = dl.data.len();

            // Copy all downloaded data as-is (preserves absolute byte offsets)
            let mut buf = Vec::with_capacity(download_len + moov_data.len());
            buf.extend_from_slice(&dl.data);

            // Patch the mdat box header in-place to claim the truncated size
            // so the mp4 crate can skip past it and find moov after it
            if self.mdat_offset + self.mdat_header_size <= download_len {
                let new_mdat_total = (download_len - self.mdat_offset) as u64;
                let i = self.mdat_offset;

                if self.mdat_header_size == 16 {
                    // Extended size: [1u32][mdat][size_u64]
                    // size_u32 stays as 1, patch the u64 at offset+8
                    if i + 16 <= buf.len() {
                        buf[i + 8..i + 16].copy_from_slice(&new_mdat_total.to_be_bytes());
                    }
                } else {
                    // Normal: [size_u32][mdat]
                    if i + 4 <= buf.len() {
                        buf[i..i + 4].copy_from_slice(&(new_mdat_total as u32).to_be_bytes());
                    }
                }
            }

            // Append moov right after — the mp4 crate finds it after scanning past mdat
            buf.extend_from_slice(moov_data);

            buf
        } else if self.mdat_offset > 0 && self.mdat_header_size > 0 {
            // Faststart MP4: moov is before mdat, but mdat box header claims
            // the full file size. Patch it to match the current buffer size
            // so the mp4 crate can parse without "box larger than file" error.
            let mut buf = dl.data.clone();
            let i = self.mdat_offset;
            if i + self.mdat_header_size <= buf.len() {
                let new_mdat_total = (buf.len() - i) as u64;
                if self.mdat_header_size == 16 {
                    if i + 16 <= buf.len() {
                        buf[i + 8..i + 16].copy_from_slice(&new_mdat_total.to_be_bytes());
                    }
                } else {
                    if i + 4 <= buf.len() {
                        buf[i..i + 4].copy_from_slice(&(new_mdat_total as u32).to_be_bytes());
                    }
                }
            }
            buf
        } else {
            // Fallback: mdat_offset not yet known. Scan the buffer for mdat
            // and patch it on-the-fly. This handles the faststart case where
            // load_mp4_range fetched moov via Range but didn't detect mdat
            // (because header_data only covered ftyp+moov, not the mdat that
            // follows in the file).
            let mut buf = dl.data.clone();
            if buf.len() >= 12 {
                let boxes = Mp4Demuxer::scan_top_level_boxes(&buf);
                if let Some(mdat) = boxes.iter().find(|b| b.is_type(b"mdat")) {
                    let i = mdat.offset as usize;
                    let hdr_size = if i + 4 <= buf.len() {
                        let size_u32 = u32::from_be_bytes([
                            buf[i], buf[i + 1], buf[i + 2], buf[i + 3],
                        ]);
                        if size_u32 == 1 { 16 } else { 8 }
                    } else {
                        8
                    };
                    if i + hdr_size <= buf.len() {
                        let new_mdat_total = (buf.len() - i) as u64;
                        if hdr_size == 16 {
                            buf[i + 8..i + 16].copy_from_slice(&new_mdat_total.to_be_bytes());
                        } else {
                            buf[i..i + 4].copy_from_slice(&(new_mdat_total as u32).to_be_bytes());
                        }
                    }
                }
            }
            buf
        }
    }

    /// Emit a download progress event from current SharedDownload state (throttled to ~2Hz).
    fn emit_download_progress(&mut self) {
        let wall_now = now_ms();
        if wall_now - self.last_download_progress_ms < 500.0 {
            return;
        }
        self.last_download_progress_ms = wall_now;
        let (received, total) = {
            let dl = self.download.borrow();
            (dl.data.len() as u64, dl.content_length)
        };
        self.emit_event(&PlayerEvent::DownloadProgress {
            received_bytes: received,
            total_bytes: total,
        });
    }

    /// Configure decoders from demuxer MediaInfo and emit events.
    /// Stores the MediaInfo for reconfiguration on seek/error recovery.
    fn configure_decoders(&mut self, media_info: &demuxer::MediaInfo) -> Result<(), JsValue> {
        self.demuxer_media_info = Some(media_info.clone());
        // Configure video decoder
        if let Some(video_track) = media_info.video_tracks.first() {
            console::log_1(
                &format!(
                    "[configure] Video: codec={}, {}x{}, codec_config={} bytes, first_bytes={:02X?}",
                    video_track.codec_string,
                    video_track.width,
                    video_track.height,
                    video_track.codec_config.len(),
                    &video_track.codec_config[..video_track.codec_config.len().min(16)]
                )
                .into(),
            );
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

            // Set adaptive A/V sync threshold based on detected FPS
            if let Some(fps) = video_track.fps {
                self.av_sync.set_fps(fps);
                console::log_1(&format!(
                    "[configure] A/V sync threshold set to {:.1}ms for {:.1}fps",
                    self.av_sync.threshold_ms(), fps
                ).into());
            }
        }

        // Configure audio decoder (graceful — unsupported codecs like AC-3 skip audio)
        if let Some(audio_track) = media_info.audio_tracks.first() {
            console::log_1(
                &format!(
                    "[configure] Audio: codec={}, rate={}, ch={}, codec_config={} bytes, first_bytes={:02X?}",
                    audio_track.codec_string,
                    audio_track.sample_rate,
                    audio_track.channels,
                    audio_track.codec_config.len(),
                    &audio_track.codec_config[..audio_track.codec_config.len().min(16)]
                )
                .into(),
            );
            match self.audio_pipeline.configure(
                &audio_track.codec_string,
                audio_track.sample_rate,
                audio_track.channels,
                Some(&audio_track.codec_config),
            ) {
                Ok(()) => {
                    self.state.has_audio = true;
                    self.av_sync.set_has_audio(true);
                }
                Err(e) => {
                    let msg = format!(
                        "Audio codec '{}' not supported by WebCodecs — video-only playback",
                        audio_track.codec_string
                    );
                    console::log_1(
                        &format!("[configure] ⚠ {}: {:?}", msg, e).into(),
                    );
                    self.emit_event(&PlayerEvent::Error {
                        message: msg,
                        recoverable: true,
                    });
                    self.state.has_audio = false;
                    self.av_sync.set_has_audio(false);
                }
            }
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

        self.emit_event(&PlayerEvent::MediaLoaded { info: player_info });
        self.emit_event(&PlayerEvent::StatusChanged {
            status: PlaybackStatus::Ready,
        });

        Ok(())
    }

    /// Reconfigure decoders from stored MediaInfo (used during seek).
    /// Does NOT emit MediaLoaded/Ready events — only recreates the WebCodecs decoders.
    fn reconfigure_decoders(&mut self, media_info: &demuxer::MediaInfo) -> Result<(), JsValue> {
        if let Some(video_track) = media_info.video_tracks.first() {
            self.video_decoder.configure(
                &video_track.codec_string,
                video_track.width,
                video_track.height,
                Some(&video_track.codec_config),
            )?;
        }
        if let Some(audio_track) = media_info.audio_tracks.first() {
            match self.audio_pipeline.configure(
                &audio_track.codec_string,
                audio_track.sample_rate,
                audio_track.channels,
                Some(&audio_track.codec_config),
            ) {
                Ok(()) => {
                    self.state.has_audio = true;
                    self.av_sync.set_has_audio(true);
                }
                Err(e) => {
                    console::log_1(
                        &format!(
                            "[configure] ⚠ Audio codec '{}' not supported, video-only: {:?}",
                            audio_track.codec_string, e
                        )
                        .into(),
                    );
                }
            }
        }
        Ok(())
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
            return Ok(false);
        }

        // Try to parse the header with current data
        let media_info = match format {
            ContainerFormat::Mp4 => {
                // Detect mdat position BEFORE parse_header — the mp4 crate
                // rejects buffers where a box claims more bytes than available.
                // For streaming, the mdat box is truncated so we must patch its
                // header to match the current buffer size.
                if self.mdat_offset == 0 {
                    let boxes = Mp4Demuxer::scan_top_level_boxes(&data);
                    if let Some(mdat) = boxes.iter().find(|b| b.is_type(b"mdat")) {
                        self.mdat_offset = mdat.offset as usize;
                        let i = mdat.offset as usize;
                        if i + 4 <= data.len() {
                            let size_u32 = u32::from_be_bytes([
                                data[i], data[i + 1], data[i + 2], data[i + 3],
                            ]);
                            self.mdat_header_size = if size_u32 == 1 { 16 } else { 8 };
                        } else {
                            self.mdat_header_size = 8;
                        }
                        console::log_1(&format!(
                            "[mp4] mdat detected at offset={}, header_size={}, claimed_size={}",
                            self.mdat_offset, self.mdat_header_size, mdat.size
                        ).into());
                    }
                }

                // Patch the mdat header in-place to match actual buffer size
                // so the mp4 crate can parse the truncated stream.
                let parse_data = if self.mdat_offset > 0 && self.mdat_header_size > 0 {
                    let mut buf = data.clone();
                    let i = self.mdat_offset;
                    if i + self.mdat_header_size <= buf.len() {
                        let new_mdat_total = (buf.len() - i) as u64;
                        if self.mdat_header_size == 16 {
                            buf[i + 8..i + 16].copy_from_slice(&new_mdat_total.to_be_bytes());
                        } else {
                            buf[i..i + 4].copy_from_slice(&(new_mdat_total as u32).to_be_bytes());
                        }
                    }
                    buf
                } else {
                    data
                };

                let mut demuxer = Mp4Demuxer::new();
                match demuxer.parse_header(&parse_data) {
                    Ok(info) => info,
                    Err(_) => return Ok(false),
                }
            }
            ContainerFormat::Mkv | ContainerFormat::WebM => {
                let mut demuxer = MkvDemuxer::new();
                match demuxer.parse_header_streaming(&data) {
                    Ok(info) => {
                        // Save header bytes (everything before first Cluster) for Range-based seek
                        if let Some(cluster_pos) = find_cluster_offset(&data) {
                            self.mkv_header_bytes = Some(data[..cluster_pos].to_vec());
                            console::log_1(&format!(
                                "[player] MKV header saved: {} bytes (first Cluster at {})",
                                cluster_pos, cluster_pos
                            ).into());
                        }
                        info
                    }
                    Err(_) => return Ok(false),
                }
            }
            _ => {
                self.state.status = PlaybackStatus::Error;
                return Err(JsValue::from_str("Unsupported container format"));
            }
        };

        self.configure_decoders(&media_info)?;
        self.header_parsed = true;
        self.demuxer_format = Some(format);

        Ok(true)
    }

    /// Demux more encoded chunks from the download buffer into chunk_queue.
    ///
    /// **MP4**: Re-creates the demuxer each time (cheap — O(1) resume via cursor indices).
    /// **MKV**: Caches the demuxer to avoid expensive O(n) frame-skipping on every call.
    ///   Only recreates when the cached demuxer's cursor is exhausted AND new data is available.
    fn try_demux_more(&mut self) {
        let data_len = self.download.borrow().data.len();

        let format = match self.demuxer_format {
            Some(f) => f,
            None => return,
        };

        const MAX_DRAIN: usize = 2048;
        let mut count = 0;

        match format {
            ContainerFormat::Mp4 => {
                // Only re-demux if we have significant new data since last session.
                // Re-parsing the header is expensive (clones full buffer), so require
                // at least 64KB of new data before bothering.
                let min_new_bytes = if self.chunk_queue.is_empty() { 0 } else { 65536 };
                if data_len <= self.last_demux_data_len + min_new_bytes {
                    return;
                }
                let data = self.build_demux_buffer();
                let mut demuxer = Mp4Demuxer::new();
                if let Err(e) = demuxer.parse_header(&data) {
                    // Only log once per data_len to avoid console spam
                    if self.last_demux_data_len == 0 || data_len != self.last_demux_data_len {
                        console::log_1(&format!(
                            "[demux] MP4 parse_header failed (buf={}KB, dl={}KB, mdat_off={}, mdat_hdr={}, moov_data={}): {:?}",
                            data.len() / 1024,
                            self.download.borrow().data.len() / 1024,
                            self.mdat_offset,
                            self.mdat_header_size,
                            self.moov_data.as_ref().map_or(0, |m| m.len()),
                            e
                        ).into());
                    }
                    self.last_demux_data_len = data_len;
                    return;
                }
                // Resume from last position (O(1) — just sets cursor indices)
                if let Some(ref cursors) = self.mp4_cursors {
                    demuxer.set_sample_positions(cursors.clone());
                }
                while count < MAX_DRAIN {
                    match demuxer.next_chunk() {
                        Ok(Some(chunk)) => {
                            self.chunk_queue.push_back(chunk);
                            count += 1;
                        }
                        Ok(None) => break,
                        Err(e) => {
                            console::log_1(&format!(
                                "[demux] MP4 next_chunk error after {} chunks: {:?}",
                                count, e
                            ).into());
                            break;
                        }
                    }
                }
                if count == 0 && self.mp4_cursors.is_none() {
                    console::log_1(&format!(
                        "[demux] MP4 WARNING: 0 chunks demuxed on first attempt (buf={}KB, moov_data={})",
                        data.len() / 1024,
                        self.moov_data.is_some()
                    ).into());
                }
                self.mp4_cursors = Some(demuxer.sample_positions());
            }
            ContainerFormat::Mkv | ContainerFormat::WebM => {
                // Strategy: reuse cached demuxer (O(1) per chunk).
                // On exhaustion + new data: recreate with incremental buffer only.
                // The expensive O(n) skip_frames fallback is ONLY used on the very first
                // call (when mkv_frames_read == 0, so skip is a no-op).

                // 1. Try reading from cached demuxer first (O(1) per chunk)
                if let Some(ref mut demuxer) = self.mkv_demuxer {
                    while count < MAX_DRAIN {
                        match demuxer.next_chunk() {
                            Ok(Some(chunk)) => {
                                self.chunk_queue.push_back(chunk);
                                count += 1;
                            }
                            _ => break,
                        }
                    }
                    self.mkv_frames_read = demuxer.frames_read();
                    if count > 0 {
                        self.last_demux_data_len = data_len;
                        return;
                    }
                }

                // 2. Only recreate if new data arrived
                if data_len <= self.last_demux_data_len {
                    return;
                }

                // 3. Try incremental buffer: header + new data from Cluster boundary
                //    This avoids the expensive O(n) skip_frames.
                if let Some(ref header) = self.mkv_header_bytes {
                    if self.mkv_cache_created_at > 0 {
                        // Build synthetic buffer while holding the borrow, then release
                        let synthetic = {
                            let dl = self.download.borrow();
                            let new_start = self.mkv_cache_created_at;
                            if new_start < dl.data.len() {
                                let new_data = &dl.data[new_start..];
                                if let Some(cluster_pos) = find_cluster_offset(new_data) {
                                    let cluster_data = &new_data[cluster_pos..];
                                    let mut buf = Vec::with_capacity(
                                        header.len() + cluster_data.len(),
                                    );
                                    buf.extend_from_slice(header);
                                    buf.extend_from_slice(cluster_data);
                                    Some(buf)
                                } else {
                                    None // No Cluster boundary yet — wait for more data
                                }
                            } else {
                                None
                            }
                        }; // dl dropped here

                        if let Some(synthetic) = synthetic {
                            let synth_len = synthetic.len();
                            let t0 = now_ms();
                            let mut demuxer = MkvDemuxer::new();
                            if demuxer.parse_header_streaming(&synthetic).is_ok() {
                                let t_parse = now_ms() - t0;
                                while count < MAX_DRAIN {
                                    match demuxer.next_chunk() {
                                        Ok(Some(chunk)) => {
                                            self.chunk_queue.push_back(chunk);
                                            count += 1;
                                        }
                                        Ok(None) => break,
                                        Err(_) => break,
                                    }
                                }
                                let t_total = now_ms() - t0;
                                if count > 0 {
                                    self.mkv_frames_read += demuxer.frames_read();
                                    self.mkv_cache_created_at = data_len;
                                    self.mkv_demuxer = Some(demuxer);
                                    console::log_1(&format!(
                                        "[demux] incremental OK: {}ch in {:.1}ms (parse={:.1}ms, synth={}B)",
                                        count, t_total, t_parse, synth_len
                                    ).into());
                                }
                                // else: Cluster found but no frames — wait for more data
                            } else {
                                console::log_1(&format!(
                                    "[demux] incremental parse_header failed, synth_len={}",
                                    synth_len
                                ).into());
                            }
                        }
                    }
                }

                // 4. First-time creation only (mkv_cache_created_at == 0 means never
                //    created before). This is cheap because mkv_frames_read == 0 at this
                //    point, so skip_frames is a no-op.
                if self.mkv_cache_created_at == 0 {
                    let t0 = now_ms();
                    let data = self.build_demux_buffer();
                    let mut demuxer = MkvDemuxer::new();
                    if demuxer.parse_header_streaming(&data).is_err() {
                        self.last_demux_data_len = data_len;
                        return;
                    }
                    let t_parse = now_ms() - t0;
                    // mkv_frames_read is always 0 here, so skip is a no-op
                    while count < MAX_DRAIN {
                        match demuxer.next_chunk() {
                            Ok(Some(chunk)) => {
                                self.chunk_queue.push_back(chunk);
                                count += 1;
                            }
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                    let t_total = now_ms() - t0;
                    console::log_1(&format!(
                        "[demux] first-time MKV: {}ch in {:.1}ms (parse={:.1}ms, buf={}B)",
                        count, t_total, t_parse, data.len()
                    ).into());
                    self.mkv_frames_read = demuxer.frames_read();
                    self.mkv_cache_created_at = data_len;
                    self.mkv_demuxer = Some(demuxer);
                }
            }
            _ => {}
        }

        self.last_demux_data_len = data_len;
    }

    /// Seek the demuxer to the nearest keyframe before `timestamp_us`.
    /// Returns the actual timestamp in ms that was seeked to.
    fn seek_demuxer(&mut self, timestamp_us: i64) -> Result<f64, JsValue> {
        let format = self.demuxer_format.ok_or_else(|| {
            JsValue::from_str("No demuxer format set")
        })?;

        let data = self.build_demux_buffer();

        match format {
            ContainerFormat::Mp4 => {
                let mut demuxer = Mp4Demuxer::new();
                demuxer.parse_header(&data).map_err(|e| {
                    JsValue::from_str(&format!("Seek: MP4 parse error: {}", e))
                })?;

                demuxer.seek_to_keyframe(timestamp_us).map_err(|e| {
                    JsValue::from_str(&format!("Seek error: {}", e))
                })?;

                // Save the new cursor positions for try_demux_more
                self.mp4_cursors = Some(demuxer.sample_positions());
            }
            ContainerFormat::Mkv | ContainerFormat::WebM => {
                let mut demuxer = MkvDemuxer::new();
                demuxer.parse_header_streaming(&data).map_err(|e| {
                    JsValue::from_str(&format!("Seek: MKV parse error: {}", e))
                })?;

                demuxer.seek_to_keyframe(timestamp_us).map_err(|e| {
                    JsValue::from_str(&format!("MKV seek error: {}", e))
                })?;

                self.mkv_frames_read = demuxer.frames_read();
                // Cache the positioned demuxer so try_demux_more can reuse it
                // directly. This avoids an expensive re-parse + skip_frames.
                self.mkv_demuxer = Some(demuxer);
                self.mkv_cache_created_at = self.download.borrow().data.len();
            }
            _ => {
                return Err(JsValue::from_str("Unsupported format for seek"));
            }
        }

        // Return the target time in ms (actual keyframe time may differ slightly)
        Ok(timestamp_us as f64 / 1000.0)
    }

    /// Range-first seek using SeekIndex for precise byte offsets.
    ///
    /// Flow:
    /// 1. SeekIndex.lookup_keyframe(timestamp_us) → exact byte_offset
    /// 2. Check if we have data around that offset (in download buffer or RangeBuffer)
    /// 3. If not → fetch a 5MB window via Range request
    /// 4. Build demux buffer → seek_to_keyframe → cache demuxer
    /// 5. Cancel old prefetch, set up new prefetch from seek position
    async fn seek_range_first(&mut self, timestamp_us: i64) -> Result<f64, JsValue> {
        let url = self.current_url.clone().ok_or_else(|| {
            JsValue::from_str("No URL for Range seek")
        })?;

        let format = self.demuxer_format.ok_or_else(|| {
            JsValue::from_str("No demuxer format for seek")
        })?;

        // 1. Lookup keyframe byte offset from SeekIndex
        let (keyframe_offset, keyframe_ts_us) = if let Some(entry) = self.seek_index.lookup_keyframe(timestamp_us) {
            console::log_1(&format!(
                "[seek] SeekIndex: target={}us → keyframe at byte {}, ts={}us",
                timestamp_us, entry.byte_offset, entry.timestamp_us
            ).into());
            (entry.byte_offset, entry.timestamp_us)
        } else {
            // No keyframe found — seek to start
            console::log_1(&"[seek] SeekIndex empty or target before first keyframe → seek to 0".into());
            (0u64, 0i64)
        };

        // 2. Determine the data window we need
        const SEEK_WINDOW: u64 = 16 * 1024 * 1024; // 16MB
        let file_size = self.range_buffer.as_ref()
            .map(|rb| rb.file_size)
            .unwrap_or(self.download.borrow().content_length);

        // For MP4: we need header data (byte 0..header_end) + data around keyframe
        // For MKV: we need saved header bytes + data from Cluster at/near keyframe offset
        let fetch_start = keyframe_offset.saturating_sub(64 * 1024); // Small margin for Cluster headers
        let fetch_end = (keyframe_offset + SEEK_WINDOW).min(file_size);

        // 3. Check if data is already available
        let have_data = {
            let dl = self.download.borrow();
            let dl_has = dl.data.len() as u64 > keyframe_offset + 512 * 1024; // At least 512KB past keyframe
            let rb_has = self.range_buffer.as_ref()
                .map(|rb| rb.has_range(fetch_start, (keyframe_offset + 512 * 1024).min(file_size)))
                .unwrap_or(false);
            dl_has || rb_has
        };

        if !have_data {
            console::log_1(&format!(
                "[seek] fetching Range: bytes {}-{} ({} KB)",
                fetch_start, fetch_end - 1, (fetch_end - fetch_start) / 1024
            ).into());

            let range_data = StreamReader::fetch_range(&url, fetch_start, fetch_end - 1).await?;

            // Insert into RangeBuffer
            if let Some(rb) = &mut self.range_buffer {
                rb.insert(fetch_start, range_data.clone());
            }

            // For MP4: we need contiguous data from byte 0 for absolute offsets
            // For MKV: we use synthetic buffer (header + cluster data)
            match format {
                ContainerFormat::Mp4 => {
                    // Extend download buffer to cover the keyframe position
                    let mut dl = self.download.borrow_mut();
                    let current_len = dl.data.len() as u64;
                    if fetch_start > current_len {
                        // Fill gap with zeros to preserve absolute byte offsets
                        dl.data.resize(fetch_start as usize, 0);
                    }
                    if fetch_start + range_data.len() as u64 > current_len {
                        // Extend with fetched data
                        let skip = if current_len > fetch_start {
                            (current_len - fetch_start) as usize
                        } else {
                            0
                        };
                        if skip < range_data.len() {
                            dl.data.extend_from_slice(&range_data[skip..]);
                        }
                    }
                }
                ContainerFormat::Mkv | ContainerFormat::WebM => {
                    // MKV uses synthetic buffer, no need to extend download linearly
                    // Data is already in RangeBuffer
                }
                _ => {}
            }
        }

        // 4. Cancel old prefetch and background download
        self.prefetch.borrow_mut().cancelled = true;
        self.prefetch = PrefetchState::new();
        self.download.borrow_mut().cancelled = true;

        // 5. Build demux buffer and seek
        match format {
            ContainerFormat::Mp4 => {
                // MP4: use standard build_demux_buffer (works because download.data has the right bytes)
                self.last_demux_data_len = 0;
                self.seek_demuxer(timestamp_us)?;

                // Start new streaming from end of fetched range
                let resume_from = fetch_end;
                if resume_from < file_size {
                    let data = std::mem::take(&mut self.download.borrow_mut().data);
                    let new_download = SharedDownload::new();
                    {
                        let mut dl = new_download.borrow_mut();
                        dl.data = data;
                        dl.content_length = file_size;
                    }
                    self.download = new_download;

                    // Spawn prefetch for the next window ahead of playback
                    self.spawn_prefetch(&url, fetch_end, SEEK_WINDOW, file_size);
                } else {
                    // Reset download state (uncancelled)
                    let data = std::mem::take(&mut self.download.borrow_mut().data);
                    let new_download = SharedDownload::new();
                    {
                        let mut dl = new_download.borrow_mut();
                        dl.data = data;
                        dl.content_length = file_size;
                        dl.complete = true;
                    }
                    self.download = new_download;
                }
            }
            ContainerFormat::Mkv | ContainerFormat::WebM => {
                let header_bytes = self.mkv_header_bytes.clone().ok_or_else(|| {
                    JsValue::from_str("No MKV header saved for seek")
                })?;

                // Get the data around the keyframe — either from download or RangeBuffer
                let range_data = if let Some(rb) = &self.range_buffer {
                    rb.get_range(fetch_start, fetch_end.min(file_size))
                        .unwrap_or_default()
                } else {
                    let dl = self.download.borrow();
                    if (fetch_start as usize) < dl.data.len() {
                        let end = (fetch_end as usize).min(dl.data.len());
                        dl.data[fetch_start as usize..end].to_vec()
                    } else {
                        Vec::new()
                    }
                };

                if range_data.is_empty() {
                    return Err(JsValue::from_str("No data available at seek target"));
                }

                // Find Cluster boundary in the fetched data
                let cluster_pos = find_cluster_offset(&range_data).ok_or_else(|| {
                    JsValue::from_str("No MKV Cluster found at seek target")
                })?;

                // Build synthetic buffer: [MKV header] + [data from Cluster boundary]
                let cluster_data = &range_data[cluster_pos..];
                let mut synthetic = Vec::with_capacity(header_bytes.len() + cluster_data.len());
                synthetic.extend_from_slice(&header_bytes);
                synthetic.extend_from_slice(cluster_data);

                console::log_1(&format!(
                    "[seek] MKV synthetic: {} bytes (header={}, cluster_data={})",
                    synthetic.len(), header_bytes.len(), cluster_data.len()
                ).into());

                // Replace download buffer with synthetic data
                let new_download = SharedDownload::new();
                {
                    let mut dl = new_download.borrow_mut();
                    dl.data = synthetic;
                    dl.content_length = file_size;
                }
                self.download = new_download;

                // Reset MKV demuxer state
                self.mkv_frames_read = 0;
                self.mkv_demuxer = None;
                self.mkv_cache_created_at = 0;
                self.last_demux_data_len = 0;

                // Parse + seek
                let mut demuxer = MkvDemuxer::new();
                demuxer.parse_header_streaming(&self.download.borrow().data).map_err(|e| {
                    JsValue::from_str(&format!("MKV seek parse error: {}", e))
                })?;

                demuxer.seek_to_keyframe(timestamp_us).map_err(|e| {
                    JsValue::from_str(&format!("MKV seek error: {}", e))
                })?;

                self.mkv_frames_read = demuxer.frames_read();
                let dl_len = self.download.borrow().data.len();
                self.mkv_demuxer = Some(demuxer);
                self.mkv_cache_created_at = dl_len;

                // Spawn prefetch for the next window
                let next_window_start = fetch_end;
                if next_window_start < file_size {
                    self.spawn_prefetch(&url, next_window_start, SEEK_WINDOW, file_size);
                }
            }
            _ => {
                return Err(JsValue::from_str("Unsupported format for seek"));
            }
        }

        // Return the keyframe timestamp (more accurate than the requested time)
        Ok(keyframe_ts_us as f64 / 1000.0)
    }

    /// Check if a seek target requires a Range request (data not yet buffered).
    /// If range support / file size are unknown, does a HEAD probe to find out.
    async fn needs_range_seek(&mut self, timestamp_us: i64) -> bool {
        let dl = self.download.borrow();
        let downloaded = dl.data.len() as u64;

        if dl.complete {
            console::log_1(&"[seek] download complete — local seek".into());
            return false; // Fully downloaded
        }

        // Get duration from media info
        let duration_us = self.demuxer_media_info
            .as_ref()
            .and_then(|info| info.duration_us)
            .unwrap_or(0);

        if duration_us <= 0 {
            console::log_1(&"[seek] unknown duration — local seek".into());
            return false; // Unknown duration — can't estimate
        }

        let mut content_length = dl.content_length;
        let mut supports_range = self.server_supports_range;
        drop(dl);

        // If we're missing file size or range support info, do a HEAD probe.
        // Fallback: if HEAD fails, try a small GET Range (bytes=0-0) to detect support.
        if content_length == 0 || !supports_range {
            if let Some(ref url) = self.current_url {
                console::log_1(&"[seek] probing server with HEAD request...".into());
                match StreamReader::head(url).await {
                    Ok(head_info) => {
                        console::log_1(&format!(
                            "[seek] HEAD probe: size={}, range={}",
                            head_info.content_length, head_info.supports_range
                        ).into());
                        if head_info.content_length > 0 {
                            content_length = head_info.content_length;
                            let mut dl = self.download.borrow_mut();
                            dl.content_length = content_length;
                        }
                        if head_info.supports_range {
                            supports_range = true;
                            self.server_supports_range = true;
                        }
                    }
                    Err(e) => {
                        console::log_1(&format!(
                            "[seek] HEAD probe failed: {:?}, trying GET Range fallback", e
                        ).into());
                        // Fallback: try a tiny GET Range request to detect support
                        match StreamReader::fetch_range(url, 0, 0).await {
                            Ok(data) => {
                                if !data.is_empty() {
                                    supports_range = true;
                                    self.server_supports_range = true;
                                    console::log_1(&"[seek] GET Range(0-0) succeeded — Range supported".into());
                                }
                            }
                            Err(_) => {
                                console::log_1(&"[seek] GET Range(0-0) also failed — Range not supported".into());
                            }
                        }
                    }
                }
            }
        }

        if !supports_range || content_length == 0 {
            console::log_1(&format!(
                "[seek] Range not available (supports_range={}, content_length={}) — local seek",
                supports_range, content_length
            ).into());
            return false;
        }

        // Proportional estimate: what byte offset corresponds to the seek target?
        let ratio = timestamp_us as f64 / duration_us as f64;
        let estimated_byte = (ratio * content_length as f64) as u64;

        let needs_range = estimated_byte > (downloaded * 9 / 10);
        console::log_1(&format!(
            "[seek] estimated_byte={}, downloaded={}, needs_range={}",
            estimated_byte, downloaded, needs_range
        ).into());

        needs_range
    }

    /// Perform a seek via HTTP Range request.
    ///
    /// Strategy:
    /// 1. Estimate the byte offset proportionally (target_time / duration * file_size)
    /// 2. Subtract a margin to catch a Cluster/keyframe before the target
    /// 3. Fetch data via Range request
    /// 4. For MKV: build synthetic buffer = saved header + range data from Cluster boundary
    /// 5. For MP4: build synthetic buffer = range data + moov (if moov-at-end)
    /// 6. Cancel old background download, replace buffer, start new streaming download
    async fn seek_via_range(&mut self, timestamp_us: i64) -> Result<f64, JsValue> {
        let url = self.current_url.clone().ok_or_else(|| {
            JsValue::from_str("No URL for Range seek")
        })?;

        let content_length = self.download.borrow().content_length;
        let duration_us = self.demuxer_media_info
            .as_ref()
            .and_then(|info| info.duration_us)
            .unwrap_or(1) as f64;

        // Estimate byte offset (with margin to catch a keyframe before target)
        let ratio = timestamp_us as f64 / duration_us;
        let margin_bytes: u64 = 2 * 1024 * 1024; // 2MB margin for keyframe search
        let raw_offset = (ratio * content_length as f64) as u64;
        let fetch_start = raw_offset.saturating_sub(margin_bytes);

        let format = self.demuxer_format.ok_or_else(|| {
            JsValue::from_str("No demuxer format for Range seek")
        })?;

        console::log_1(&format!(
            "[seek] Range request: estimated_byte={}, fetch_start={}, file_size={}",
            raw_offset, fetch_start, content_length
        ).into());

        match format {
            ContainerFormat::Mkv | ContainerFormat::WebM => {
                self.seek_mkv_via_range(&url, fetch_start, content_length, timestamp_us).await
            }
            ContainerFormat::Mp4 => {
                self.seek_mp4_via_range(&url, fetch_start, content_length, timestamp_us).await
            }
            _ => Err(JsValue::from_str("Unsupported format for Range seek")),
        }
    }

    /// MKV Range-based seek: fetch data, find Cluster boundary, build synthetic buffer.
    async fn seek_mkv_via_range(
        &mut self,
        url: &str,
        fetch_start: u64,
        content_length: u64,
        timestamp_us: i64,
    ) -> Result<f64, JsValue> {
        let header_bytes = self.mkv_header_bytes.clone().ok_or_else(|| {
            JsValue::from_str("No MKV header saved for Range seek")
        })?;

        // Fetch a chunk of data around the estimated seek position (up to 5MB)
        let fetch_end = std::cmp::min(fetch_start + 5 * 1024 * 1024, content_length - 1);
        let range_data = StreamReader::fetch_range(url, fetch_start, fetch_end).await?;

        console::log_1(&format!(
            "[seek] MKV Range fetched: {} bytes ({}..{})",
            range_data.len(), fetch_start, fetch_end
        ).into());

        if range_data.is_empty() {
            return Err(JsValue::from_str("Range request returned empty data"));
        }

        // Find first Cluster element in the range data
        let cluster_pos = find_cluster_offset(&range_data).ok_or_else(|| {
            JsValue::from_str("No MKV Cluster found in Range data")
        })?;

        console::log_1(&format!(
            "[seek] First Cluster in range data at offset {} (absolute ~{})",
            cluster_pos, fetch_start as usize + cluster_pos
        ).into());

        // Build synthetic buffer: [MKV header] + [range data from Cluster boundary]
        let cluster_data = &range_data[cluster_pos..];
        let mut synthetic = Vec::with_capacity(header_bytes.len() + cluster_data.len());
        synthetic.extend_from_slice(&header_bytes);
        synthetic.extend_from_slice(cluster_data);

        console::log_1(&format!(
            "[seek] Synthetic MKV buffer: {} bytes (header={}, cluster_data={})",
            synthetic.len(), header_bytes.len(), cluster_data.len()
        ).into());

        // Cancel old background download
        self.download.borrow_mut().cancelled = true;

        // Create new download buffer with synthetic data
        let new_download = SharedDownload::new();
        {
            let mut dl = new_download.borrow_mut();
            dl.data = synthetic;
            dl.content_length = content_length;
            // Not complete yet — we'll stream the rest
        }
        self.download = new_download;

        // Reset MKV demuxer state
        self.mkv_frames_read = 0;
        self.mkv_demuxer = None;
        self.mkv_cache_created_at = 0;
        self.last_demux_data_len = 0;

        // Parse header + seek within synthetic buffer
        let mut demuxer = MkvDemuxer::new();
        demuxer.parse_header_streaming(&self.download.borrow().data).map_err(|e| {
            JsValue::from_str(&format!("MKV Range seek parse error: {}", e))
        })?;

        demuxer.seek_to_keyframe(timestamp_us).map_err(|e| {
            JsValue::from_str(&format!("MKV Range seek error: {}", e))
        })?;

        self.mkv_frames_read = demuxer.frames_read();
        // Cache the positioned demuxer so try_demux_more can reuse it directly
        let dl_len = self.download.borrow().data.len();
        self.mkv_demuxer = Some(demuxer);
        self.mkv_cache_created_at = dl_len;

        // Start new streaming download from end of fetched range
        let resume_from = fetch_end + 1;
        if resume_from < content_length {
            let stream = StreamReader::open_range(url, resume_from).await?;
            self.spawn_background_download(stream);
            console::log_1(&format!(
                "[seek] Streaming resumed from byte {}", resume_from
            ).into());
        } else {
            self.download.borrow_mut().complete = true;
            console::log_1(&"[seek] No more data to download after seek".into());
        }

        Ok(timestamp_us as f64 / 1000.0)
    }

    /// MP4 Range-based seek: restart download from estimated position.
    ///
    /// MP4 sample offsets (stco/co64) are absolute byte positions, so the download
    /// buffer must start at byte 0. We can't simply splice data. Instead, we restart
    /// the download from the estimated position and keep existing data + moov.
    async fn seek_mp4_via_range(
        &mut self,
        url: &str,
        fetch_start: u64,
        content_length: u64,
        timestamp_us: i64,
    ) -> Result<f64, JsValue> {
        // For MP4, fetch a chunk around the target and append it to existing data.
        // MP4 demuxer uses absolute byte offsets, so we need data at those positions.
        // The simplest approach: fetch the needed range and extend our buffer.
        let fetch_end = std::cmp::min(fetch_start + 5 * 1024 * 1024, content_length - 1);
        let current_len = self.download.borrow().data.len() as u64;

        // Only fetch if we need data beyond what we have
        if fetch_start > current_len {
            console::log_1(&format!(
                "[seek] MP4 Range fetch: {}..{} (gap from current {})",
                fetch_start, fetch_end, current_len
            ).into());

            // Fetch the range data
            let range_data = StreamReader::fetch_range(url, fetch_start, fetch_end).await?;

            // Fill gap with zeros + append range data
            // This preserves absolute byte offsets for stco/co64
            {
                let mut dl = self.download.borrow_mut();
                let gap = (fetch_start - current_len) as usize;
                dl.data.resize(current_len as usize + gap, 0);
                dl.data.extend_from_slice(&range_data);
            }

            // Cancel old background download, start new one from after the fetched range
            self.download.borrow_mut().cancelled = true;
            let resume_from = fetch_end + 1;
            if resume_from < content_length {
                // Create new shared download state (keep the data we just built)
                let data = std::mem::take(&mut self.download.borrow_mut().data);
                let new_download = SharedDownload::new();
                {
                    let mut dl = new_download.borrow_mut();
                    dl.data = data;
                    dl.content_length = content_length;
                }
                self.download = new_download;

                let stream = StreamReader::open_range(url, resume_from).await?;
                self.spawn_background_download(stream);
            }
        }

        // Now seek within the (extended) buffer
        self.last_demux_data_len = 0;
        self.seek_demuxer(timestamp_us)
    }

    /// Get the current master clock in milliseconds.
    /// Uses performance.now() offset from playback_start_time.
    /// After seek to time T, playback_start_time is set so clock starts at T.
    fn clock_ms(&self) -> f64 {
        now_ms() - self.playback_start_time
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
                // Check cancellation or pause
                {
                    let dl = download.borrow();
                    if dl.cancelled {
                        break;
                    }
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

    /// Spawn a background prefetch task to fetch a data window via Range request.
    ///
    /// Fetches `window_size` bytes starting at `start_byte` and writes the result
    /// into the shared `PrefetchState`. The synchronous `drain_prefetch()` method
    /// (called in render_tick) then moves the data into the player's buffers.
    fn spawn_prefetch(&self, url: &str, start_byte: u64, window_size: u64, file_size: u64) {
        let prefetch = self.prefetch.clone();

        // Mark in-flight
        {
            let mut pf = prefetch.borrow_mut();
            if pf.in_flight {
                return; // Already a prefetch in progress
            }
            pf.in_flight = true;
        }

        let end_byte = (start_byte + window_size).min(file_size);
        if start_byte >= end_byte {
            prefetch.borrow_mut().in_flight = false;
            return;
        }

        let url = url.to_string();
        console::log_1(&format!(
            "[prefetch] spawning: bytes {}-{} ({} KB)",
            start_byte, end_byte - 1, (end_byte - start_byte) / 1024
        ).into());

        wasm_bindgen_futures::spawn_local(async move {
            // Check cancellation
            if prefetch.borrow().cancelled {
                prefetch.borrow_mut().in_flight = false;
                return;
            }

            match StreamReader::fetch_range(&url, start_byte, end_byte - 1).await {
                Ok(data) => {
                    let len = data.len();
                    let mut pf = prefetch.borrow_mut();
                    if !pf.cancelled {
                        pf.pending_data.push((start_byte, data));
                        console::log_1(&format!(
                            "[prefetch] done: {} KB fetched", len / 1024
                        ).into());
                    }
                    pf.in_flight = false;
                }
                Err(e) => {
                    console::log_1(&format!(
                        "[prefetch] error: {:?}", e
                    ).into());
                    prefetch.borrow_mut().in_flight = false;
                }
            }
        });
    }

    /// Drain pending prefetch data into download buffer and RangeBuffer.
    ///
    /// Called from render_tick (synchronous). Returns the number of bytes drained.
    fn drain_prefetch(&mut self) -> usize {
        let pending: Vec<(u64, Vec<u8>)> = {
            let mut pf = self.prefetch.borrow_mut();
            if pf.pending_data.is_empty() {
                return 0;
            }
            std::mem::take(&mut pf.pending_data)
        };

        let mut total = 0;
        for (offset, data) in pending {
            let len = data.len();
            total += len;

            // Insert into RangeBuffer
            if let Some(rb) = &mut self.range_buffer {
                rb.insert(offset, data.clone());
            }

            // Extend download buffer if this data is contiguous with existing data
            let dl_len = self.download.borrow().data.len() as u64;
            if offset == dl_len {
                // Contiguous — extend linearly
                self.download.borrow_mut().data.extend_from_slice(&data);
            } else if offset < dl_len && offset + data.len() as u64 > dl_len {
                // Partially overlapping — extend the non-overlapping part
                let skip = (dl_len - offset) as usize;
                self.download.borrow_mut().data.extend_from_slice(&data[skip..]);
            }
            // If offset > dl_len, there's a gap — we can't extend linearly.
            // The data is still in RangeBuffer for future use.
        }

        if total > 0 {
            console::log_1(&format!(
                "[prefetch] drained {} KB into buffers", total / 1024
            ).into());
        }

        total
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
        self.mkv_demuxer = None;
        self.mkv_cache_created_at = 0;
        self.mkv_header_bytes = None;
        self.last_demux_data_len = 0;
        self.moov_data = None;
        self.mdat_offset = 0;
        self.mdat_header_size = 0;
        self.range_buffer = None;
        self.prefetch.borrow_mut().cancelled = true;
        self.prefetch = PrefetchState::new();
        self.pre_seek_status = None;
        self.demuxer_media_info = None;
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

// find_cluster_offset is now in demuxer::mkv — imported via `use demuxer::find_cluster_offset;`
