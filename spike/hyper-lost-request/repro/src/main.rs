//! Reproducer for a lost request in hyper-util's legacy `Client` (HTTP/1).
//!
//! The server answers `/stream` with a chunked body that sends one chunk and
//! then stays open (like a Kubernetes watch), and `/quick` at once. Streamer
//! tasks keep opening streams on pooled connections and hold each body for
//! `HOLD`; quick tasks send `/quick` with a `DEADLINE`. A quick request that
//! misses its deadline was queued onto a connection that is still streaming:
//! the connection went back to the idle pool as soon as the stream's response
//! head arrived, because its `want` signal was stale.
//!
//! usage: hyper-stale-want-repro [seconds] [streamers] [quick-workers]
//! (defaults 60 64 16)
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Frame;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;

/// How long a streamer holds a stream open.
const HOLD: Duration = Duration::from_secs(5);
/// How long a quick request may take before it counts as lost. Localhost
/// answers in well under a millisecond.
const DEADLINE: Duration = Duration::from_secs(2);

type Body = http_body_util::combinators::BoxBody<Bytes, Infallible>;

async fn serve(req: Request<hyper::body::Incoming>) -> Result<Response<Body>, Infallible> {
    if req.uri().path() == "/stream" {
        // one chunk, then nothing until the client goes away
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, Infallible>>(1);
        tokio::spawn(async move {
            let _ = tx.send(Ok(Frame::data(Bytes::from_static(b"{}\n")))).await;
            tx.closed().await;
        });
        Ok(Response::new(chan_body(rx).boxed()))
    } else {
        Ok(Response::new(
            http_body_util::Full::new(Bytes::from_static(b"ok")).boxed(),
        ))
    }
}

fn chan_body(
    rx: tokio::sync::mpsc::Receiver<Result<Frame<Bytes>, Infallible>>,
) -> impl hyper::body::Body<Data = Bytes, Error = Infallible> {
    // a Body over the channel
    struct ChanBody(tokio::sync::mpsc::Receiver<Result<Frame<Bytes>, Infallible>>);
    impl hyper::body::Body for ChanBody {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            self.0.poll_recv(cx)
        }
    }
    ChanBody(rx)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<u64> = std::env::args()
        .skip(1)
        .map(|a| a.parse().unwrap())
        .collect();
    let secs = args.first().copied().unwrap_or(60);
    let streamers = args.get(1).copied().unwrap_or(64);
    let quick = args.get(2).copied().unwrap_or(16);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (io, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(io), service_fn(serve))
                    .await;
            });
        }
    });

    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let stop = Instant::now() + Duration::from_secs(secs);
    let (sent, lost, streams) = (
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
    );

    let mut tasks = Vec::new();
    for _ in 0..streamers {
        let (client, streams) = (client.clone(), streams.clone());
        tasks.push(tokio::spawn(async move {
            while Instant::now() < stop {
                // a quick request first, so the stream goes out on a just-
                // released pooled connection whose task is still settling
                let quick: hyper::Uri = format!("http://{addr}/quick").parse().unwrap();
                if let Ok(res) = client.get(quick).await {
                    let _ = res.into_body().collect().await;
                }
                let uri = format!("http://{addr}/stream").parse().unwrap();
                let Ok(res) = client.get(uri).await else {
                    continue;
                };
                streams.fetch_add(1, Ordering::Relaxed);
                // hold the open body like a watcher, then drop it
                let mut body = res.into_body();
                let _ = tokio::time::timeout(HOLD, async { while body.frame().await.is_some() {} })
                    .await;
            }
        }));
    }
    for _ in 0..quick {
        let (client, sent, lost) = (client.clone(), sent.clone(), lost.clone());
        tasks.push(tokio::spawn(async move {
            while Instant::now() < stop {
                let uri: hyper::Uri = format!("http://{addr}/quick").parse().unwrap();
                sent.fetch_add(1, Ordering::Relaxed);
                let started = Instant::now();
                let answer = tokio::time::timeout(DEADLINE, async {
                    let res = client.get(uri).await.map_err(|_| ())?;
                    res.into_body().collect().await.map(|_| ()).map_err(|_| ())
                })
                .await;
                if answer.is_err() {
                    let n = lost.fetch_add(1, Ordering::Relaxed) + 1;
                    eprintln!(
                        "LOST quick request #{n}: no answer in {:?}",
                        started.elapsed()
                    );
                }
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    println!(
        "quick requests: {} sent, {} lost (> {:?}); streams opened: {}",
        sent.load(Ordering::Relaxed),
        lost.load(Ordering::Relaxed),
        DEADLINE,
        streams.load(Ordering::Relaxed)
    );
}
