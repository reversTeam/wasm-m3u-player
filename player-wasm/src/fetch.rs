use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, RequestMode, Response};

/// Progress callback: (bytes_received, total_bytes_or_0)
pub type ProgressCallback = Box<dyn Fn(u64, u64)>;

/// Fetch a URL and return the full body as bytes (no progress).
pub async fn fetch_bytes(url: &str) -> Result<Vec<u8>, JsValue> {
    fetch_bytes_with_progress(url, None).await
}

/// Fetch a URL with streaming download + progress reporting.
///
/// Reads the response body via ReadableStream chunk-by-chunk,
/// calling `on_progress(bytes_received, content_length)` after each chunk.
/// `content_length` is 0 if the server didn't send Content-Length.
pub async fn fetch_bytes_with_progress(
    url: &str,
    on_progress: Option<ProgressCallback>,
) -> Result<Vec<u8>, JsValue> {
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

    // Try to get Content-Length for progress reporting
    let content_length: u64 = resp
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|s: String| s.parse().ok())
        .unwrap_or(0);

    // Get ReadableStream body
    let body = resp
        .body()
        .ok_or_else(|| JsValue::from_str("Response has no body"))?;

    let reader = body
        .get_reader()
        .dyn_into::<web_sys::ReadableStreamDefaultReader>()?;

    let mut buffer = if content_length > 0 {
        Vec::with_capacity(content_length as usize)
    } else {
        Vec::new()
    };

    let mut bytes_received: u64 = 0;

    loop {
        let result = JsFuture::from(reader.read()).await?;

        let done = js_sys::Reflect::get(&result, &"done".into())?
            .as_bool()
            .unwrap_or(true);

        if done {
            break;
        }

        let value = js_sys::Reflect::get(&result, &"value".into())?;
        let chunk = js_sys::Uint8Array::new(&value);
        let chunk_len = chunk.length() as usize;

        let offset = buffer.len();
        buffer.resize(offset + chunk_len, 0);
        chunk.copy_to(&mut buffer[offset..]);

        bytes_received += chunk_len as u64;

        if let Some(ref cb) = on_progress {
            cb(bytes_received, content_length);
        }
    }

    Ok(buffer)
}
