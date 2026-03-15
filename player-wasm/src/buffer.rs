use std::cell::RefCell;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

/// Configuration for streaming buffer management.
/// Exposed to JS so consumers can tune buffering behavior.
#[wasm_bindgen]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferConfig {
    /// Maximum chunks to feed to decoders per render_tick. Default: 8
    pub decode_batch_size: usize,
    /// Maximum download rate in bytes/sec. 0 = unlimited. Default: 0
    pub max_download_rate: u64,
    /// Pause download when decoded video frame queue exceeds this. Default: 120
    pub max_video_queue: usize,
    /// Resume download when video frame queue drops below this. Default: 30
    pub resume_video_queue: usize,
    /// Minimum demuxed chunk queue size before trying to demux more. Default: 24
    pub min_chunk_queue: usize,
    /// Number of chunks to demux in one batch. Default: 32
    pub demux_batch_size: usize,
}

#[wasm_bindgen]
impl BufferConfig {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            decode_batch_size: 8,
            max_download_rate: 0,
            max_video_queue: 120,
            resume_video_queue: 30,
            min_chunk_queue: 24,
            demux_batch_size: 32,
        }
    }
}

/// Shared download state — written by background download task, read by player.
pub struct SharedDownload {
    pub data: Vec<u8>,
    pub content_length: u64,
    pub complete: bool,
    pub error: Option<String>,
    /// Back-pressure flag: signals download task to pause.
    pub paused: bool,
}

impl SharedDownload {
    pub fn new() -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            data: Vec::new(),
            content_length: 0,
            complete: false,
            error: None,
            paused: false,
        }))
    }
}
