//! A scripted Daemon over plain HTTP (Spec O §21.3): records every `PUT`
//! and `DELETE`, answers `GET` with the record it keeps, and can be told
//! to reject or to stop.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use balerix_api::{
    AgentStatus, DeclaredPlugins, ErrorBody, FleetRecord, FleetRequest, ManagedFleet, PluginStatus,
};

#[derive(Default)]
struct Inner {
    puts: Vec<FleetRequest>,
    deletes: Vec<String>,
    records: BTreeMap<String, FleetRecord>,
    reject: Option<String>,
    /// `<fleet>` to (`fleet/crew/agent` to status), laid over every record read.
    agents: BTreeMap<String, BTreeMap<String, AgentStatus>>,
    lists: Vec<DeclaredPlugins>,
    /// `PUT /v1/plugins` answers 400 with this.
    reject_lists: Option<String>,
    plugin_rows: Vec<PluginStatus>,
    managed: Vec<ManagedFleet>,
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Inner>>);

pub struct StubDaemon {
    url: String,
    state: Shared,
    shutdown: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

async fn put_fleet(
    State(s): State<Shared>,
    Path(name): Path<String>,
    Json(request): Json<FleetRequest>,
) -> axum::response::Response {
    let mut inner = s.0.lock().unwrap();
    inner.puts.push(request.clone());
    if let Some(message) = &inner.reject {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: message.clone(),
            }),
        )
            .into_response();
    }
    let mut record = FleetRecord::with_owner(
        request.spec,
        Some(
            request
                .managed_by
                .clone()
                .unwrap_or_else(|| "kubernetes".into()),
        ),
    );
    record.status.agents = inner.agents.get(&name).cloned().unwrap_or_default();
    inner.records.insert(name, record.clone());
    Json(record).into_response()
}

async fn get_fleet(State(s): State<Shared>, Path(name): Path<String>) -> axum::response::Response {
    let inner = s.0.lock().unwrap();
    match inner.records.get(&name) {
        Some(record) => {
            let mut record = record.clone();
            record.status.agents = inner.agents.get(&name).cloned().unwrap_or_default();
            Json(record).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: format!("no fleet {name}"),
            }),
        )
            .into_response(),
    }
}

async fn delete_fleet(State(s): State<Shared>, Path(name): Path<String>) -> StatusCode {
    let mut inner = s.0.lock().unwrap();
    inner.deletes.push(name.clone());
    inner.records.remove(&name);
    StatusCode::NO_CONTENT
}

async fn put_plugins(
    State(s): State<Shared>,
    Json(list): Json<DeclaredPlugins>,
) -> axum::response::Response {
    let mut inner = s.0.lock().unwrap();
    inner.lists.push(list);
    match &inner.reject_lists {
        Some(message) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: message.clone(),
            }),
        )
            .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn get_plugins(State(s): State<Shared>) -> Json<Vec<PluginStatus>> {
    Json(s.0.lock().unwrap().plugin_rows.clone())
}

async fn get_managed(State(s): State<Shared>) -> Json<Vec<ManagedFleet>> {
    Json(s.0.lock().unwrap().managed.clone())
}

impl StubDaemon {
    pub async fn start() -> Self {
        let state = Shared::default();
        let router = Router::new()
            .route("/readyz", get(|| async { StatusCode::OK }))
            .route(
                "/v1/fleets/{name}",
                get(get_fleet).put(put_fleet).delete(delete_fleet),
            )
            .route("/v1/plugins", get(get_plugins).put(put_plugins))
            .route("/v1/managed-fleets", get(get_managed))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap()
        });
        Self {
            url,
            state,
            shutdown,
            server,
        }
    }
    pub fn url(&self) -> String {
        self.url.clone()
    }
    pub fn puts(&self) -> Vec<FleetRequest> {
        self.state.0.lock().unwrap().puts.clone()
    }
    pub fn deletes(&self) -> Vec<String> {
        self.state.0.lock().unwrap().deletes.clone()
    }
    pub fn lists(&self) -> Vec<DeclaredPlugins> {
        self.state.0.lock().unwrap().lists.clone()
    }
    pub fn set_plugin_rows(&self, rows: Vec<PluginStatus>) {
        self.state.0.lock().unwrap().plugin_rows = rows;
    }
    pub fn set_managed(&self, rows: Vec<ManagedFleet>) {
        self.state.0.lock().unwrap().managed = rows;
    }
    pub fn reject(&self, message: Option<&str>) {
        self.state.0.lock().unwrap().reject = message.map(str::to_string);
    }
    pub fn reject_lists(&self, message: Option<&str>) {
        self.state.0.lock().unwrap().reject_lists = message.map(str::to_string);
    }
    pub fn set_agent(&self, fleet: &str, key: &str, status: AgentStatus) {
        self.state
            .0
            .lock()
            .unwrap()
            .agents
            .entry(fleet.to_string())
            .or_default()
            .insert(key.to_string(), status);
    }
    /// Ends the server: the listener closes, idle keep-alive connections
    /// are shut, and connections are refused from here on.
    pub async fn stop(self) {
        let _ = self.shutdown.send(());
        self.server.await.unwrap();
    }
}
