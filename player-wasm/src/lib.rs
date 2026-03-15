use wasm_bindgen::prelude::*;

pub mod decoder;
pub mod renderer;
pub mod fetch;

/// Initialize the WASM module (sets up panic hook for better error messages).
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
}
