use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::{
    AudioBufferSourceNode, AudioContext, AudioData, AudioDataCopyToOptions, AudioDecoder,
    AudioDecoderConfig, AudioDecoderInit, AudioSampleFormat, EncodedAudioChunk,
    EncodedAudioChunkInit, EncodedAudioChunkType,
};

use demuxer::EncodedChunk;

/// Wrapper around WebCodecs AudioDecoder + Web Audio API playback.
pub struct AudioPipeline {
    decoder: Option<AudioDecoder>,
    audio_ctx: Option<AudioContext>,
    /// Queue of decoded AudioData waiting to be played.
    data_queue: Rc<RefCell<VecDeque<AudioData>>>,
    error: Rc<RefCell<Option<String>>>,
    /// Next scheduled playback time in AudioContext seconds.
    next_play_time: f64,
    /// Keep closures alive.
    _output_closure: Option<Closure<dyn FnMut(AudioData)>>,
    _error_closure: Option<Closure<dyn FnMut(JsValue)>>,
}

impl AudioPipeline {
    pub fn new() -> Self {
        Self {
            decoder: None,
            audio_ctx: None,
            data_queue: Rc::new(RefCell::new(VecDeque::new())),
            error: Rc::new(RefCell::new(None)),
            next_play_time: 0.0,
            _output_closure: None,
            _error_closure: None,
        }
    }

    /// Configure the audio decoder and create AudioContext.
    pub fn configure(
        &mut self,
        codec: &str,
        sample_rate: u32,
        channels: u32,
        codec_config: Option<&[u8]>,
    ) -> Result<(), JsValue> {
        // Create AudioContext (must be resumed after user interaction)
        let audio_ctx = AudioContext::new()?;
        self.next_play_time = audio_ctx.current_time();

        let data_queue = self.data_queue.clone();
        let error_state = self.error.clone();

        let output_closure = Closure::wrap(Box::new(move |data: AudioData| {
            data_queue.borrow_mut().push_back(data);
        }) as Box<dyn FnMut(AudioData)>);

        let error_closure = Closure::wrap(Box::new(move |e: JsValue| {
            let msg = e.as_string().unwrap_or_else(|| format!("{:?}", e));
            *error_state.borrow_mut() = Some(msg);
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

        self.decoder = Some(decoder);
        self.audio_ctx = Some(audio_ctx);
        self._output_closure = Some(output_closure);
        self._error_closure = Some(error_closure);

        Ok(())
    }

    /// Decode an encoded audio chunk from the demuxer.
    pub fn decode(&self, chunk: &EncodedChunk) -> Result<(), JsValue> {
        let decoder = self
            .decoder
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Audio decoder not configured"))?;

        if let Some(err) = self.error.borrow().as_ref() {
            return Err(JsValue::from_str(&format!("Audio decoder error: {}", err)));
        }

        let chunk_type = if chunk.is_keyframe {
            EncodedAudioChunkType::Key
        } else {
            EncodedAudioChunkType::Delta
        };

        let data = js_sys::Uint8Array::from(chunk.data.as_slice());
        let init =
            EncodedAudioChunkInit::new(&data.buffer(), chunk.timestamp_us as i32, chunk_type);

        let encoded_chunk = EncodedAudioChunk::new(&init)?;
        decoder.decode(&encoded_chunk)?;

        Ok(())
    }

    /// Schedule decoded audio data for playback via AudioBufferSourceNode.
    /// Should be called regularly (e.g. each rAF) to drain the queue.
    pub fn pump_audio(&mut self) -> Result<(), JsValue> {
        let ctx = match &self.audio_ctx {
            Some(ctx) => ctx,
            None => return Ok(()),
        };

        let mut queue = self.data_queue.borrow_mut();
        while let Some(audio_data) = queue.pop_front() {
            let sample_rate = audio_data.sample_rate() as f32;
            let num_channels = audio_data.number_of_channels();
            let num_frames = audio_data.number_of_frames();

            // Create AudioBuffer and copy each channel from AudioData
            let audio_buffer =
                ctx.create_buffer(num_channels, num_frames, sample_rate)?;

            // Copy each plane (channel) separately using f32-planar format
            for ch in 0..num_channels {
                let js_buffer = js_sys::ArrayBuffer::new(num_frames * 4); // f32 = 4 bytes

                let opts = AudioDataCopyToOptions::new(ch);
                opts.set_format(AudioSampleFormat::F32Planar);

                audio_data.copy_to_with_buffer_source(&js_buffer, &opts)?;

                let float_array = js_sys::Float32Array::new(&js_buffer);
                let mut channel_data = vec![0f32; num_frames as usize];
                float_array.copy_to(&mut channel_data);

                audio_buffer.copy_to_channel(&mut channel_data, ch as i32)?;
            }

            audio_data.close();

            // Schedule playback
            let source: AudioBufferSourceNode = ctx.create_buffer_source()?;
            source.set_buffer(Some(&audio_buffer));
            source.connect_with_audio_node(&ctx.destination())?;

            // Don't schedule in the past
            let current_time = ctx.current_time();
            if self.next_play_time < current_time {
                self.next_play_time = current_time;
            }

            source.start_with_when(self.next_play_time)?;
            self.next_play_time += num_frames as f64 / sample_rate as f64;
        }

        Ok(())
    }

    /// Get the AudioContext's current time in milliseconds.
    pub fn current_time_ms(&self) -> f64 {
        self.audio_ctx
            .as_ref()
            .map(|ctx| ctx.current_time() * 1000.0)
            .unwrap_or(0.0)
    }

    /// Resume AudioContext (must be called after user interaction).
    pub async fn resume(&self) -> Result<(), JsValue> {
        if let Some(ctx) = &self.audio_ctx {
            let promise = ctx.resume()?;
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(promise)).await?;
        }
        Ok(())
    }

    /// Close the audio pipeline and release resources.
    pub fn close(&mut self) {
        if let Some(decoder) = self.decoder.take() {
            let _ = decoder.close();
        }
        if let Some(ctx) = self.audio_ctx.take() {
            let _ = ctx.close();
        }
        for data in self.data_queue.borrow_mut().drain(..) {
            data.close();
        }
    }

    /// Check if audio is configured.
    pub fn is_configured(&self) -> bool {
        self.decoder.is_some()
    }

    pub fn has_error(&self) -> Option<String> {
        self.error.borrow().clone()
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.close();
    }
}
