//! Over-limit request bodies (#116): the same drain as the daemon's
//! `balerix_server::body_limit`, for the plugin's own listener (the SDK
//! depends on `balerix-api` only, so it keeps its own copy).
//!
//! axum refuses a body the moment it has read past the route's
//! `DefaultBodyLimit`, and the connection is then closed with the rest of
//! the body unread: a client that is still writing gets EPIPE or a reset
//! in place of the answer. [`limited`] puts [`drain_over_limit`] in front
//! of a router beside its `DefaultBodyLimit`:
//!
//! - a body that says it fits (its `Content-Length`) goes through untouched;
//! - one that says it is over [`DRAIN_FACTOR`] times the limit is answered
//!   at once, without reading it, and the connection closed;
//! - any other body is read to its end and thrown away, then the route
//!   answers it as over the limit, so the client reads that answer after
//!   its last byte; a body without a length that turns out to fit is
//!   handed on as read, and one that runs past the ceiling is answered
//!   then and the connection closed.
//!
//! "Answered as over the limit" means the route's own handler runs on a
//! stand-in body one byte over the limit: whatever the route answers an
//! over-limit body today — its `{ "error": … }`, and its own order of
//! checks (a bad token is still a 401) — is what the client gets.

use std::future::poll_fn;
use std::pin::Pin;

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::http::header::{CONNECTION, CONTENT_LENGTH, HeaderValue, TRANSFER_ENCODING};
use axum::http::request::Parts;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};

use axum::Json;
use balerix_api::ErrorBody;

/// How far past a route's limit a body is still read to its end.
pub(crate) const DRAIN_FACTOR: usize = 4;

/// `router` with a body limit of `limit` bytes, an over-limit body
/// drained (up to [`DRAIN_FACTOR`] × `limit`) before it is answered.
pub(crate) fn limited<S>(router: Router<S>, limit: usize) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router
        .layer(DefaultBodyLimit::max(limit))
        .layer(middleware::from_fn_with_state(limit, drain_over_limit))
}

pub(crate) async fn drain_over_limit(
    State(limit): State<usize>,
    req: Request,
    next: Next,
) -> Response {
    let hint = req.body().size_hint();
    if hint.upper().is_some_and(|n| n <= limit as u64) {
        return next.run(req).await;
    }
    let ceiling = limit.saturating_mul(DRAIN_FACTOR);
    let (parts, mut body) = req.into_parts();
    if hint.lower() > ceiling as u64 {
        return closing(next.run(over_limit(parts, limit)).await);
    }
    // Only a body of unknown length can still turn out to fit.
    let mut kept = hint.upper().is_none().then(Vec::new);
    let mut read = 0usize;
    while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(e) => {
                let error = format!("Failed to read the request body: {e}");
                return closing(
                    (StatusCode::BAD_REQUEST, Json(ErrorBody { error })).into_response(),
                );
            }
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        read += data.len();
        if read > ceiling {
            return closing(next.run(over_limit(parts, limit)).await);
        }
        if let Some(kept) = kept.as_mut()
            && read <= limit
        {
            kept.extend_from_slice(&data);
        }
    }
    match kept {
        Some(kept) if read <= limit => next.run(Request::from_parts(parts, Body::from(kept))).await,
        _ => next.run(over_limit(parts, limit)).await,
    }
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
fn closing(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    resp
}
