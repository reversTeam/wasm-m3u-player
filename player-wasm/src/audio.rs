use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::{
    AudioBufferSourceNode, AudioContext, AudioData, AudioDataCopyToOptions, AudioDecoder,
    AudioDecoderConfig, AudioDecoderInit, AudioSampleFormat, EncodedAudioChunk,
    EncodedAudioChunkInit, EncodedAudioChunkType, GainNode,
};

use demuxer::EncodedChunk;

/// PCM buffer produced by the software decoder (deinterleaved f32).
struct PcmBuffer {
    channels: Vec<Vec<f32>>,
    sample_rate: f32,
}

/// Which backend is active for audio decoding.
enum AudioBackend {
    /// WebCodecs AudioDecoder (hardware-accelerated).
    WebCodecs {
        decoder: AudioDecoder,
        data_queue: Rc<RefCell<VecDeque<AudioData>>>,
        error: Rc<RefCell<Option<String>>>,
        _output_closure: Closure<dyn FnMut(AudioData)>,
        _error_closure: Closure<dyn FnMut(JsValue)>,
    },
    /// Software AC-3/E-AC-3 decoder (pure Rust).
    Software {
        decoder: ac3_decode::Ac3Decoder,
        sample_rate: u32,
        channels: u32,
        pcm_queue: VecDeque<PcmBuffer>,
        /// Suppress repeated error logging (e.g., E-AC-3 unsupported on every frame).
        error_count: u32,
    },
}

/// Wrapper around WebCodecs AudioDecoder + Web Audio API playback.
/// Falls back to a pure-Rust software decoder for codecs not supported
/// by WebCodecs (AC-3, E-AC-3).
pub struct AudioPipeline {
    backend: Option<AudioBackend>,
    audio_ctx: Option<AudioContext>,
    /// GainNode for volume control — all sources connect through this.
    gain_node: Option<GainNode>,
    /// Next scheduled playback time in AudioContext seconds.
    next_play_time: f64,
    /// PTS-based scheduling origin (set after seek).
    /// When set, audio buffers are scheduled based on their PTS relative to this
    /// origin rather than purely back-to-back. This ensures audio/video alignment.
    /// (ctx_time, media_time_s): at ctx_time, media is at media_time_s.
    schedule_origin: Option<(f64, f64)>,
}

impl AudioPipeline {
    pub fn new() -> Self {
        Self {
            backend: None,
            audio_ctx: None,
            gain_node: None,
            next_play_time: 0.0,
            schedule_origin: None,
        }
    }

    /// Configure the audio decoder and create AudioContext.
    /// Tries WebCodecs first; falls back to software decoder for AC-3/E-AC-3.
    /// If an AudioContext already exists (e.g. after seek), it is reused to
    /// preserve `currentTime` continuity — creating a new one would reset the
    /// master clock to 0 and corrupt A/V sync.
    pub fn configure(
        &mut self,
        codec: &str,
        sample_rate: u32,
        channels: u32,
        codec_config: Option<&[u8]>,
    ) -> Result<(), JsValue> {
        // Reuse existing AudioContext if available (critical for seek)
        if self.audio_ctx.is_none() {
            let audio_ctx = AudioContext::new()?;
            let gain = audio_ctx.create_gain()?;
            gain.connect_with_audio_node(&audio_ctx.destination())?;
            self.gain_node = Some(gain);
            self.audio_ctx = Some(audio_ctx);
        }

        self.next_play_time = self.audio_ctx.as_ref().unwrap().current_time();

        // Check if this is an AC-3/E-AC-3 codec — use software decoder directly
        // (WebCodecs rejects these asynchronously, which causes silent failures)
        if codec == "ac-3" || codec == "ec-3" {
            web_sys::console::log_1(
                &format!(
                    "[audio] Using software AC-3/E-AC-3 decoder for codec '{}'",
                    codec
                )
                .into(),
            );
            self.backend = Some(AudioBackend::Software {
                decoder: ac3_decode::Ac3Decoder::new(),
                sample_rate,
                channels,
                pcm_queue: VecDeque::new(),
                error_count: 0,
            });
            return Ok(());
        }

        // Try WebCodecs
        let data_queue = Rc::new(RefCell::new(VecDeque::new()));
        let error = Rc::new(RefCell::new(None));

        let dq = data_queue.clone();
        let output_closure = Closure::wrap(Box::new(move |data: AudioData| {
            dq.borrow_mut().push_back(data);
        }) as Box<dyn FnMut(AudioData)>);

        let es = error.clone();
        let error_closure = Closure::wrap(Box::new(move |e: JsValue| {
            let msg = e.as_string().unwrap_or_else(|| format!("{:?}", e));
            *es.borrow_mut() = Some(msg);
        }) as Box<dyn FnMut(JsValue)>);

        let init = AudioDecoderInit::new(
            error_closure.as_ref().unchecked_ref(),
            output_closure.as_ref().unchecked_ref(),
        );

        let decoder = AudioDecoder::new(&init)?;

        let config = AudioDecoderConfig::new(codec, channels, sample_rate);
        if let Some(config_data) = codec_config {
            if !config_data.is_empty() {
                let buffer = js_sys::Uint8Array::from(config_data);
                config.set_description(&buffer.buffer());
            }
        }

        decoder.configure(&config)?;

        self.backend = Some(AudioBackend::WebCodecs {
            decoder,
            data_queue,
            error,
            _output_closure: output_closure,
            _error_closure: error_closure,
        });

        Ok(())
    }

    /// Decode an encoded audio chunk from the demuxer.
    pub fn decode(&mut self, chunk: &EncodedChunk) -> Result<(), JsValue> {
        match &mut self.backend {
            Some(AudioBackend::WebCodecs {
                decoder,
                error,
                ..
            }) => {
                if let Some(err) = error.borrow().as_ref() {
                    return Err(JsValue::from_str(&format!("Audio decoder error: {}", err)));
                }

                let chunk_type = if chunk.is_keyframe {
                    EncodedAudioChunkType::Key
                } else {
                    EncodedAudioChunkType::Delta
                };

                let data = js_sys::Uint8Array::from(chunk.data.as_slice());
                let init = EncodedAudioChunkInit::new(&data.buffer(), 0_i32, chunk_type);
                js_sys::Reflect::set(
                    init.as_ref(),
                    &"timestamp".into(),
                    &JsValue::from_f64(chunk.timestamp_us as f64),
                )?;

                let encoded_chunk = EncodedAudioChunk::new(&init)?;
                decoder.decode(&encoded_chunk)?;
                Ok(())
            }
            Some(AudioBackend::Software {
                decoder,
                sample_rate: _,
                channels: _,
                pcm_queue,
                error_count,
            }) => {
                // Software AC-3/E-AC-3 decode
                match decoder.decode_frame(&chunk.data) {
                    Ok(decoded) => {
                        // Convert interleaved to deinterleaved
                        let nch = decoded.channels as usize;
                        let samples_per_ch = decoded.samples_per_channel;
                        let mut channels_data = vec![vec![0.0f32; samples_per_ch]; nch];

                        for s in 0..samples_per_ch {
                            for ch in 0..nch {
                                channels_data[ch][s] = decoded.samples[s * nch + ch];
                            }
                        }

                        // Downmix to stereo if needed (most Web Audio contexts are stereo)
                        let final_channels = if nch > 2 {
                            downmix_to_stereo(&channels_data, nch)
                        } else {
                            channels_data
                        };

                        pcm_queue.push_back(PcmBuffer {
                            channels: final_channels,
                            sample_rate: decoded.sample_rate as f32,
                        });
                        Ok(())
                    }
                    Err(e) => {
                        // Log first few errors, then suppress to avoid flooding console
                        *error_count += 1;
                        if *error_count <= 3 {
                            let hex: String = chunk.data.iter().take(12)
                                .map(|b| format!("{:02x}", b))
                                .collect::<Vec<_>>()
                                .join(" ");
                            web_sys::console::log_1(
                                &format!(
                                    "[audio-sw] decode error: {} (len={}, head={})",
                                    e, chunk.data.len(), hex
                                ).into(),
                            );
                            if *error_count == 3 {
                                web_sys::console::log_1(
                                    &"[audio-sw] suppressing further decode errors".into(),
                                );
                            }
                        }
                        Ok(())
                    }
                }
            }
            None => Err(JsValue::from_str("Audio decoder not configured")),
        }
    }

    /// Drain all decoded audio data from the queue without closing the decoder.
    pub fn flush_queue(&mut self) {
        match &mut self.backend {
            Some(AudioBackend::WebCodecs { data_queue, .. }) => {
                for data in data_queue.borrow_mut().drain(..) {
                    data.close();
                }
            }
            Some(AudioBackend::Software { pcm_queue, .. }) => {
                pcm_queue.clear();
            }
            None => {}
        }
    }

    /// Set the PTS origin for PTS-based scheduling after seek.
    /// `media_time_s` is the media position (in seconds) that should play
    /// at the current AudioContext time. Audio buffers will be scheduled
    /// at: ctx_origin + (buffer_pts - media_time_s).
    pub fn set_schedule_origin(&mut self, media_time_s: f64) {
        if let Some(ctx) = &self.audio_ctx {
            self.schedule_origin = Some((ctx.current_time(), media_time_s));
        }
    }

    /// Reset the audio scheduling time to the current AudioContext time.
    /// Also disconnects the old GainNode and creates a fresh one, which
    /// silences any previously-scheduled AudioBufferSourceNodes still playing.
    pub fn reset_schedule(&mut self) {
        if let Some(ctx) = &self.audio_ctx {
            self.next_play_time = ctx.current_time();

            // Save current volume before disconnecting
            let current_volume = self.gain_node.as_ref()
                .map(|g| g.gain().value())
                .unwrap_or(1.0);

            // Disconnect old gain node — all previously-scheduled sources
            // were routed through it, so they'll play into the void.
            if let Some(old_gain) = self.gain_node.take() {
                let _ = old_gain.disconnect();
            }
            // Create fresh gain node with same volume
            if let Ok(gain) = ctx.create_gain() {
                let _ = gain.connect_with_audio_node(&ctx.destination());
                gain.gain().set_value(current_volume);
                self.gain_node = Some(gain);
            }
        }
    }

    /// Schedule decoded audio data for playback via AudioBufferSourceNode.
    pub fn pump_audio(&mut self) -> Result<(), JsValue> {
        let ctx = match &self.audio_ctx {
            Some(ctx) => ctx.clone(),
            None => return Ok(()),
        };

        // Drain queues into local vecs to avoid borrow conflicts with self
        enum Pending {
            WebCodecs(Vec<AudioData>),
            Software(Vec<PcmBuffer>),
        }

        let pending = match &mut self.backend {
            Some(AudioBackend::WebCodecs { data_queue, .. }) => {
                let items: Vec<_> = data_queue.borrow_mut().drain(..).collect();
                if items.is_empty() { return Ok(()); }
                Pending::WebCodecs(items)
            }
            Some(AudioBackend::Software { pcm_queue, .. }) => {
                let items: Vec<_> = pcm_queue.drain(..).collect();
                if items.is_empty() { return Ok(()); }
                Pending::Software(items)
            }
            None => return Ok(()),
        };

        match pending {
            Pending::WebCodecs(items) => {
                for audio_data in items {
                    self.schedule_audio_data(&ctx, &audio_data)?;
                    audio_data.close();
                }
            }
            Pending::Software(items) => {
                for pcm in items {
                    self.schedule_pcm(&ctx, &pcm)?;
                }
            }
        }

        Ok(())
    }

    /// Schedule WebCodecs AudioData for playback.
    fn schedule_audio_data(&mut self, ctx: &AudioContext, audio_data: &AudioData) -> Result<(), JsValue> {
        let sample_rate = audio_data.sample_rate() as f32;
        let num_channels = audio_data.number_of_channels();
        let num_frames = audio_data.number_of_frames();

        let audio_buffer = ctx.create_buffer(num_channels, num_frames, sample_rate)?;

        for ch in 0..num_channels {
            let js_buffer = js_sys::ArrayBuffer::new(num_frames * 4);
            let opts = AudioDataCopyToOptions::new(ch);
            opts.set_format(AudioSampleFormat::F32Planar);
            audio_data.copy_to_with_buffer_source(&js_buffer, &opts)?;
            let float_array = js_sys::Float32Array::new(&js_buffer);
            let mut channel_data = vec![0f32; num_frames as usize];
            float_array.copy_to(&mut channel_data);
            audio_buffer.copy_to_channel(&mut channel_data, ch as i32)?;
        }

        let source: AudioBufferSourceNode = ctx.create_buffer_source()?;
        source.set_buffer(Some(&audio_buffer));
        // Route through gain node for volume control
        if let Some(gain) = &self.gain_node {
            source.connect_with_audio_node(gain)?;
        } else {
            source.connect_with_audio_node(&ctx.destination())?;
        }

        let current_time = ctx.current_time();
        let duration_s = num_frames as f64 / sample_rate as f64;

        // PTS-based scheduling: compute target time from audio PTS
        // relative to the seek origin. This ensures audio/video alignment.
        let play_at = if let Some((ctx_origin, media_origin_s)) = self.schedule_origin {
            let audio_pts_s = audio_data.timestamp() as f64 / 1_000_000.0;
            let target = ctx_origin + (audio_pts_s - media_origin_s);
            // Don't schedule in the past — but also don't jump too far ahead
            if target >= current_time - 0.050 {
                target.max(current_time)
            } else {
                // Audio PTS is way before origin — skip this buffer
                return Ok(());
            }
        } else {
            // Normal sequential scheduling (no seek origin)
            if self.next_play_time < current_time {
                self.next_play_time = current_time;
            }
            self.next_play_time
        };

        source.start_with_when(play_at)?;
        self.next_play_time = play_at + duration_s;

        Ok(())
    }

    /// Schedule software-decoded PCM for playback.
    fn schedule_pcm(&mut self, ctx: &AudioContext, pcm: &PcmBuffer) -> Result<(), JsValue> {
        let nch = pcm.channels.len() as u32;
        if nch == 0 || pcm.channels[0].is_empty() {
            return Ok(());
        }
        let num_frames = pcm.channels[0].len() as u32;

        let audio_buffer = ctx.create_buffer(nch, num_frames, pcm.sample_rate)?;

        for ch in 0..nch as usize {
            let mut data = pcm.channels[ch].clone();
            audio_buffer.copy_to_channel(&mut data, ch as i32)?;
        }

        let source: AudioBufferSourceNode = ctx.create_buffer_source()?;
        source.set_buffer(Some(&audio_buffer));
        // Route through gain node for volume control
        if let Some(gain) = &self.gain_node {
            source.connect_with_audio_node(gain)?;
        } else {
            source.connect_with_audio_node(&ctx.destination())?;
        }

        let current_time = ctx.current_time();
        if self.next_play_time < current_time {
            self.next_play_time = current_time;
        }

        source.start_with_when(self.next_play_time)?;
        self.next_play_time += num_frames as f64 / pcm.sample_rate as f64;

        Ok(())
    }

    /// Get the next scheduled play time (AudioContext seconds).
    pub fn next_play_time(&self) -> f64 {
        self.next_play_time
    }

    /// Get the AudioContext's current time in milliseconds.
    pub fn current_time_ms(&self) -> f64 {
        self.audio_ctx
            .as_ref()
            .map(|ctx| ctx.current_time() * 1000.0)
            .unwrap_or(0.0)
    }

    /// Set volume (0.0 = muted, 1.0 = full volume).
    pub fn set_volume(&self, volume: f64) {
        if let Some(gain) = &self.gain_node {
            gain.gain().set_value(volume as f32);
        }
    }

    /// Suspend AudioContext.
    pub async fn suspend(&self) -> Result<(), JsValue> {
        if let Some(ctx) = &self.audio_ctx {
            let promise = ctx.suspend()?;
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(promise)).await?;
        }
        Ok(())
    }

    /// Resume AudioContext.
    pub async fn resume(&self) -> Result<(), JsValue> {
        if let Some(ctx) = &self.audio_ctx {
            let promise = ctx.resume()?;
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(promise)).await?;
        }
        Ok(())
    }

    /// Close just the audio decoder backend, keeping AudioContext + GainNode alive.
    /// Use this during seek to preserve `currentTime` continuity.
    pub fn close_decoder(&mut self) {
        match self.backend.take() {
            Some(AudioBackend::WebCodecs { decoder, data_queue, .. }) => {
                let _ = decoder.close();
                for data in data_queue.borrow_mut().drain(..) {
                    data.close();
                }
            }
            Some(AudioBackend::Software { .. }) => {
                // Nothing to close — pure Rust
            }
            None => {}
        }
    }

    /// Close the audio pipeline and release ALL resources (AudioContext included).
    /// Use this only on full teardown (destroy, load new file).
    pub fn close(&mut self) {
        self.close_decoder();
        self.gain_node.take();
        if let Some(ctx) = self.audio_ctx.take() {
            let _ = ctx.close();
        }
    }

    /// Check if audio is configured.
    pub fn is_configured(&self) -> bool {
        self.backend.is_some()
    }

    pub fn has_error(&self) -> Option<String> {
        match &self.backend {
            Some(AudioBackend::WebCodecs { error, .. }) => error.borrow().clone(),
            _ => None,
        }
    }

    /// Get the number of decoded audio data items waiting in the queue.
    pub fn queue_len(&self) -> usize {
        match &self.backend {
            Some(AudioBackend::WebCodecs { data_queue, .. }) => data_queue.borrow().len(),
            Some(AudioBackend::Software { pcm_queue, .. }) => pcm_queue.len(),
            None => 0,
        }
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.close();
    }
}

/// Downmix multi-channel audio to stereo.
/// Input: deinterleaved channels. Output: 2 channels [left, right].
/// Supports 5.1 (6ch), 5.0 (5ch), quad (4ch), 3.0 (3ch).
fn downmix_to_stereo(channels: &[Vec<f32>], nch: usize) -> Vec<Vec<f32>> {
    let len = channels[0].len();
    let mut left = vec![0.0f32; len];
    let mut right = vec![0.0f32; len];

    match nch {
        6 => {
            // 5.1: L, C, R, SL, SR, LFE (standard AC-3 order for acmod=7 + LFE)
            // Downmix: L' = L + 0.707*C + 0.707*SL, R' = R + 0.707*C + 0.707*SR
            let cmix = 0.707f32;
            let smix = 0.707f32;
            for i in 0..len {
                left[i] = channels[0][i] + cmix * channels[1][i] + smix * channels[3][i];
                right[i] = channels[2][i] + cmix * channels[1][i] + smix * channels[4][i];
            }
        }
        5 => {
            // 5.0: L, C, R, SL, SR
            let cmix = 0.707f32;
            let smix = 0.707f32;
            for i in 0..len {
                left[i] = channels[0][i] + cmix * channels[1][i] + smix * channels[3][i];
                right[i] = channels[2][i] + cmix * channels[1][i] + smix * channels[4][i];
            }
        }
        4 => {
            // Quad: L, R, SL, SR
            let smix = 0.707f32;
            for i in 0..len {
                left[i] = channels[0][i] + smix * channels[2][i];
                right[i] = channels[1][i] + smix * channels[3][i];
            }
        }
        3 => {
            // 3.0: L, C, R
            let cmix = 0.707f32;
            for i in 0..len {
                left[i] = channels[0][i] + cmix * channels[1][i];
                right[i] = channels[2][i] + cmix * channels[1][i];
            }
        }
        1 => {
            // Mono → duplicate to both channels
            left.copy_from_slice(&channels[0]);
            right.copy_from_slice(&channels[0]);
        }
        _ => {
            // Unknown layout — just use first 2 channels
            if nch >= 2 {
                left.copy_from_slice(&channels[0]);
                right.copy_from_slice(&channels[1]);
            } else {
                left.copy_from_slice(&channels[0]);
                right.copy_from_slice(&channels[0]);
            }
        }
    }

    // Normalize to prevent clipping
    let mut peak = 0.0f32;
    for i in 0..len {
        peak = peak.max(left[i].abs()).max(right[i].abs());
    }
    if peak > 1.0 {
        let scale = 1.0 / peak;
        for i in 0..len {
            left[i] *= scale;
            right[i] *= scale;
        }
    }

    vec![left, right]
}
