//! The sandboxed frame renderers run in, `/visualizer-frame`.
//!
//! The page loads it in a frame with `sandbox="allow-scripts"`, and the
//! response says so too, so its origin is opaque. Its policy allows no
//! request at all, only its own inline script, by a nonce chosen for each
//! response, and the workers that script starts from blobs, which inherit
//! the policy. `'unsafe-eval'` lets a worker evaluate a renderer once it
//! has locked itself down (`web/src/visualize/worker.js`).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use serde_json::json;
use std::sync::Arc;

use super::App;
use super::auth::random_hex;

/// The frame's script, which keeps a worker per card.
const FRAME: &str = include_str!("../../web/src/visualize/frame.js");

/// The worker's script, which locks itself down and runs a renderer.
const WORKER: &str = include_str!("../../web/src/visualize/worker.js");

pub async fn frame(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !app.known_host(&headers) {
        return (StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response();
    }

    let Ok(nonce) = random_hex(16) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let csp = format!(
        "sandbox allow-scripts; default-src 'none'; script-src 'nonce-{nonce}' 'unsafe-eval'; \
         worker-src blob:; frame-ancestors 'self'; base-uri 'none'; form-action 'none'"
    );

    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_owned()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            (header::REFERRER_POLICY, "no-referrer".to_owned()),
            (header::CONTENT_SECURITY_POLICY, csp),
        ],
        page(&nonce),
    )
        .into_response()
}

/// The frame's page: its script, with the worker's as a string before it.
fn page(nonce: &str) -> String {
    format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>microscope renderers</title>\n\
         <script nonce=\"{nonce}\">\nconst WORKER_SOURCE = {};\n{FRAME}</script>\n",
        script_string(WORKER)
    )
}

/// `text` as a JavaScript string that cannot end the script it is in.
fn script_string(text: &str) -> String {
    serde_json::to_string(text)
        .unwrap_or_else(|error| json!(format!("the worker did not encode: {error}")).to_string())
        .replace('<', "\\u003c")
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_frames_script_holds_nothing_that_ends_it() {
        let page = super::page("00");
        assert_eq!(page.matches("</script").count(), 1);
        assert!(!super::FRAME.contains('<'), "frame.js is inlined as it is");
    }
}
