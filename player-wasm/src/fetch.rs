use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, RequestMode, ReadableStreamDefaultReader, Response};

/// A streaming HTTP reader — opens a fetch request and reads chunks on demand.
pub struct StreamReader {
    reader: ReadableStreamDefaultReader,
    pub content_length: u64,
}

impl StreamReader {
    /// Open a streaming fetch to the given URL.
    /// Returns immediately after the response headers are received.
    pub async fn open(url: &str) -> Result<Self, JsValue> {
        let opts = RequestInit::new();
        opts.set_method("GET");
        opts.set_mode(RequestMode::Cors);

        let request = Request::new_with_str_and_init(url, &opts)?;

        let window = web_sys::window().ok_or_else(|| JsValue::from_str("No window"))?;
        let resp_value = JsFuture::from(window.fetch_with_request(&request)).await?;
        let resp: Response = resp_value.dyn_into()?;

        if !resp.ok() {
            return Err(JsValue::from_str(&format!(
                "HTTP error: {} {}",
                resp.status(),
                resp.status_text()
            )));
        }

        let content_length: u64 = resp
            .headers()
            .get("content-length")
            .ok()
            .flatten()
            .and_then(|s: String| s.parse().ok())
            .unwrap_or(0);

        let body = resp
            .body()
            .ok_or_else(|| JsValue::from_str("Response has no body"))?;

        let reader = body
            .get_reader()
            .dyn_into::<ReadableStreamDefaultReader>()?;

        Ok(Self {
            reader,
            content_length,
        })
    }

    /// Read the next chunk from the stream.
    /// Returns `Ok(Some(bytes))` for data, `Ok(None)` at EOF.
    pub async fn read_chunk(&self) -> Result<Option<Vec<u8>>, JsValue> {
        let result = JsFuture::from(self.reader.read()).await?;

        let done = js_sys::Reflect::get(&result, &"done".into())?
            .as_bool()
            .unwrap_or(true);

        if done {
            return Ok(None);
        }

        let value = js_sys::Reflect::get(&result, &"value".into())?;
        let chunk = js_sys::Uint8Array::new(&value);
        let mut bytes = vec![0u8; chunk.length() as usize];
        chunk.copy_to(&mut bytes);

        Ok(Some(bytes))
    }
}

/// Sleep for the given number of milliseconds (WASM-compatible).
pub async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let win = web_sys::window().unwrap();
        win.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    let _ = JsFuture::from(promise).await;
}
