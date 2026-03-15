use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, RequestMode, ReadableStreamDefaultReader, Response};

/// Information from an HTTP HEAD request.
pub struct HeadInfo {
    pub content_length: u64,
    pub supports_range: bool,
}

/// A streaming HTTP reader — opens a fetch request and reads chunks on demand.
pub struct StreamReader {
    reader: ReadableStreamDefaultReader,
    pub content_length: u64,
    pub supports_range: bool,
}

/// Send a fetch request and return the response (shared helper).
async fn do_fetch(url: &str, method: &str, range_header: Option<&str>) -> Result<Response, JsValue> {
    let opts = RequestInit::new();
    opts.set_method(method);
    opts.set_mode(RequestMode::Cors);

    let request = Request::new_with_str_and_init(url, &opts)?;

    if let Some(range) = range_header {
        request.headers().set("Range", range)?;
    }

    let window = web_sys::window().ok_or_else(|| JsValue::from_str("No window"))?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request)).await?;
    let resp: Response = resp_value.dyn_into()?;

    // Accept 200, 206 (Partial Content), and 204 for HEAD
    let status = resp.status();
    if status >= 400 {
        return Err(JsValue::from_str(&format!(
            "HTTP error: {} {}",
            status,
            resp.status_text()
        )));
    }

    Ok(resp)
}

/// Parse content-length and range support from a response.
fn parse_head_info(resp: &Response) -> HeadInfo {
    let content_length: u64 = resp
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|s: String| s.parse().ok())
        .unwrap_or(0);

    let supports_range = resp
        .headers()
        .get("accept-ranges")
        .ok()
        .flatten()
        .map(|v| v.to_lowercase().contains("bytes"))
        .unwrap_or(false);

    HeadInfo {
        content_length,
        supports_range,
    }
}

impl StreamReader {
    /// Perform a HEAD request to get file size and range support info.
    pub async fn head(url: &str) -> Result<HeadInfo, JsValue> {
        let resp = do_fetch(url, "HEAD", None).await?;
        Ok(parse_head_info(&resp))
    }

    /// Fetch a specific byte range from the URL.
    /// Returns the bytes and whether the server actually returned partial content (206).
    /// If the server ignores Range and returns 200, the full body is returned.
    pub async fn fetch_range(url: &str, start: u64, end: u64) -> Result<Vec<u8>, JsValue> {
        let range = format!("bytes={}-{}", start, end);
        let resp = do_fetch(url, "GET", Some(&range)).await?;

        // Read the full body
        let array_buffer = JsFuture::from(
            resp.array_buffer()
                .map_err(|e| JsValue::from_str(&format!("arrayBuffer() failed: {:?}", e)))?,
        )
        .await?;
        let uint8 = js_sys::Uint8Array::new(&array_buffer);
        let mut bytes = vec![0u8; uint8.length() as usize];
        uint8.copy_to(&mut bytes);

        Ok(bytes)
    }

    /// Open a streaming fetch to the given URL.
    /// Returns immediately after the response headers are received.
    pub async fn open(url: &str) -> Result<Self, JsValue> {
        let resp = do_fetch(url, "GET", None).await?;
        let info = parse_head_info(&resp);

        let body = resp
            .body()
            .ok_or_else(|| JsValue::from_str("Response has no body"))?;

        let reader = body
            .get_reader()
            .dyn_into::<ReadableStreamDefaultReader>()?;

        Ok(Self {
            reader,
            content_length: info.content_length,
            supports_range: info.supports_range,
        })
    }

    /// Open a streaming fetch starting at a byte offset (Range request).
    /// Falls back to a full GET if the server doesn't support ranges.
    pub async fn open_range(url: &str, start: u64) -> Result<Self, JsValue> {
        let range = format!("bytes={}-", start);
        let resp = do_fetch(url, "GET", Some(&range)).await?;
        let info = parse_head_info(&resp);

        // If server returned 200 instead of 206, it ignored the Range header
        let actual_supports_range = resp.status() == 206;

        let body = resp
            .body()
            .ok_or_else(|| JsValue::from_str("Response has no body"))?;

        let reader = body
            .get_reader()
            .dyn_into::<ReadableStreamDefaultReader>()?;

        Ok(Self {
            reader,
            content_length: info.content_length,
            supports_range: actual_supports_range || info.supports_range,
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
