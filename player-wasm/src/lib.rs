use wasm_bindgen::prelude::*;

pub mod buffer;
pub mod decoder;
pub mod renderer;
pub mod fetch;
pub mod audio;
pub mod sync;
pub mod player;

/// Initialize the WASM module (sets up panic hook for better error messages).
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
}
