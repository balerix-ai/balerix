//! The reverse-proxied plugin mount (plugins spec §6, §18.1, §18.2):
//! `/v1/plugins/<name>/…` → `http://<listen>/v1/routes/…` on hyper's
//! legacy client. Bodies in are capped, bodies out stream, the daemon's
//! own credentials and the hop-by-hop headers never cross, and a 101 is
//! upgraded on both sides and copied byte for byte — the proxy never
//! parses a WebSocket frame.

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};

use crate::api::ApiError;
use crate::plugins::PluginAddr;
use crate::sessions::MOUNT_PREFIX;

pub type HttpClient = Client<HttpConnector, Body>;

/// Proxied request bodies are capped like every other body.
pub const MAX_BODY: usize = 1 << 20;
/// Tells a plugin where it is mounted, so it can build links.
pub const FORWARDED_PREFIX: &str = "x-balerix-forwarded-prefix";

/// Never forwarded on a request: the daemon's own credentials, the host
/// (hyper sets it from the upstream URI), and the hop-by-hop set of RFC
/// 9110 §7.6.1 — `connection` and `upgrade` excepted on an upgrade
/// request, which is exactly what they are for. Whatever `Connection:`
/// itself names is hop-by-hop too (`connection_named`).
pub(crate) const DROPPED: [&str; 9] = [
    "authorization",
    "cookie",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Never forwarded on a response; `connection`/`upgrade` stay on a 101.
const DROPPED_RESPONSE: [&str; 6] = [
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Negligible on loopback; bounds a plugin whose listener has gone away
/// without closing its port.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn client() -> HttpClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
    Client::builder(TokioExecutor::new()).build(connector)
}

/// An upgrade request is `Upgrade` together with a `Connection` that
/// lists `upgrade` (RFC 9110 §7.8); either alone is a stray header.
pub(crate) fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
        && connection_named(headers).any(|name| name.eq_ignore_ascii_case("upgrade"))
}

/// The field names a `Connection:` header lists, every occurrence,
/// lowercased as header names compare.
fn connection_named(headers: &HeaderMap) -> impl Iterator<Item = String> + '_ {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
}

fn filtered(src: &HeaderMap, dropped: &[&str], keep_upgrade: bool) -> HeaderMap {
    let named: Vec<String> = connection_named(src).collect();
    let mut out = HeaderMap::new();
    for (k, v) in src {
        let name = k.as_str();
        if dropped.contains(&name) {
            continue;
        }
        if name == "connection" {
            // On an upgrade the value is rewritten to exactly `Upgrade`:
            // the other names it carried were stripped just above and a
            // `Connection` naming absent headers would be a lie.
            if keep_upgrade && !out.contains_key(header::CONNECTION) {
                out.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
            }
            continue;
        }
        if name == "upgrade" {
            if keep_upgrade {
                out.append(k.clone(), v.clone());
            }
            continue;
        }
        if named.iter().any(|n| n == name) {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    out
}

pub fn forwarded_headers(src: &HeaderMap) -> HeaderMap {
    filtered(src, &DROPPED, is_upgrade(src))
}

pub fn response_headers(src: &HeaderMap, upgraded: bool) -> HeaderMap {
    filtered(src, &DROPPED_RESPONSE, upgraded)
}

/// Why a request body could not be read whole.
#[derive(Debug)]
pub(crate) enum BodyError {
    /// More than `MAX_BODY` bytes: the client's fault, 413.
    TooLarge,
    /// The connection failed under the body: nobody's request to
    /// forward, 400.
    Transport(String),
}

/// The whole body, capped at `MAX_BODY`. Read frame by frame rather than
/// through `axum::body::to_bytes` so the two ways it can fail stay apart:
/// that helper folds a client that disconnected mid-body into the same
/// error as one that sent too much.
pub(crate) async fn read_body(body: Body) -> Result<Bytes, BodyError> {
    use hyper::body::Body as _;
    let mut body = std::pin::pin!(body);
    let mut out = Vec::new();
    while let Some(frame) = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await {
        let frame = frame.map_err(|e| BodyError::Transport(e.to_string()))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if out.len() + data.len() > MAX_BODY {
            return Err(BodyError::TooLarge);
        }
        out.extend_from_slice(&data);
    }
    Ok(Bytes::from(out))
}

/// The `<rest>` of `/v1/plugins/<name>/<rest>`, taken from the request's
/// raw path and left percent-encoded. axum's `Path` extractor decodes
/// what it captures, and a decoded segment spliced back into a URI means
/// something else: `%3F` would start a query, `%2F` would split a
/// segment, `%20` would not parse at all. The mount therefore takes the
/// path apart itself and forwards the bytes the client actually sent.
pub fn forwarded_rest(path: &str) -> &str {
    path.strip_prefix(MOUNT_PREFIX)
        .and_then(|s| s.split_once('/'))
        .map_or("", |(_name, rest)| rest)
}

/// A segment that means `.` or `..`, its `%2e` spellings included — RFC
/// 3986 normalization decodes those before it removes dot segments, so a
/// plugin that normalizes would resolve them out of `/v1/routes` while
/// holding its own bearer.
fn dot_segment(segment: &str) -> bool {
    matches!(
        segment.to_ascii_lowercase().replace("%2e", ".").as_str(),
        "." | ".."
    )
}

/// `http://<listen>/v1/routes` for an empty `rest` (axum answers a nested
/// router's `/` there, not at `/v1/routes/`), else `/v1/routes/<rest>`
/// with `rest` as the client encoded it, and the query string as it came.
/// A `.` or `..` segment is refused rather than forwarded.
pub fn upstream_uri(listen: &str, rest: &str, query: Option<&str>) -> Result<Uri, String> {
    if rest.split('/').any(dot_segment) {
        return Err("path: . and .. segments are not forwarded".to_string());
    }
    let path = if rest.is_empty() {
        "/v1/routes".to_string()
    } else {
        format!("/v1/routes/{rest}")
    };
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    format!("http://{listen}{path}{query}")
        .parse()
        .map_err(|e: axum::http::uri::InvalidUri| e.to_string())
}

/// Forwards one request to the plugin and answers with its response. On a
/// 101 both sides are upgraded and copied until either closes; the copy
/// runs on its own task, the 101 goes back to the client at once.
pub async fn forward(
    client: &HttpClient,
    addr: &PluginAddr,
    name: &str,
    mut req: Request,
) -> Response {
    // Taken before the request is consumed: hyper stores the client
    // side's upgrade handle in the request's extensions.
    let downstream = hyper::upgrade::on(&mut req);
    let (parts, body) = req.into_parts();
    let bytes = match read_body(body).await {
        Ok(b) => b,
        Err(BodyError::TooLarge) => {
            return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "body exceeds 1 MiB")
                .into_response();
        }
        Err(BodyError::Transport(e)) => {
            return ApiError::new(StatusCode::BAD_REQUEST, format!("body: {e}")).into_response();
        }
    };
    let rest = forwarded_rest(parts.uri.path());
    let uri = match upstream_uri(&addr.listen, rest, parts.uri.query()) {
        Ok(u) => u,
        Err(e) => return ApiError::new(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let mut headers = forwarded_headers(&parts.headers);
    let prefix = format!("/v1/plugins/{name}");
    let (Ok(prefix), Ok(bearer)) = (
        HeaderValue::from_str(&prefix),
        HeaderValue::from_str(&format!("Bearer {}", addr.token)),
    ) else {
        return ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "proxy: bad header value")
            .into_response();
    };
    headers.insert(HeaderName::from_static(FORWARDED_PREFIX), prefix);
    headers.insert(header::AUTHORIZATION, bearer);
    let mut upstream = match Request::builder()
        .method(parts.method)
        .uri(uri)
        .body(Body::from(bytes))
    {
        Ok(r) => r,
        Err(e) => {
            return ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("proxy: {e}"))
                .into_response();
        }
    };
    *upstream.headers_mut() = headers;
    let mut resp = match client.request(upstream).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(plugin = %name, "proxy: {e}");
            return ApiError::new(StatusCode::BAD_GATEWAY, format!("plugin {name:?}: {e}"))
                .into_response();
        }
    };
    let upgraded = resp.status() == StatusCode::SWITCHING_PROTOCOLS;
    if upgraded && !is_upgrade(&parts.headers) {
        // The client never asked; awaiting its side of the upgrade would
        // park this task, and the plugin's connection with it, for good.
        tracing::warn!(plugin = %name, "proxy: 101 to a request that did not ask to upgrade");
        return ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("plugin {name:?}: answered 101 to a request that did not ask to upgrade"),
        )
        .into_response();
    }
    if upgraded {
        let upstream = hyper::upgrade::on(&mut resp);
        let plugin = name.to_string();
        tokio::spawn(async move {
            // The client's side first: if it does not complete there is
            // nothing to copy, and the plugin's side may never resolve.
            let a = match downstream.await {
                Ok(a) => a,
                Err(e) => {
                    tracing::debug!(plugin = %plugin, "upgrade failed downstream: {e}");
                    return;
                }
            };
            let b = match upstream.await {
                Ok(b) => b,
                Err(e) => {
                    tracing::debug!(plugin = %plugin, "upgrade failed upstream: {e}");
                    return;
                }
            };
            let (mut a, mut b) = (TokioIo::new(a), TokioIo::new(b));
            if let Err(e) = tokio::io::copy_bidirectional(&mut a, &mut b).await {
                tracing::debug!(plugin = %plugin, "proxied stream ended: {e}");
            }
        });
    }
    let (mut parts, body) = resp.into_parts();
    parts.headers = response_headers(&parts.headers, upgraded);
    Response::from_parts(parts, Body::new(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use proptest::prelude::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn credentials_host_and_hop_by_hop_headers_never_cross() {
        let src = headers(&[
            ("authorization", "Bearer admin"),
            ("cookie", "balerix_session=x"),
            ("host", "127.0.0.1:7643"),
            ("keep-alive", "timeout=5"),
            ("proxy-authorization", "x"),
            ("te", "trailers"),
            ("trailer", "x"),
            ("transfer-encoding", "chunked"),
            ("connection", "Upgrade"),
            ("upgrade", "h2c"),
            ("accept", "text/html"),
            ("x-custom", "1"),
        ]);
        let out = forwarded_headers(&headers(
            &src.iter()
                .filter(|(k, _)| k.as_str() != "upgrade")
                .map(|(k, v)| (k.as_str(), v.to_str().unwrap()))
                .collect::<Vec<_>>(),
        ));
        let names: Vec<&str> = out.keys().map(|k| k.as_str()).collect();
        assert_eq!(
            names,
            vec!["accept", "x-custom"],
            "no upgrade: connection dropped too"
        );
        let out = forwarded_headers(&src);
        let mut names: Vec<&str> = out.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["accept", "connection", "upgrade", "x-custom"],
            "an upgrade keeps its two hop-by-hop headers"
        );
        let resp = response_headers(
            &headers(&[
                ("set-cookie", "a=1"),
                ("transfer-encoding", "chunked"),
                ("connection", "close"),
                ("content-type", "text/html"),
            ]),
            false,
        );
        let mut names: Vec<&str> = resp.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["content-type", "set-cookie"]);
        let resp = response_headers(
            &headers(&[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-accept", "x"),
            ]),
            true,
        );
        assert_eq!(resp.len(), 3, "a 101 keeps its upgrade headers");
    }

    #[test]
    fn the_upstream_uri_lands_under_v1_routes() {
        assert_eq!(
            upstream_uri("127.0.0.1:4000", "", None)
                .unwrap()
                .to_string(),
            "http://127.0.0.1:4000/v1/routes"
        );
        assert_eq!(
            upstream_uri("127.0.0.1:4000", "agents/f/c/a/ws", Some("cols=80"))
                .unwrap()
                .to_string(),
            "http://127.0.0.1:4000/v1/routes/agents/f/c/a/ws?cols=80"
        );
        // what the client encoded is what the plugin receives: a `%3F`
        // stays one path byte instead of starting a query, and a `%20`
        // parses where a decoded space would not
        assert_eq!(
            upstream_uri("127.0.0.1:4000", "a%20b%3Fx=1%2Fy", None)
                .unwrap()
                .to_string(),
            "http://127.0.0.1:4000/v1/routes/a%20b%3Fx=1%2Fy"
        );
        assert!(upstream_uri("127.0.0.1:4000", "a b", None).is_err());
        for rest in [
            "..",
            "../hook",
            "a/../../hook",
            "%2e%2e/hook",
            "a/%2E%2e",
            ".",
            "a/./b",
        ] {
            assert_eq!(
                upstream_uri("127.0.0.1:4000", rest, None),
                Err("path: . and .. segments are not forwarded".to_string()),
                "{rest}"
            );
        }
        // only a whole segment is a dot segment
        assert!(upstream_uri("127.0.0.1:4000", "..a/b..", None).is_ok());
    }

    #[test]
    fn the_rest_is_taken_from_the_raw_path_undecoded() {
        assert_eq!(forwarded_rest("/v1/plugins/web/"), "");
        assert_eq!(forwarded_rest("/v1/plugins/web/a/b"), "a/b");
        assert_eq!(
            forwarded_rest("/v1/plugins/web/a%20b%3Fx=1"),
            "a%20b%3Fx=1",
            "axum's Path would have decoded these"
        );
        assert_eq!(forwarded_rest("/v1/plugins/web/../hook"), "../hook");
        // never reached through the mount's routes, but never a panic
        assert_eq!(forwarded_rest("/v1/plugins/web"), "");
        assert_eq!(forwarded_rest("/v1/fleets"), "");
    }

    #[test]
    fn headers_named_by_connection_are_hop_by_hop() {
        // RFC 9110 §7.6.1: every field name listed in `Connection` is
        // hop-by-hop, whatever it is called.
        let out = forwarded_headers(&headers(&[
            ("connection", "keep-alive, X-Hop"),
            ("x-hop", "1"),
            ("accept", "text/html"),
        ]));
        let names: Vec<&str> = out.keys().map(|k| k.as_str()).collect();
        assert_eq!(names, vec!["accept"]);
        let resp = response_headers(
            &headers(&[
                ("connection", "close, x-resp-hop"),
                ("x-resp-hop", "1"),
                ("content-type", "text/html"),
            ]),
            false,
        );
        let names: Vec<&str> = resp.keys().map(|k| k.as_str()).collect();
        assert_eq!(names, vec!["content-type"]);
    }

    #[test]
    fn an_upgrade_is_both_headers_together() {
        assert!(
            !is_upgrade(&headers(&[("upgrade", "websocket")])),
            "Upgrade alone is not an upgrade request"
        );
        assert!(!is_upgrade(&headers(&[("connection", "Upgrade")])));
        assert!(is_upgrade(&headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket")
        ])));
        assert!(
            is_upgrade(&headers(&[
                ("connection", "keep-alive, upgrade"),
                ("upgrade", "websocket")
            ])),
            "a token among others, in any case"
        );
        let out = forwarded_headers(&headers(&[
            ("connection", "keep-alive, X-Hop, Upgrade"),
            ("x-hop", "1"),
            ("upgrade", "websocket"),
        ]));
        assert_eq!(
            out.get("connection").map(|v| v.to_str().unwrap()),
            Some("Upgrade"),
            "rewritten: the names it carried are gone"
        );
        assert!(!out.contains_key("x-hop"));
        let out = forwarded_headers(&headers(&[("upgrade", "h2c"), ("accept", "*/*")]));
        let names: Vec<&str> = out.keys().map(|k| k.as_str()).collect();
        assert_eq!(names, vec!["accept"], "a stray Upgrade is not forwarded");
    }

    /// A body whose first frame is a transport error.
    struct Failing;

    impl hyper::body::Body for Failing {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, std::io::Error>>> {
            std::task::Poll::Ready(Some(Err(std::io::Error::other("connection reset"))))
        }
    }

    #[tokio::test]
    async fn a_too_large_body_and_a_broken_body_are_told_apart() {
        let exact = Body::from(vec![b'x'; MAX_BODY]);
        assert_eq!(read_body(exact).await.unwrap().len(), MAX_BODY);
        let big = Body::from(vec![b'x'; MAX_BODY + 1]);
        assert!(matches!(read_body(big).await, Err(BodyError::TooLarge)));
        match read_body(Body::new(Failing)).await {
            Err(BodyError::Transport(e)) => assert!(e.contains("connection reset"), "{e}"),
            other => panic!("a broken body is not a 413: {other:?}"),
        }
    }

    proptest! {
        /// Whatever comes in, the daemon's credentials and the hop-by-hop
        /// set never go out, and connection/upgrade go out only together
        /// with an upgrade.
        #[test]
        fn forwarded_headers_never_leak(
            names in proptest::collection::vec("[a-z-]{1,12}", 0..12),
            upgrade in proptest::bool::ANY,
        ) {
            let mut pairs: Vec<(String, String)> = names.iter().map(|n| (n.clone(), "v".to_string())).collect();
            for n in ["authorization", "cookie", "host", "te", "transfer-encoding"] {
                pairs.push((n.to_string(), "v".to_string()));
            }
            if upgrade {
                pairs.push(("connection".to_string(), "Upgrade".to_string()));
                pairs.push(("upgrade".to_string(), "websocket".to_string()));
            } else {
                pairs.push(("connection".to_string(), "keep-alive".to_string()));
            }
            let src = headers(&pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>());
            let out = forwarded_headers(&src);
            for k in out.keys() {
                prop_assert!(!DROPPED.contains(&k.as_str()), "{k}");
            }
            prop_assert_eq!(out.contains_key("connection"), upgrade);
            prop_assert_eq!(out.contains_key("upgrade"), upgrade);
        }
    }
}
