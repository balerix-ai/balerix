//! `GET /v1/plugin-host/fleets/watch` (plugins spec §18.4): one text frame
//! per change, each the complete `fleets` list — the user fleets with
//! their activation overlay — so a consumer replaces its state and never
//! diffs or handles removals. The daemon's change tick wakes the handler;
//! it recomputes and sends only what differs from the last frame.
//!
//! Each connected client recomputes the overlay for itself on every tick
//! (`plugin_fleets`), O(clients × fleets) per change. Fine for the
//! handful of plugins that watch; a shared, tick-keyed snapshot is the
//! next step if watchers multiply.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use balerix_core::FleetRecord;

use crate::daemon::Daemon;

/// Dead clients are reaped by the ping's failure.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

pub async fn serve_watch(mut socket: WebSocket, daemon: Arc<Daemon>) {
    let mut changes = daemon.changes();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // the first tick is immediate
    let mut last: Option<Vec<FleetRecord>> = None;
    loop {
        let now = daemon.plugin_fleets().await;
        if last.as_ref() != Some(&now) {
            let text = match serde_json::to_string(&now) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("fleets/watch: cannot encode: {e}");
                    return;
                }
            };
            if socket.send(Message::Text(text.into())).await.is_err() {
                return;
            }
            last = Some(now);
        }
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    return;
                }
            }
            msg = socket.recv() => match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}
