//! Shared scaffolding for the plugin-protocol integration tests: the
//! blocking `Api` client of `api_it.rs` plus the plugin-host helpers, and
//! a `World` that serves a daemon with the chain handler on a real port.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// `support` is compiled into every integration test that declares it; a
// helper only one of them uses is not dead code for the suite.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use balerix_server::testing::{Harness, write_plugin_package};
use balerix_server::{Daemon, Metrics, PluginEventHandler, router, serve};
use serde_json::Value;

pub struct Api {
    pub base: String,
    token: String,
    agent: ureq::Agent,
}

impl Api {
    pub fn new(base: String, token: &str) -> Api {
        Api {
            base,
            token: token.to_string(),
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(5)))
                .http_status_as_error(false)
                .build()
                .into(),
        }
    }

    pub fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let mut req = match method {
            "GET" => self.agent.get(&url).force_send_body(),
            "POST" => self.agent.post(&url),
            "PUT" => self.agent.put(&url),
            "DELETE" => self.agent.delete(&url).force_send_body(),
            _ => unreachable!(),
        };
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        let mut resp = match body {
            Some(b) => req.send_json(b).unwrap(),
            None => req.send_empty().unwrap(),
        };
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap();
        let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, v)
    }

    pub fn admin(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        self.call(method, path, Some(&self.token.clone()), body)
    }

    /// The same, as a plugin: the bearer is the plugin's own token.
    pub fn plugin(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
        self.call(method, path, Some(token), body)
    }

    /// `PUT` with an opaque body, for the kv routes.
    pub fn raw_put(&self, token: &str, path: &str, bytes: &[u8]) -> (u16, Value) {
        let mut resp = self
            .agent
            .put(&format!("{}{path}", self.base))
            .header("Authorization", &format!("Bearer {token}"))
            .header("content-type", "application/octet-stream")
            .send(bytes)
            .unwrap();
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap();
        let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, v)
    }

    /// `GET` returning the bytes, for the kv routes.
    pub fn raw_get(&self, token: &str, path: &str) -> (u16, Vec<u8>) {
        let mut resp = self
            .agent
            .get(&format!("{}{path}", self.base))
            .force_send_body()
            .header("Authorization", &format!("Bearer {token}"))
            .send_empty()
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.body_mut().read_to_vec().unwrap();
        (status, bytes)
    }

    /// A request with explicit headers, redirects not followed, answered
    /// as status, response headers and body text — for the login and
    /// proxy paths, where the headers are the point.
    pub fn raw(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> (u16, Vec<(String, String)>, String) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .into();
        let url = format!("{}{path}", self.base);
        let mut req = match method {
            "GET" => agent.get(&url).force_send_body(),
            "POST" => agent.post(&url),
            _ => unreachable!(),
        };
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let mut resp = match body {
            Some(b) => req.send(b).unwrap(),
            None => req.send_empty().unwrap(),
        };
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let text = resp.body_mut().read_to_string().unwrap();
        (status, headers, text)
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

pub struct World {
    pub api: Api,
    pub daemon: Arc<Daemon>,
    pub h: Harness,
    pub dir: tempfile::TempDir,
    pub stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// A daemon with the chain handler, served on a port, with one package
/// `flow` declared (intercepts PreToolUse+Stop, observes Stop, needs
/// actions+kv) and one package `web` (observes SessionStart, needs
/// fleets+attach+workspace, serves routes).
pub async fn world() -> World {
    world_with(&[]).await
}

/// `world`, plus one package per `(name, manifest lines)` in `extra`,
/// declared after `flow` and `web`.
pub async fn world_with(extra: &[(&str, &str)]) -> World {
    let h = Harness::new(Duration::from_secs(3600));
    let dir = tempfile::tempdir().unwrap();
    write_plugin_package(
        &dir.path().join("flow-pkg"),
        "flow",
        "hooks: { intercept: [PreToolUse, Stop], observe: [Stop] }\nneeds: [actions, kv]\n",
    );
    write_plugin_package(
        &dir.path().join("web-pkg"),
        "web",
        "hooks: { observe: [SessionStart] }\nneeds: [fleets, attach, workspace]\nroutes: true\n",
    );
    let mut plugins_yaml = String::from(
        "plugins:\n  - name: flow\n    source: ./flow-pkg\n  - name: web\n    source: ./web-pkg\n",
    );
    for (name, manifest) in extra {
        write_plugin_package(&dir.path().join(format!("{name}-pkg")), name, manifest);
        plugins_yaml.push_str(&format!("  - name: {name}\n    source: ./{name}-pkg\n"));
    }
    std::fs::write(dir.path().join("plugins.yaml"), plugins_yaml).unwrap();
    // One registry for both: the handler's counters are the ones `/metrics`
    // encodes, so the daemon and the chain must share a `Metrics`.
    let metrics = Metrics::new().unwrap();
    let handler = PluginEventHandler::new(h.registry.clone(), h.client.clone(), metrics.clone());
    let daemon = h.daemon_with(handler, dir.path(), metrics);
    daemon.sync_plugins().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, router(daemon.clone()), async {
        let _ = rx.await;
    }));
    World {
        api: Api::new(base, "admin-tok"),
        daemon,
        h,
        dir,
        stop: Some(stop),
    }
}
