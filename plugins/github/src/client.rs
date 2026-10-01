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
use crate::github::{
    GitHubError, GitHubPort, IssueInfo, Permission, ReviewComment, ReviewComments, Target,
};
use crate::webhook::Listener;

pub const API: &str = "https://api.github.com";

/// A review's inline comments are read a hundred to a page (GitHub's
/// largest) for at most ten pages; past that the agent is told the
/// review goes on (#95).
const REVIEW_PAGE: usize = 100;
const REVIEW_PAGES: usize = 10;

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
    base: reqwest::Url,
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
            base: reqwest::Url::parse(API).map_err(|e| e.to_string())?,
        })
    }

    /// The same client against a local fixture instead of GitHub.
    #[cfg(test)]
    fn with_base(mut self, base: &str) -> Self {
        self.base = reqwest::Url::parse(base).unwrap();
        self
    }

    /// The API URL of `parts` and `query`, percent-encoded (#95). A part
    /// may hold several segments (`owner/name`, a file path): its `/`
    /// stays a separator and everything else in it is escaped, so a `?`,
    /// `#` or `%` in a branch or a file name cannot end the path early.
    fn url(&self, parts: &[&str], query: &[(&str, &str)]) -> reqwest::Url {
        let mut url = self.base.clone();
        // Only a cannot-be-a-base URL has no segments; `base` is http(s).
        if let Ok(mut segments) = url.path_segments_mut() {
            segments
                .pop_if_empty()
                .extend(parts.iter().flat_map(|p| p.split('/')));
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
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
            .post(self.url(
                &[
                    "app",
                    "installations",
                    &installation.to_string(),
                    "access_tokens",
                ],
                &[],
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
        url: reqwest::Url,
        body: Option<Value>,
    ) -> Result<Value, GitHubError> {
        let path = url.path().to_string();
        let mut refreshed = false;
        loop {
            let token = self.installation_token(installation, refreshed).await?;
            let mut req = self
                .http
                .request(method.clone(), url.clone())
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
            .get(self.url(&["app"], &[]))
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
            .call(
                installation,
                Method::GET,
                self.url(&["repos", repo], &[]),
                None,
            )
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
                self.url(&["repos", repo, "contents", path], &[("ref", git_ref)]),
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
                self.url(&["repos", repo, "collaborators", login, "permission"], &[]),
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
                self.url(&["repos", repo, "issues", &number.to_string()], &[]),
                None,
            )
            .await?;
        let pr = if v.get("pull_request").is_some() {
            let p = self
                .call(
                    installation,
                    Method::GET,
                    self.url(&["repos", repo, "pulls", &number.to_string()], &[]),
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
                self.url(
                    &["repos", repo, "issues", &number.to_string(), "comments"],
                    &[],
                ),
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
            self.url(
                &["repos", repo, "issues", "comments", &comment_id.to_string()],
                &[],
            ),
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
        let url = match target {
            Target::Issue(n) => {
                self.url(&["repos", repo, "issues", &n.to_string(), "reactions"], &[])
            }
            Target::Comment(id) => self.url(
                &[
                    "repos",
                    repo,
                    "issues",
                    "comments",
                    &id.to_string(),
                    "reactions",
                ],
                &[],
            ),
        };
        self.call(
            installation,
            Method::POST,
            url,
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
    ) -> Result<ReviewComments, GitHubError> {
        let per_page = REVIEW_PAGE.to_string();
        let mut comments: Vec<ReviewComment> = Vec::new();
        // One page past the cap, so a review of exactly the cap is not
        // reported as longer than it is.
        for page in 1..=REVIEW_PAGES + 1 {
            let v = self
                .call(
                    installation,
                    Method::GET,
                    self.url(
                        &[
                            "repos",
                            repo,
                            "pulls",
                            &number.to_string(),
                            "reviews",
                            &review_id.to_string(),
                            "comments",
                        ],
                        &[("per_page", &per_page), ("page", &page.to_string())],
                    ),
                    None,
                )
                .await?;
            let batch: Vec<ReviewComment> = serde_json::from_value(v)
                .map_err(|e| GitHubError::Other(format!("review comments: {e}")))?;
            let last = batch.len() < REVIEW_PAGE;
            comments.extend(batch);
            if last {
                break;
            }
        }
        let cap = REVIEW_PAGE * REVIEW_PAGES;
        let more = comments.len() > cap;
        comments.truncate(cap);
        Ok(ReviewComments { comments, more })
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    /// What the fixture saw of one API request.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Seen {
        uri: String,
        token: String,
    }

    /// A local stand-in for api.github.com: it mints the installation
    /// tokens `t1`, `t2`, … and answers every other request from a
    /// script of (request index, request URI) → (status, body).
    struct Fixture {
        client: GitHubClient,
        mints: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl Fixture {
        fn mints(&self) -> usize {
            self.mints.load(Ordering::SeqCst)
        }
        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
        fn uris(&self) -> Vec<String> {
            self.seen().into_iter().map(|s| s.uri).collect()
        }
    }

    async fn fixture(
        script: impl Fn(usize, &str) -> (u16, String) + Send + Sync + 'static,
    ) -> Fixture {
        let mints = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(script);
        let router = axum::Router::new()
            .route(
                "/app/installations/{id}/access_tokens",
                axum::routing::post({
                    let mints = mints.clone();
                    move || {
                        let n = mints.fetch_add(1, Ordering::SeqCst) + 1;
                        async move { axum::Json(json!({ "token": format!("t{n}") })) }
                    }
                }),
            )
            .fallback({
                let seen = seen.clone();
                move |req: axum::extract::Request| {
                    let uri = req.uri().to_string();
                    let token = req
                        .headers()
                        .get(AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .unwrap_or("")
                        .to_string();
                    let nth = {
                        let mut seen = seen.lock().unwrap();
                        seen.push(Seen {
                            uri: uri.clone(),
                            token,
                        });
                        seen.len() - 1
                    };
                    let (status, body) = script(nth, &uri);
                    async move { (StatusCode::from_u16(status).unwrap(), body) }
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        Fixture {
            client: GitHubClient::new(1, &Secret::new(KEY))
                .unwrap()
                .with_base(&base),
            mints,
            seen,
        }
    }

    const REPO_JSON: &str = r#"{"default_branch":"trunk"}"#;

    #[tokio::test]
    async fn a_401_drops_the_token_and_retries_once_with_a_fresh_one() {
        let f = fixture(|nth, _| match nth {
            0 => (401, r#"{"message":"Bad credentials"}"#.into()),
            _ => (200, REPO_JSON.into()),
        })
        .await;
        assert_eq!(
            f.client.default_branch(7, "acme/api").await.unwrap(),
            "trunk"
        );
        assert_eq!(f.mints(), 2);
        let tokens: Vec<String> = f.seen().into_iter().map(|s| s.token).collect();
        assert_eq!(tokens, ["t1", "t2"]);
    }

    #[tokio::test]
    async fn a_second_401_is_an_auth_error_with_no_third_attempt() {
        let f = fixture(|_, _| (401, r#"{"message":"Bad credentials"}"#.into())).await;
        assert_eq!(
            f.client.default_branch(7, "acme/api").await,
            Err(GitHubError::Auth(
                "installation token rejected twice".into()
            ))
        );
        assert_eq!(f.seen().len(), 2);
        assert_eq!(f.mints(), 2);
    }

    #[tokio::test]
    async fn an_accepted_token_is_reused_by_the_next_call() {
        let f = fixture(|_, _| (200, REPO_JSON.into())).await;
        f.client.default_branch(7, "acme/api").await.unwrap();
        f.client.default_branch(7, "acme/api").await.unwrap();
        assert_eq!(f.mints(), 1);
        let tokens: Vec<String> = f.seen().into_iter().map(|s| s.token).collect();
        assert_eq!(tokens, ["t1", "t1"]);
    }

    #[tokio::test]
    async fn a_file_path_and_a_ref_reach_github_percent_encoded() {
        let f = fixture(|_, _| (200, r#"{"content":"a2luZDogRmxlZXQ=\n"}"#.into())).await;
        let text = f
            .client
            .read_file(7, "acme/api", ".github/my config.yaml", "fix#1%&x")
            .await
            .unwrap();
        assert_eq!(text.as_deref(), Some("kind: Fleet"));
        assert_eq!(
            f.uris(),
            ["/repos/acme/api/contents/.github/my%20config.yaml?ref=fix%231%25%26x"]
        );
    }

    #[tokio::test]
    async fn a_question_mark_in_a_path_segment_stays_in_the_path() {
        let f = fixture(|_, _| (200, r#"{"permission":"write"}"#.into())).await;
        f.client.permission(7, "acme/api", "a?b#c").await.unwrap();
        assert_eq!(
            f.uris(),
            ["/repos/acme/api/collaborators/a%3Fb%23c/permission"]
        );
    }

    /// `n` review comments numbered from `from`, as GitHub lists them.
    fn page(from: usize, n: usize) -> String {
        let items: Vec<Value> = (from..from + n)
            .map(|i| json!({ "path": "src/lib.rs", "body": format!("c{i}") }))
            .collect();
        Value::Array(items).to_string()
    }

    /// Serves `total` comments a hundred to a page, by the `page` query.
    fn paged(total: usize) -> impl Fn(usize, &str) -> (u16, String) {
        move |_, uri| {
            let n: usize = uri.rsplit_once("page=").unwrap().1.parse().unwrap();
            let from = (n - 1) * 100;
            (200, page(from, total.saturating_sub(from).min(100)))
        }
    }

    #[tokio::test]
    async fn review_comments_follow_the_pages_to_a_short_one() {
        let f = fixture(paged(250)).await;
        let r = f
            .client
            .review_comments(7, "acme/api", 34, 9)
            .await
            .unwrap();
        assert!(!r.more);
        let bodies: Vec<&str> = r.comments.iter().map(|c| c.body.as_str()).collect();
        let want: Vec<String> = (0..250).map(|i| format!("c{i}")).collect();
        assert_eq!(bodies, want);
        assert_eq!(
            f.uris(),
            [
                "/repos/acme/api/pulls/34/reviews/9/comments?per_page=100&page=1",
                "/repos/acme/api/pulls/34/reviews/9/comments?per_page=100&page=2",
                "/repos/acme/api/pulls/34/reviews/9/comments?per_page=100&page=3",
            ]
        );
    }

    #[tokio::test]
    async fn review_comments_stop_at_a_thousand_and_say_there_are_more() {
        let f = fixture(paged(1001)).await;
        let r = f
            .client
            .review_comments(7, "acme/api", 34, 9)
            .await
            .unwrap();
        assert!(r.more);
        assert_eq!(r.comments.len(), 1000);
        assert_eq!(r.comments[999].body, "c999");
        assert_eq!(f.seen().len(), 11);
    }

    #[tokio::test]
    async fn exactly_a_thousand_review_comments_are_all_of_them() {
        let f = fixture(paged(1000)).await;
        let r = f
            .client
            .review_comments(7, "acme/api", 34, 9)
            .await
            .unwrap();
        assert!(!r.more);
        assert_eq!(r.comments.len(), 1000);
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
