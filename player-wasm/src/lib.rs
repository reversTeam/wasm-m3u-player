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
