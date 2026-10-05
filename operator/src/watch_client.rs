//! The kube `Client` the controllers' watches run on.
//!
//! hyper 1.11.1's HTTP/1 client can leave a connection's "want" signal set
//! while a request is in flight (hyperium/hyper#4207, balerix#129), so
//! hyper-util's pool takes a connection back while a watch's body still
//! streams on it. The next request checked out onto it is not written until
//! the watch ends, up to 290 s; when that request is itself a watch, its
//! controller gets no events at all. The watches therefore get a client whose
//! pool keeps no idle connection: every request has a connection of its own,
//! and nothing is ever queued behind a stream. Reconciles keep the pooled
//! client. Remove this, and run the watches on `Client::try_from(config)`, once a
//! hyper with the fix (hyperium/hyper#4208) ships; the reconciles keep
//! `request_client`.

use hyper::body::Incoming;
use hyper::http::header::HeaderMap;
use hyper::http::{Request, Response};
use hyper_timeout::TimeoutConnector;
use hyper_util::client::legacy::Builder;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use kube::client::retry::RetryPolicy;
use kube::client::{Body, ConfigExt};
use kube::{Client, Config};
use std::time::Duration;
use tower::retry::RetryLayer;
use tower::{BoxError, ServiceBuilder};
use tower_http::classify::ServerErrorsFailureClass;
use tower_http::trace::TraceLayer;
use tracing::Span;

/// A `Client` for watches only: kube 4.2's own stack (`ClientBuilder`'s
/// `TryFrom<Config>` with the operator's features: rustls, no proxy, no
/// gzip) over a hyper-util client that pools nothing, with kube's `HTTP`
/// trace span and failure logs. Unlike kube's, it carries no `valid_until`
/// (an exec plugin's client certificate, which the operator does not use).
pub fn watch_client(config: Config) -> kube::Result<Client> {
    if let Some(proxy_url) = config.proxy_url.clone() {
        // the operator's kube has neither proxy feature: the pooled client
        // refuses it with these same errors
        return Err(match proxy_url.scheme_str() {
            Some("socks5") => kube::Error::ProxyProtocolDisabled {
                proxy_url,
                protocol_feature: "kube/socks5",
            },
            Some("http" | "https") => kube::Error::ProxyProtocolDisabled {
                proxy_url,
                protocol_feature: "kube/http-proxy",
            },
            _ => kube::Error::ProxyProtocolUnsupported { proxy_url },
        });
    }
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    let mut connector = TimeoutConnector::new(config.rustls_https_connector_with_connector(http)?);
    connector.set_connect_timeout(config.connect_timeout);
    connector.set_read_timeout(config.read_timeout);
    connector.set_write_timeout(config.write_timeout);
    let hyper = unpooled().build::<_, Body>(connector);
    let service = ServiceBuilder::new()
        .layer(config.base_uri_layer())
        .option_layer(
            config
                .default_retry
                .then(|| RetryLayer::new(RetryPolicy::server_retry())),
        )
        .option_layer(config.auth_layer()?)
        .layer(config.extra_headers_layer()?)
        // kube's own span and logs, copied from `make_generic_builder`
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &Request<Body>| {
                    tracing::debug_span!(
                        "HTTP",
                        http.method = %req.method(),
                        http.url = %req.uri(),
                        http.status_code = tracing::field::Empty,
                        otel.name = req.extensions().get::<&'static str>().unwrap_or(&"HTTP"),
                        otel.kind = "client",
                        otel.status_code = tracing::field::Empty,
                    )
                })
                .on_request(|_req: &Request<Body>, _span: &Span| {
                    tracing::debug!("requesting");
                })
                .on_response(
                    |res: &Response<Incoming>, _latency: Duration, span: &Span| {
                        let status = res.status();
                        span.record("http.status_code", status.as_u16());
                        if status.is_client_error() || status.is_server_error() {
                            span.record("otel.status_code", "ERROR");
                        }
                    },
                )
                .on_body_chunk(())
                .on_eos(|_: Option<&HeaderMap>, _duration: Duration, _span: &Span| {
                    tracing::debug!("stream closed");
                })
                .on_failure(
                    |ec: ServerErrorsFailureClass, _latency: Duration, span: &Span| {
                        span.record("otel.status_code", "ERROR");
                        match ec {
                            ServerErrorsFailureClass::StatusCode(status) => {
                                span.record("http.status_code", status.as_u16());
                                tracing::error!("failed with status {}", status)
                            }
                            ServerErrorsFailureClass::Error(err) => {
                                tracing::error!("failed with error {}", err)
                            }
                        }
                    },
                ),
        )
        .map_err(BoxError::from)
        .service(hyper);
    Ok(Client::new(service, config.default_namespace))
}

/// hyper-util's client builder with no idle connection kept per host.
fn unpooled() -> Builder {
    let mut builder = Builder::new(TokioExecutor::new());
    builder.pool_max_idle_per_host(0);
    builder
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A keep-alive HTTP/1 server answering `{}`; the requests each
    /// accepted connection carried.
    async fn server() -> (String, Arc<Mutex<Vec<usize>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let served = Arc::new(Mutex::new(Vec::new()));
        let record = served.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let index = {
                    let mut served = record.lock().unwrap();
                    served.push(0);
                    served.len() - 1
                };
                let record = record.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let Ok(n) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        // a GET has no body: each head is one request
                        while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            buf.drain(..end + 4);
                            record.lock().unwrap()[index] += 1;
                            let answer = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}";
                            if socket.write_all(answer.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        (url, served)
    }

    async fn three_gets(client: &Client) {
        for _ in 0..3 {
            let get = http::Request::get("/x").body(Vec::new()).unwrap();
            assert_eq!(client.request_text(get).await.unwrap(), "{}");
            // time for the pooled client to take the connection back
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn the_watch_client_never_reuses_a_connection() {
        let (url, served) = server().await;
        let client = watch_client(Config::new(url.parse().unwrap())).unwrap();
        three_gets(&client).await;
        assert_eq!(*served.lock().unwrap(), vec![1, 1, 1]);
    }

    #[tokio::test]
    async fn the_pooled_client_reuses_one_against_the_same_server() {
        // the control: the server keeps a connection alive for a pool
        let (url, served) = server().await;
        let client = Client::try_from(Config::new(url.parse().unwrap())).unwrap();
        three_gets(&client).await;
        assert_eq!(*served.lock().unwrap(), vec![3]);
    }
}
