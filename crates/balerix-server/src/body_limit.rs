//! Over-limit request bodies (#116). The SDK
//! (`balerix-plugin-sdk/src/body_limit.rs`) and the agent sidecar
//! (`agent/src/body_limit.rs`) keep copies; change them together.
//!
//! axum refuses a body the moment it has read past the route's
//! `DefaultBodyLimit`, and the connection is then closed with the rest of
//! the body unread: a client that is still writing gets EPIPE or a reset
//! in place of the answer. [`drain_over_limit`] sits in front of a limited
//! route, beside its `DefaultBodyLimit` and *inside* its authentication
//! (a caller that fails it is answered before a byte of the body is read):
//!
//! - a body that says it fits (its `Content-Length`) goes through untouched;
//! - one that says it is over [`DRAIN_FACTOR`] times the limit is answered
//!   at once, without reading it, and the connection closed;
//! - any other body is read to its end and thrown away, then the route
//!   answers it as over the limit, so the client reads that answer after
//!   its last byte; a body without a length that turns out to fit is
//!   handed on as read, and one that runs past the ceiling, or is still
//!   arriving after [`DRAIN_TIME`], is answered then and the connection
//!   closed.
//!
//! "Answered as over the limit" means the route's own handler runs on a
//! stand-in body one byte over the limit: whatever the route answers an
//! over-limit body today — its `{ "error": … }` and its status — is what
//! the client gets.

use std::future::poll_fn;
use std::pin::Pin;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::http::header::{CONNECTION, CONTENT_LENGTH, HeaderValue, TRANSFER_ENCODING};
use axum::http::request::Parts;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};

use crate::api::ApiError;

/// How far past a route's limit a body is still read to its end.
pub const DRAIN_FACTOR: usize = 4;
/// How long an over-limit body may take to arrive before it is answered
/// unread: a slow trickle does not hold the connection.
pub const DRAIN_TIME: Duration = Duration::from_secs(10);

/// A route's body limit and how long its drain may take.
#[derive(Debug, Clone, Copy)]
pub struct Drain {
    pub limit: usize,
    pub time: Duration,
}

impl Drain {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            time: DRAIN_TIME,
        }
    }
}

/// `router` with a body limit and the drain. Authentication goes on
/// *after* this (a `route_layer`, which wraps outside it), so it answers
/// before the body is read.
pub fn limited<S>(router: Router<S>, drain: Drain) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router
        .layer(DefaultBodyLimit::max(drain.limit))
        .layer(middleware::from_fn_with_state(drain, drain_over_limit))
}

/// What [`read`] made of a body.
pub(crate) enum Read {
    Fits(Vec<u8>),
    Over,
    PastCeiling,
    Broken(String),
}

pub async fn drain_over_limit(State(drain): State<Drain>, req: Request, next: Next) -> Response {
    let limit = drain.limit;
    let hint = req.body().size_hint();
    if hint.upper().is_some_and(|n| n <= limit as u64) {
        return next.run(req).await;
    }
    let ceiling = limit.saturating_mul(DRAIN_FACTOR);
    let (parts, body) = req.into_parts();
    if hint.lower() > ceiling as u64 {
        return closing(next.run(over_limit(parts, limit)).await);
    }
    // Only a body of unknown length can still turn out to fit.
    let keep = hint.upper().is_none();
    match tokio::time::timeout(drain.time, read(body, limit, ceiling, keep)).await {
        Ok(Read::Fits(kept)) => next.run(Request::from_parts(parts, Body::from(kept))).await,
        Ok(Read::Over) => next.run(over_limit(parts, limit)).await,
        Ok(Read::PastCeiling) | Err(_) => closing(next.run(over_limit(parts, limit)).await),
        Ok(Read::Broken(e)) => closing(
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("Failed to read the request body: {e}"),
            )
            .into_response(),
        ),
    }
}

/// Reads `body` to its end, keeping it (when `keep`) while it fits
/// `limit` and stopping once past `ceiling`. The proxy reads its bodies
/// with this too (#168).
pub(crate) async fn read(mut body: Body, limit: usize, ceiling: usize, keep: bool) -> Read {
    let mut kept = keep.then(Vec::new);
    let mut read = 0usize;
    while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(e) => return Read::Broken(e.to_string()),
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        read += data.len();
        if read > ceiling {
            return Read::PastCeiling;
        }
        if let Some(kept) = kept.as_mut()
            && read <= limit
        {
            kept.extend_from_slice(&data);
        }
    }
    match kept {
        Some(kept) if read <= limit => Read::Fits(kept),
        _ => Read::Over,
    }
}

/// The most of an unauthenticated request's body that is read before its
/// refusal: enough for any honest small request, whose keep-alive
/// connection then survives the 401.
pub const REFUSAL_READ: usize = 64 << 10;

/// `resp`, the refusal of a caller that failed authentication, without
/// reading more than [`REFUSAL_READ`] of its body (#116): a body that says
/// it is no longer is read and dropped (within [`DRAIN_TIME`]) so the
/// connection stays usable; any other is left unread and the connection
/// closed.
pub async fn refuse(req: Request, resp: Response) -> Response {
    let hint = req.body().size_hint();
    if hint.upper().is_some_and(|n| n <= REFUSAL_READ as u64) {
        let body = req.into_body();
        if let Ok(Read::Fits(_) | Read::Over) =
            tokio::time::timeout(DRAIN_TIME, read(body, 0, REFUSAL_READ, false)).await
        {
            return resp;
        }
    }
    closing(resp)
}

/// The request as the route would have seen it, with a body one byte over
/// `limit`: its extractor refuses it the way it refuses the real one.
fn over_limit(mut parts: Parts, limit: usize) -> Request {
    let len = limit + 1;
    parts.headers.remove(TRANSFER_ENCODING);
    parts.headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
    Request::from_parts(parts, Body::from(Bytes::from(vec![b' '; len])))
}

/// The rest of the body is not read: the connection cannot be reused.
pub(crate) fn closing(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use std::io::{Read as _, Write};

    /// A body that trickles in slower than the drain allows is answered
    /// when the time is up, as over the limit, and the connection closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_trickle_is_answered_when_the_drain_time_is_up() {
        let app = limited(
            Router::new().route("/", post(|_b: Bytes| async { "fits" })),
            Drain {
                limit: 8,
                time: Duration::from_millis(300),
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let answer = tokio::task::spawn_blocking(move || {
            let mut s = std::net::TcpStream::connect(addr).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            // 16 bytes declared (over 8, under the 32 ceiling), 2 sent
            write!(
                s,
                "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 16\r\n\r\nab"
            )
            .unwrap();
            let started = std::time::Instant::now();
            let mut answer = String::new();
            let _ = s.read_to_string(&mut answer);
            (started.elapsed(), answer)
        })
        .await
        .unwrap();
        let (took, text) = answer;
        assert!(text.starts_with("HTTP/1.1 413"), "{text}");
        assert!(
            text.to_ascii_lowercase().contains("connection: close"),
            "{text}"
        );
        assert!(
            took >= Duration::from_millis(250) && took < Duration::from_secs(4),
            "{took:?}"
        );
    }
}
