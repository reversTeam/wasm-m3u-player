use wasm_bindgen::prelude::*;

/// Initialize the WASM module (sets up panic hook for better error messages).
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
}
