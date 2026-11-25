//! The HTTP edge: a request's head, status, timing and end.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use respan_gateway_core::Extensions;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http_body::{self, Frame, SizeHint};

use super::log::{Entry, RequestLog, Tracked};

/// How much of an error body is read for its message.
const ERROR_BODY: usize = 4096;

/// Logs each request through: its start now, its end once its body ends.
pub async fn track(State(log): State<Arc<RequestLog>>, mut req: Request, next: Next) -> Response {
    let entry = log.start(&req);
    let mut extensions = req
        .extensions_mut()
        .remove::<Extensions>()
        .unwrap_or_default();
    extensions.insert(Tracked(entry.clone()));
    req.extensions_mut().insert(extensions);
    let mut headless = Headless(Some((log.clone(), entry.clone())));
    let mut resp = next.run(req).await;
    headless.0 = None;
    let status = resp.status();
    let session = {
        let mut record = entry.record();
        record.status = Some(status.as_u16());
        if record.client.is_some() && record.method == "POST" && status.is_success() {
            let receipt = format!("{}~{}", record.id, record.started_at);
            resp.headers_mut().insert(
                "x-respan-usage-receipt",
                receipt.parse().expect("gateway receipt"),
            );
        }
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        record.stream = status.as_u16() == 101
            || content_type.is_some_and(|t| t.starts_with("text/event-stream"));
        // A session's `101` only starts it: the plugin's end hook ends it.
        let recorded = entry.recorded.load(Ordering::SeqCst);
        let live = status.as_u16() == 101 && recorded && !entry.ended.load(Ordering::SeqCst);
        if live {
            record.ttfb_ms = Some(entry.elapsed_ms());
        }
        live
    };
    let (head, body) = resp.into_parts();
    if session {
        return Response::from_parts(head, body);
    }
    if http_body::Body::is_end_stream(&body) {
        entry.record().ttfb_ms = Some(entry.elapsed_ms());
        log.finish(&entry, None);
        return Response::from_parts(head, body);
    }
    let error_body = status.as_u16() >= 400;
    let body = TrackedBody {
        inner: body,
        entry,
        log,
        error_body: error_body.then(Vec::new),
    };
    Response::from_parts(head, Body::new(body))
}

/// Finishes its entry if dropped while armed: the request was dropped
/// before its response head, as when the client left or a caller's timeout
/// cut it.
struct Headless(Option<(Arc<RequestLog>, Arc<Entry>)>);

impl Drop for Headless {
    fn drop(&mut self) {
        if let Some((log, entry)) = self.0.take() {
            let left = "the client left before the response began";
            log.finish(&entry, Some(left.to_owned()));
        }
    }
}

/// A response body that tells the log when its first bytes go and when it
/// ends.
struct TrackedBody {
    inner: Body,
    entry: Arc<Entry>,
    log: Arc<RequestLog>,
    /// What came of an error's body, for its message.
    error_body: Option<Vec<u8>>,
}

impl TrackedBody {
    fn end(&mut self, error: Option<String>) {
        let said = self.error_body.as_deref().map(error_message);
        let error = error.or(said.filter(|message| !message.is_empty()));
        self.log.finish(&self.entry, error);
    }
}

impl http_body::Body for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let mut record = this.entry.record();
                    if record.ttfb_ms.is_none() {
                        record.ttfb_ms = Some(this.entry.elapsed_ms());
                    }
                    drop(record);
                    if let Some(kept) = &mut this.error_body {
                        let room = ERROR_BODY.saturating_sub(kept.len());
                        kept.extend_from_slice(&data[..data.len().min(room)]);
                    }
                }
                if this.inner.is_end_stream() {
                    this.end(None);
                }
            }
            Poll::Ready(Some(Err(e))) => this.end(Some(e.to_string())),
            Poll::Ready(None) => this.end(None),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for TrackedBody {
    fn drop(&mut self) {
        self.end(Some("the client left before the response ended".to_owned()));
    }
}

/// The message of an error body: its JSON `error.message` or `message`, or
/// its text.
fn error_message(body: &[u8]) -> String {
    let json: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let said = json.as_ref().and_then(|v| {
        let error = &v["error"];
        error["message"]
            .as_str()
            .or(error.as_str())
            .or(v["message"].as_str())
    });
    let text = match said {
        Some(message) => message.to_owned(),
        None => String::from_utf8_lossy(body).trim().to_owned(),
    };
    match text.char_indices().nth(300) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_message_is_read_from_its_body() {
        assert_eq!(
            error_message(br#"{"error":{"message":"bad key","type":"x"}}"#),
            "bad key"
        );
        assert_eq!(error_message(br#"{"error":"nope"}"#), "nope");
        assert_eq!(
            error_message(b" no format served accepts GET /x \n"),
            "no format served accepts GET /x"
        );
    }
}

/// A named client can read only its own finished request's metering fields.
pub async fn receipt(
    State(log): State<Arc<RequestLog>>,
    axum::extract::Path(token): axum::extract::Path<String>,
    req: Request,
) -> Response {
    use axum::response::IntoResponse;
    use respan_gateway_http::http::StatusCode;
    let client = req
        .extensions()
        .get::<Extensions>()
        .and_then(|e| e.get::<crate::auth::KeyName>());
    let Some((id, started)) = token.split_once('~') else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(r) = log.get(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if client.is_none()
        || r.client.as_deref() != client.map(|c| c.0.as_str())
        || r.started_at != started
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    if r.duration_ms.is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    (
        [(axum::http::header::CACHE_CONTROL, "private, no-store")],
        axum::Json(serde_json::json!({
            "receipt":token, "provider":r.provider, "model":r.model, "rate_cards":r.rate_cards,
            "cost_nano_usd":r.cost_usd.map(|usd| (usd * 1e9).round() as u64)
        })),
    )
        .into_response()
}
