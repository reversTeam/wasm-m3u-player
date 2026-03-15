use wasm_bindgen::prelude::*;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, VideoFrame};

/// Canvas 2D renderer for VideoFrame output.
pub struct CanvasRenderer {
    canvas: HtmlCanvasElement,
    context: CanvasRenderingContext2d,
}

impl CanvasRenderer {
    pub fn new(canvas: HtmlCanvasElement) -> Result<Self, JsValue> {
        let context = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("Failed to get 2D context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;

        Ok(Self { canvas, context })
    }

    /// Resize the canvas to match video dimensions.
    pub fn resize(&self, width: u32, height: u32) {
        self.canvas.set_width(width);
        self.canvas.set_height(height);
    }

    /// Render a single VideoFrame to the canvas, then close the frame.
    pub fn render_frame(&self, frame: &VideoFrame) -> Result<(), JsValue> {
        // Resize canvas to video dimensions if needed
        let fw = frame.display_width();
        let fh = frame.display_height();
        if self.canvas.width() != fw || self.canvas.height() != fh {
            self.resize(fw, fh);
        }

        // drawImage with VideoFrame — zero-copy GPU transfer
        self.context
            .draw_image_with_video_frame(frame, 0.0, 0.0)?;

        Ok(())
    }

    /// Clear the canvas.
    pub fn clear(&self) {
        self.context.clear_rect(
            0.0,
            0.0,
            self.canvas.width() as f64,
            self.canvas.height() as f64,
        );
    }

    /// Get a reference to the canvas element.
    pub fn canvas(&self) -> &HtmlCanvasElement {
        &self.canvas
    }

    /// Get the current canvas dimensions.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.canvas.width(), self.canvas.height())
    }
}
