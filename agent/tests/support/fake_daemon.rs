//! A fake Daemon for the sidecar's tests (Spec O §7.2): the link, the
//! attach socket and the events route, each connection handed to the test
//! through a channel. `stop` ends the server and every open socket; a
//! second `start` on the same address is "the Daemon came back".
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::IntoFuture;
use std::net::SocketAddr;

use axum::Router;
use axum::extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use balerix_api::{LinkRequest, SidecarFrame};
use futures_util::SinkExt;
use tokio::sync::{mpsc, watch};

/// One link connection as the fake Daemon sees it.
pub struct Conn {
    pub headers: HeaderMap,
    pub to_sidecar: mpsc::UnboundedSender<LinkRequest>,
    pub from_sidecar: mpsc::UnboundedReceiver<SidecarFrame>,
}

/// One attach socket as the fake Daemon sees it.
pub struct AttachConn {
    pub headers: HeaderMap,
    pub session: String,
    pub to_sidecar: mpsc::UnboundedSender<AxMsg>,
    pub from_sidecar: mpsc::UnboundedReceiver<AxMsg>,
}

#[derive(Clone)]
struct Fake {
    links: mpsc::UnboundedSender<Conn>,
    attaches: mpsc::UnboundedSender<AttachConn>,
    events: mpsc::UnboundedSender<String>,
    /// Flipped by `stop`: every open socket closes.
    shutdown: watch::Receiver<bool>,
}

async fn link_route(State(f): State<Fake>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        let (to_tx, mut to_rx) = mpsc::unbounded_channel::<LinkRequest>();
        let (from_tx, from_rx) = mpsc::unbounded_channel::<SidecarFrame>();
        let _ = f.links.send(Conn {
            headers,
            to_sidecar: to_tx,
            from_sidecar: from_rx,
        });
        let mut shutdown = f.shutdown.clone();
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                req = to_rx.recv() => match req {
                    Some(r) => { let _ = socket.send(AxMsg::Text(serde_json::to_string(&r).unwrap().into())).await; }
                    None => { let _ = socket.close().await; break; }
                },
                msg = socket.recv() => match msg {
                    Some(Ok(AxMsg::Text(t))) => { let _ = from_tx.send(serde_json::from_str(t.as_str()).unwrap()); }
                    Some(Ok(_)) => {}
                    _ => break,
                },
            }
        }
    })
}

async fn attach_route(
    State(f): State<Fake>,
    Path((_f, _c, _a, session)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        let (to_tx, mut to_rx) = mpsc::unbounded_channel::<AxMsg>();
        let (from_tx, from_rx) = mpsc::unbounded_channel::<AxMsg>();
        let _ = f.attaches.send(AttachConn {
            headers,
            session,
            to_sidecar: to_tx,
            from_sidecar: from_rx,
        });
        let mut shutdown = f.shutdown.clone();
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                out = to_rx.recv() => match out {
                    Some(m) => { if socket.send(m).await.is_err() { break; } }
                    None => { let _ = socket.close().await; break; }
                },
                msg = socket.recv() => match msg {
                    Some(Ok(m)) => { let _ = from_tx.send(m); }
                    _ => break,
                },
            }
        }
    })
}

/// Records the event's name and answers as an empty chain.
async fn events_route(
    State(f): State<Fake>,
    body: axum::body::Bytes,
) -> axum::Json<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    if let Some(name) = v["hook_event_name"].as_str() {
        let _ = f.events.send(name.to_string());
    }
    axum::Json(serde_json::json!({}))
}

pub struct FakeDaemon {
    pub addr: SocketAddr,
    pub links: mpsc::UnboundedReceiver<Conn>,
    pub attaches: mpsc::UnboundedReceiver<AttachConn>,
    /// Every `hook_event_name` the events route received.
    pub events: mpsc::UnboundedReceiver<String>,
    shutdown: watch::Sender<bool>,
}

impl FakeDaemon {
    /// On `addr`, or any free port. Dropping it is `stop`.
    pub async fn start(addr: Option<SocketAddr>) -> FakeDaemon {
        let listener =
            tokio::net::TcpListener::bind(addr.unwrap_or_else(|| "127.0.0.1:0".parse().unwrap()))
                .await
                .unwrap();
        let addr = listener.local_addr().unwrap();
        let (links, links_rx) = mpsc::unbounded_channel();
        let (attaches, attaches_rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let app = Router::new()
            .route("/v1/agents/{f}/{c}/{a}/link", get(link_route))
            .route("/v1/agents/{f}/{c}/{a}/link/attach/{s}", get(attach_route))
            .route("/v1/agents/{f}/{c}/{a}/events", post(events_route))
            .with_state(Fake {
                links,
                attaches,
                events,
                shutdown: shutdown_rx.clone(),
            });
        let mut stopping = shutdown_rx;
        tokio::spawn(
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stopping.changed().await;
                })
                .into_future(),
        );
        FakeDaemon {
            addr,
            links: links_rx,
            attaches: attaches_rx,
            events: events_rx,
            shutdown,
        }
    }

    /// Stops listening and closes every open link and attach socket.
    pub fn stop(self) {
        let _ = self.shutdown.send(true);
    }
}
