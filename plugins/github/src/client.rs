//! The GitHub REST client (M-12): App JWT → installation token per
//! installation (cached until a minute before expiry), the endpoints
//! `GitHubPort` names, one retry on `Retry-After`, a 401 on an
//! installation token refetched once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use balerix_plugin_common::config::Secret;
use balerix_plugin_sdk::Host;
use base64::Engine;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::actor::{Actor, Command, Counters, Health, Queue, TICK};
use crate::config::DaemonConfig;
use crate::github::{GitHubError, GitHubPort, IssueInfo, Permission, ReviewComment, Target};
use crate::webhook::Listener;

pub const API: &str = "https://api.github.com";

#[derive(Debug, serde::Serialize, Deserialize)]
pub(crate) struct Claims {
    pub iss: String,
    pub iat: u64,
    pub exp: u64,
}

/// RS256, `iat` a minute back for clock skew, `exp` nine minutes ahead
/// (GitHub allows ten).
pub fn jwt(app_id: u64, key: &Secret, now: u64) -> Result<String, String> {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(key.expose().as_bytes())
        .map_err(|e| format!("privateKey: not an RSA PEM: {e}"))?;
    let claims = Claims {
        iss: app_id.to_string(),
        iat: now.saturating_sub(60),
        exp: now + 540,
    };
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &key,
    )
    .map_err(|e| format!("privateKey: signing: {e}"))
}

/// GitHub's two ways of saying "later": `Retry-After` seconds, or
/// `x-ratelimit-remaining: 0` with a reset epoch, on 403 or 429.
pub fn retry_after_ms(status: u16, headers: &HeaderMap, now: u64) -> Option<u64> {
    if status != 403 && status != 429 {
        return None;
    }
    let get = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    if let Some(s) = get("retry-after") {
        return Some(s.saturating_mul(1000));
    }
    if get("x-ratelimit-remaining") == Some(0)
        && let Some(reset) = get("x-ratelimit-reset")
    {
        return Some(reset.saturating_sub(now).saturating_mul(1000));
    }
    None
}

/// The JSON `message` field when the body parses as JSON, else the body's
/// first 200 characters — the same fallback for every failure body,
/// whatever GitHub (or a proxy in front of it) actually sent back.
fn error_message(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| text.chars().take(200).collect())
}

/// A non-2xx on an App or installation-token call is always `Auth` (§11):
/// never `Other`, whether or not the body happens to be JSON.
fn auth_error(what: &str, status: u16, text: &str) -> GitHubError {
    GitHubError::Auth(format!("{what}: HTTP {status}: {}", error_message(text)))
}

struct Token {
    value: Secret,
    expires: u64,
}

pub struct GitHubClient {
    http: reqwest::Client,
    app_id: u64,
    key: Secret,
    tokens: Mutex<HashMap<u64, Token>>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl GitHubClient {
    pub fn new(app_id: u64, key: &Secret) -> Result<Self, String> {
        jwt(app_id, key, now())?; // proves the PEM parses before any request
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!(
                "balerix-plugin-github/{}",
                env!("CARGO_PKG_VERSION")
            ))
            .map_err(|e| e.to_string())?,
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            app_id,
            key: key.clone(),
            tokens: Mutex::new(HashMap::new()),
        })
    }

    async fn installation_token(
        &self,
        installation: u64,
        force: bool,
    ) -> Result<Secret, GitHubError> {
        if !force
            && let Some(t) = self
                .tokens
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&installation)
            && t.expires > now() + 60
        {
            return Ok(t.value.clone());
        }
        let jwt = jwt(self.app_id, &self.key, now()).map_err(GitHubError::Auth)?;
        let resp = self
            .http
            .post(format!(
                "{API}/app/installations/{installation}/access_tokens"
            ))
            .header(AUTHORIZATION, format!("Bearer {jwt}"))
            .send()
            .await
            .map_err(|e| GitHubError::Other(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| GitHubError::Other(e.to_string()))?;
        if !status.is_success() {
            return Err(auth_error("installation token", status.as_u16(), &text));
        }
        let body: Value = serde_json::from_str(&text)
            .map_err(|e| GitHubError::Other(format!("installation token: bad JSON: {e}")))?;
        let value = Secret::new(body["token"].as_str().unwrap_or(""));
        // `expires_at` is RFC 3339; an hour is GitHub's fixed lifetime, so
        // a parse is not worth a dependency: cache for fifty minutes.
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                installation,
                Token {
                    value: value.clone(),
                    expires: now() + 50 * 60,
                },
            );
        Ok(value)
    }

    /// One request with the installation token; a 401 drops the token
    /// and retries once (§11); 403/429 with a retry hint is `RateLimited`;
    /// 404 is `NotFound`.
    async fn call(
        &self,
        installation: u64,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, GitHubError> {
        let mut refreshed = false;
        loop {
            let token = self.installation_token(installation, refreshed).await?;
            let mut req = self
                .http
                .request(method.clone(), format!("{API}{path}"))
                .header(AUTHORIZATION, format!("Bearer {}", token.expose()));
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| GitHubError::Other(e.to_string()))?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                continue;
            }
            if let Some(ms) = retry_after_ms(status.as_u16(), resp.headers(), now()) {
                return Err(GitHubError::RateLimited { retry_after_ms: ms });
            }
            if status == StatusCode::NOT_FOUND {
                return Err(GitHubError::NotFound);
            }
            if status == StatusCode::UNAUTHORIZED {
                return Err(GitHubError::Auth(
                    "installation token rejected twice".into(),
                ));
            }
            let text = resp
                .text()
                .await
                .map_err(|e| GitHubError::Other(e.to_string()))?;
            if !status.is_success() {
                return Err(GitHubError::Other(format!(
                    "{method} {path}: HTTP {}: {}",
                    status.as_u16(),
                    error_message(&text)
                )));
            }
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text)
                .map_err(|e| GitHubError::Other(format!("{method} {path}: bad JSON: {e}")));
        }
    }
}

impl GitHubPort for GitHubClient {
    async fn app_slug(&self) -> Result<String, GitHubError> {
        let jwt = jwt(self.app_id, &self.key, now()).map_err(GitHubError::Auth)?;
        let resp = self
            .http
            .get(format!("{API}/app"))
            .header(AUTHORIZATION, format!("Bearer {jwt}"))
            .send()
            .await
            .map_err(|e| GitHubError::Other(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| GitHubError::Other(e.to_string()))?;
        if !status.is_success() {
            return Err(auth_error("GET /app", status.as_u16(), &text));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| GitHubError::Other(format!("GET /app: bad JSON: {e}")))?;
        v["slug"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| GitHubError::Other("GET /app: no slug".into()))
    }
    async fn default_branch(&self, installation: u64, repo: &str) -> Result<String, GitHubError> {
        let v = self
            .call(installation, Method::GET, &format!("/repos/{repo}"), None)
            .await?;
        Ok(v["default_branch"].as_str().unwrap_or("main").to_string())
    }
    async fn read_file(
        &self,
        installation: u64,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>, GitHubError> {
        let v = match self
            .call(
                installation,
                Method::GET,
                &format!("/repos/{repo}/contents/{path}?ref={git_ref}"),
                None,
            )
            .await
        {
            Ok(v) => v,
            Err(GitHubError::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        let content: String = v["content"]
            .as_str()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content)
            .map_err(|e| GitHubError::Other(format!("contents of {path}: {e}")))?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }
    async fn permission(
        &self,
        installation: u64,
        repo: &str,
        login: &str,
    ) -> Result<Permission, GitHubError> {
        let v = self
            .call(
                installation,
                Method::GET,
                &format!("/repos/{repo}/collaborators/{login}/permission"),
                None,
            )
            .await?;
        Ok(Permission::parse(
            v["role_name"].as_str(),
            v["permission"].as_str(),
        ))
    }
    async fn issue(
        &self,
        installation: u64,
        repo: &str,
        number: u64,
    ) -> Result<IssueInfo, GitHubError> {
        let v = self
            .call(
                installation,
                Method::GET,
                &format!("/repos/{repo}/issues/{number}"),
                None,
            )
            .await?;
        let pr = if v.get("pull_request").is_some() {
            let p = self
                .call(
                    installation,
                    Method::GET,
                    &format!("/repos/{repo}/pulls/{number}"),
                    None,
                )
                .await?;
            Some((
                p["head"]["ref"].as_str().unwrap_or("").into(),
                p["head"]["repo"]["full_name"].as_str().unwrap_or("").into(),
                p["base"]["ref"].as_str().unwrap_or("").into(),
            ))
        } else {
            None
        };
        Ok(IssueInfo {
            title: v["title"].as_str().unwrap_or("").into(),
            body: v["body"].as_str().unwrap_or("").into(),
            url: v["html_url"].as_str().unwrap_or("").into(),
            pr,
        })
    }
    async fn comment(
        &self,
        installation: u64,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<u64, GitHubError> {
        let v = self
            .call(
                installation,
                Method::POST,
                &format!("/repos/{repo}/issues/{number}/comments"),
                Some(json!({ "body": body })),
            )
            .await?;
        v["id"]
            .as_u64()
            .ok_or_else(|| GitHubError::Other("comment: no id".into()))
    }
    async fn edit_comment(
        &self,
        installation: u64,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<(), GitHubError> {
        self.call(
            installation,
            Method::PATCH,
            &format!("/repos/{repo}/issues/comments/{comment_id}"),
            Some(json!({ "body": body })),
        )
        .await
        .map(|_| ())
    }
    async fn react(
        &self,
        installation: u64,
        repo: &str,
        target: Target,
        content: &str,
    ) -> Result<(), GitHubError> {
        let path = match target {
            Target::Issue(n) => format!("/repos/{repo}/issues/{n}/reactions"),
            Target::Comment(id) => format!("/repos/{repo}/issues/comments/{id}/reactions"),
        };
        self.call(
            installation,
            Method::POST,
            &path,
            Some(json!({ "content": content })),
        )
        .await
        .map(|_| ())
    }
    async fn review_comments(
        &self,
        installation: u64,
        repo: &str,
        number: u64,
        review_id: u64,
    ) -> Result<Vec<ReviewComment>, GitHubError> {
        let v = self
            .call(
                installation,
                Method::GET,
                &format!("/repos/{repo}/pulls/{number}/reviews/{review_id}/comments?per_page=100"),
                None,
            )
            .await?;
        serde_json::from_value(v).map_err(|e| GitHubError::Other(format!("review comments: {e}")))
    }
}

/// Proves the App, starts the actor with its slug, serves the webhook
/// listener into the queue and spawns the ticker (Spec M §3, §11).
pub struct GitHubLauncher {
    pub host: Host,
    pub counters: Counters,
    pub health: Health,
}

impl crate::plugin::Launcher for GitHubLauncher {
    async fn launch(&self, config: DaemonConfig, queue: Arc<Queue>) -> Result<(), String> {
        let client = GitHubClient::new(config.app_id, &config.private_key)?;
        let slug = client
            .app_slug()
            .await
            .map_err(|e| format!("proving the App: {e}"))?;
        tracing::info!("github: App @{slug}");
        let listener = tokio::net::TcpListener::bind(config.listen)
            .await
            .map_err(|e| format!("listen {}: {e}", config.listen))?;
        let mut actor = Actor::new(
            self.host.clone(),
            client,
            self.counters.clone(),
            self.health.clone(),
            slug,
        );
        actor.load().await;
        tokio::spawn(actor.run(queue.clone()));
        let sink_queue = queue.clone();
        let webhook = Listener::new(
            config.webhook_secret.clone(),
            move |ev| sink_queue.push(Command::Webhook(ev)),
            self.counters.webhooks.clone(),
        );
        // §11: the listener is one of the actor's three health kinds, so
        // its death is reported through the queue, not written to the cell.
        let listener_queue = queue.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::webhook::serve(listener, webhook.router()).await {
                listener_queue.push(Command::ListenerFailed(e.to_string()));
            }
        });
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TICK);
            loop {
                interval.tick().await;
                queue.push(Command::Tick);
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A throwaway 2048-bit key generated for this test only:
    // `openssl genrsa 2048`. Not a secret; it signs nothing real.
    const KEY: &str = include_str!("../tests/fixtures/test-app-key.pem");

    fn pub_key_of(_key: &str) -> String {
        include_str!("../tests/fixtures/test-app-key.pub.pem").to_string()
    }

    #[test]
    fn the_app_jwt_carries_iss_iat_and_a_ten_minute_exp() {
        let token = jwt(12345, &Secret::new(KEY), 1_700_000_000).unwrap();
        let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        v.validate_exp = false;
        v.set_required_spec_claims::<&str>(&[]);
        let key = jsonwebtoken::DecodingKey::from_rsa_pem(pub_key_of(KEY).as_bytes()).unwrap();
        let data = jsonwebtoken::decode::<Claims>(&token, &key, &v).unwrap();
        assert_eq!(data.claims.iss, "12345");
        assert_eq!(data.claims.iat, 1_700_000_000 - 60);
        assert_eq!(data.claims.exp, 1_700_000_000 + 540);
        assert!(
            jwt(1, &Secret::new("not a key"), 0)
                .unwrap_err()
                .contains("privateKey")
        );
    }

    #[test]
    fn auth_error_reads_the_json_message_or_falls_back_to_the_body() {
        assert_eq!(
            auth_error(
                "installation token",
                401,
                r#"{"message":"Bad credentials"}"#
            ),
            GitHubError::Auth("installation token: HTTP 401: Bad credentials".into())
        );
        assert_eq!(
            auth_error("GET /app", 502, "<html>oops</html>"),
            GitHubError::Auth("GET /app: HTTP 502: <html>oops</html>".into())
        );
    }

    #[test]
    fn retry_after_reads_the_header_then_the_reset_epoch() {
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_ms(200, &h, 0), None);
        assert_eq!(
            retry_after_ms(403, &h, 0),
            None,
            "a plain 403 is not a rate limit"
        );
        h.insert("retry-after", "2".parse().unwrap());
        assert_eq!(retry_after_ms(429, &h, 0), Some(2000));
        h.remove("retry-after");
        h.insert("x-ratelimit-remaining", "0".parse().unwrap());
        h.insert("x-ratelimit-reset", "1000".parse().unwrap());
        assert_eq!(retry_after_ms(403, &h, 997), Some(3000));
        assert_eq!(retry_after_ms(403, &h, 5000), Some(0));
    }
}
