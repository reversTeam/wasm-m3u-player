// WASM bindings produce many patterns that trigger clippy (JS interop casts,
// RefCell across await in single-threaded WASM, format! in format!, etc.)
#![allow(dead_code)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::new_without_default)]
#![allow(clippy::manual_map)]
#![allow(clippy::while_let_loop)]
#![allow(clippy::format_in_format_args)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::await_holding_refcell_ref)]
#![allow(clippy::unnecessary_to_owned)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::double_ended_iterator_last)]
#![allow(clippy::missing_const_for_thread_local)]
#![allow(clippy::empty_line_after_doc_comments)]
#![allow(clippy::doc_lazy_continuation)]

use wasm_bindgen::prelude::*;

pub mod audio;
pub mod buffer;
pub mod decoder;
pub mod fetch;
pub mod player;
pub mod renderer;
pub mod sync;

/// Initialize the WASM module (sets up panic hook for better error messages).
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
}
