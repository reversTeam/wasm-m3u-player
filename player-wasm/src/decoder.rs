use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::{
    EncodedVideoChunk, EncodedVideoChunkInit, EncodedVideoChunkType, VideoDecoder,
    VideoDecoderConfig, VideoDecoderInit, VideoFrame,
};

use demuxer::EncodedChunk;

/// Wrapper around WebCodecs VideoDecoder.
pub struct VideoDecoderWrapper {
    decoder: Option<VideoDecoder>,
    frame_queue: Rc<RefCell<VecDeque<VideoFrame>>>,
    error: Rc<RefCell<Option<String>>>,
    /// Keep closures alive
    _output_closure: Option<Closure<dyn FnMut(VideoFrame)>>,
    _error_closure: Option<Closure<dyn FnMut(JsValue)>>,
}

impl VideoDecoderWrapper {
    pub fn new() -> Self {
        Self {
            decoder: None,
            frame_queue: Rc::new(RefCell::new(VecDeque::new())),
            error: Rc::new(RefCell::new(None)),
            _output_closure: None,
            _error_closure: None,
        }
    }

    /// Configure the decoder with a WebCodecs-compatible codec string.
    pub fn configure(
        &mut self,
        codec: &str,
        width: u32,
        height: u32,
        codec_config: Option<&[u8]>,
    ) -> Result<(), JsValue> {
        let frame_queue = self.frame_queue.clone();
        let error_state = self.error.clone();

        // Output callback: store decoded VideoFrame in queue
        let output_closure = Closure::wrap(Box::new(move |frame: VideoFrame| {
            frame_queue.borrow_mut().push_back(frame);
        }) as Box<dyn FnMut(VideoFrame)>);

        // Error callback
        let error_closure = Closure::wrap(Box::new(move |e: JsValue| {
            let msg = js_sys::Object::try_from(&e)
                .and_then(|obj| {
                    js_sys::Reflect::get(obj, &"message".into())
                        .ok()
                        .map(|v| v.as_string().unwrap_or_default())
                })
                .unwrap_or_else(|| format!("{:?}", e));
            *error_state.borrow_mut() = Some(msg);
        }) as Box<dyn FnMut(JsValue)>);

        let init = VideoDecoderInit::new(
            error_closure.as_ref().unchecked_ref(),
            output_closure.as_ref().unchecked_ref(),
        );

        let decoder = VideoDecoder::new(&init)?;

        let config = VideoDecoderConfig::new(codec);
        config.set_coded_width(width);
        config.set_coded_height(height);

        // Set codec-specific description (e.g. avcC for H264)
        if let Some(config_data) = codec_config {
            if !config_data.is_empty() {
                let buffer = js_sys::Uint8Array::from(config_data);
                config.set_description(&buffer.buffer());
            }
        }

        decoder.configure(&config)?;

        self.decoder = Some(decoder);
        self._output_closure = Some(output_closure);
        self._error_closure = Some(error_closure);

        Ok(())
    }

    /// Decode an encoded chunk from the demuxer.
    pub fn decode(&self, chunk: &EncodedChunk) -> Result<(), JsValue> {
        let decoder = self
            .decoder
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Decoder not configured"))?;

        // Check for pending errors
        if let Some(err) = self.error.borrow().as_ref() {
            return Err(JsValue::from_str(&format!("Decoder error: {}", err)));
        }

        let chunk_type = if chunk.is_keyframe {
            EncodedVideoChunkType::Key
        } else {
            EncodedVideoChunkType::Delta
        };

        let data = js_sys::Uint8Array::from(chunk.data.as_slice());

        let init = EncodedVideoChunkInit::new(&data.buffer(), chunk.timestamp_us as i32, chunk_type);
        if chunk.duration_us > 0 {
            init.set_duration(chunk.duration_us as u32);
        }

        let encoded_chunk = EncodedVideoChunk::new(&init)?;
        decoder.decode(&encoded_chunk)?;

        Ok(())
    }

    /// Take the next decoded frame from the queue.
    pub fn take_frame(&self) -> Option<VideoFrame> {
        self.frame_queue.borrow_mut().pop_front()
    }

    /// Peek at the timestamp (in microseconds) of the next frame without removing it.
    pub fn peek_timestamp_us(&self) -> Option<f64> {
        self.frame_queue
            .borrow()
            .front()
            .map(|f| f.timestamp().unwrap_or(0.0))
    }

    /// Get the number of frames waiting in the queue.
    pub fn queue_len(&self) -> usize {
        self.frame_queue.borrow().len()
    }

    /// Flush the decoder (wait for all pending frames).
    pub async fn flush(&self) -> Result<(), JsValue> {
        if let Some(decoder) = &self.decoder {
            let promise = decoder.flush();
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(promise)).await?;
        }
        Ok(())
    }

    /// Close the decoder and release resources.
    pub fn close(&mut self) {
        if let Some(decoder) = self.decoder.take() {
            let _ = decoder.close();
        }
        // Drop remaining frames
        for frame in self.frame_queue.borrow_mut().drain(..) {
            frame.close();
        }
    }

    /// Check if the decoder has a pending error.
    pub fn has_error(&self) -> Option<String> {
        self.error.borrow().clone()
    }
}

impl Drop for VideoDecoderWrapper {
    fn drop(&mut self) {
        self.close();
    }
}
