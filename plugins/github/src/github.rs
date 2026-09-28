//! The GitHub port (M-11): what the actor needs from GitHub, behind a
//! trait with a recording fake, so the ordering rules are unit-tested
//! without GitHub. `client.rs` is the real implementation.

use std::future::Future;

/// Reactions GitHub accepts on issues and comments.
pub const EYES: &str = "eyes";
pub const PLUS_ONE: &str = "+1";
pub const MINUS_ONE: &str = "-1";
pub const CONFUSED: &str = "confused";
pub const HOORAY: &str = "hooray";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitHubError {
    #[error("rate limited, retry in {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },
    #[error("auth: {0}")]
    Auth(String),
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Other(String),
}

/// A collaborator's permission on a repository (`role_name` of
/// `GET /repos/{o}/{r}/collaborators/{login}/permission`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Admin,
    Maintain,
    Write,
    Triage,
    Read,
    None,
}

impl Permission {
    /// M-2: write permission is the boundary.
    pub fn may_prompt(self) -> bool {
        matches!(
            self,
            Permission::Admin | Permission::Maintain | Permission::Write
        )
    }

    /// From `role_name` (exact) or, when absent, `permission` (coarser:
    /// `maintain` reports as `write`, `triage` as `read`).
    pub fn parse(role_name: Option<&str>, permission: Option<&str>) -> Permission {
        match role_name.or(permission).unwrap_or("none") {
            "admin" => Permission::Admin,
            "maintain" => Permission::Maintain,
            "write" => Permission::Write,
            "triage" => Permission::Triage,
            "read" => Permission::Read,
            _ => Permission::None,
        }
    }
}

/// Where a reaction goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The issue or pull request body itself.
    Issue(u64),
    /// A comment, by id.
    Comment(u64),
}

/// One inline comment of a submitted review.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ReviewComment {
    pub path: String,
    #[serde(default)]
    pub side: String,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub original_line: Option<u64>,
    #[serde(default)]
    pub diff_hunk: String,
    #[serde(default)]
    pub body: String,
}

/// What `issue` answers: the issue or PR a comment was left on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueInfo {
    pub title: String,
    pub body: String,
    pub url: String,
    /// `(head, head_repo, base)` for a pull request.
    pub pr: Option<(String, String, String)>,
}

pub trait GitHubPort: Send + Sync + 'static {
    /// `GET /app` with the App JWT: proves the key and answers the slug
    /// the mention is matched against.
    fn app_slug(&self) -> impl Future<Output = Result<String, GitHubError>> + Send;
    fn default_branch(
        &self,
        installation: u64,
        repo: &str,
    ) -> impl Future<Output = Result<String, GitHubError>> + Send;
    /// The decoded file at `path` on `git_ref`; `None` when absent.
    fn read_file(
        &self,
        installation: u64,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> impl Future<Output = Result<Option<String>, GitHubError>> + Send;
    fn permission(
        &self,
        installation: u64,
        repo: &str,
        login: &str,
    ) -> impl Future<Output = Result<Permission, GitHubError>> + Send;
    /// Posts a comment on issue or PR `number`; answers its id.
    fn comment(
        &self,
        installation: u64,
        repo: &str,
        number: u64,
        body: &str,
    ) -> impl Future<Output = Result<u64, GitHubError>> + Send;
    /// `NotFound` when the comment was deleted.
    fn edit_comment(
        &self,
        installation: u64,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> impl Future<Output = Result<(), GitHubError>> + Send;
    fn react(
        &self,
        installation: u64,
        repo: &str,
        target: Target,
        content: &str,
    ) -> impl Future<Output = Result<(), GitHubError>> + Send;
    fn review_comments(
        &self,
        installation: u64,
        repo: &str,
        number: u64,
        review_id: u64,
    ) -> impl Future<Output = Result<Vec<ReviewComment>, GitHubError>> + Send;
    /// Title, body, URL and, for a PR, `(head, head_repo, base)`.
    fn issue(
        &self,
        installation: u64,
        repo: &str,
        number: u64,
    ) -> impl Future<Output = Result<IssueInfo, GitHubError>> + Send;
}

/// A recording port for tests; always compiled, `tests/plugin_it.rs`
/// uses it.
pub mod fake {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    use super::{GitHubError, GitHubPort, IssueInfo, Permission, ReviewComment, Target};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        AppSlug,
        DefaultBranch {
            repo: String,
        },
        ReadFile {
            repo: String,
            path: String,
            git_ref: String,
        },
        Permission {
            repo: String,
            login: String,
        },
        Comment {
            repo: String,
            number: u64,
            body: String,
            id: u64,
        },
        EditComment {
            repo: String,
            comment_id: u64,
            body: String,
        },
        React {
            repo: String,
            target: Target,
            content: String,
        },
        ReviewComments {
            repo: String,
            number: u64,
            review_id: u64,
        },
        Issue {
            repo: String,
            number: u64,
        },
    }

    #[derive(Default)]
    struct Inner {
        calls: Vec<Call>,
        next_id: u64,
        fail_next: Option<GitHubError>,
        files: HashMap<(String, String, String), String>,
        permissions: HashMap<(String, String), Permission>,
        default_branches: HashMap<String, String>,
        reviews: HashMap<(String, u64), Vec<ReviewComment>>,
        deleted: HashSet<u64>,
        issues: HashMap<(String, u64), IssueInfo>,
    }

    #[derive(Clone)]
    pub struct FakePort {
        slug: String,
        inner: Arc<Mutex<Inner>>,
    }

    impl FakePort {
        pub fn new(slug: &str) -> Self {
            Self {
                slug: slug.to_string(),
                inner: Arc::new(Mutex::new(Inner {
                    next_id: 1000,
                    ..Inner::default()
                })),
            }
        }
        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.inner.lock().unwrap_or_else(|e| e.into_inner())
        }
        pub fn calls(&self) -> Vec<Call> {
            self.lock().calls.clone()
        }
        pub fn take_calls(&self) -> Vec<Call> {
            std::mem::take(&mut self.lock().calls)
        }
        /// The next call of any kind fails once with `error`.
        pub fn fail_next(&self, error: GitHubError) {
            self.lock().fail_next = Some(error);
        }
        pub fn set_file(&self, repo: &str, git_ref: &str, path: &str, text: &str) {
            self.lock()
                .files
                .insert((repo.into(), git_ref.into(), path.into()), text.into());
        }
        pub fn set_permission(&self, repo: &str, login: &str, p: Permission) {
            self.lock()
                .permissions
                .insert((repo.into(), login.into()), p);
        }
        pub fn set_default_branch(&self, repo: &str, branch: &str) {
            self.lock()
                .default_branches
                .insert(repo.into(), branch.into());
        }
        pub fn set_review_comments(
            &self,
            repo: &str,
            review_id: u64,
            comments: Vec<ReviewComment>,
        ) {
            self.lock()
                .reviews
                .insert((repo.into(), review_id), comments);
        }
        /// What `issue` answers for `number`; unset, an empty title and
        /// body, the issue's URL, and no PR.
        pub fn set_issue(&self, repo: &str, number: u64, info: IssueInfo) {
            self.lock().issues.insert((repo.into(), number), info);
        }
        /// The comment is gone: `edit_comment` answers `NotFound`.
        pub fn delete_comment(&self, id: u64) {
            self.lock().deleted.insert(id);
        }
        fn check(&self) -> Result<(), GitHubError> {
            match self.lock().fail_next.take() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
        fn record(&self, call: Call) {
            self.lock().calls.push(call);
        }
    }

    impl GitHubPort for FakePort {
        async fn app_slug(&self) -> Result<String, GitHubError> {
            self.check()?;
            self.record(Call::AppSlug);
            Ok(self.slug.clone())
        }
        async fn default_branch(&self, _i: u64, repo: &str) -> Result<String, GitHubError> {
            self.check()?;
            self.record(Call::DefaultBranch { repo: repo.into() });
            Ok(self
                .lock()
                .default_branches
                .get(repo)
                .cloned()
                .unwrap_or_else(|| "main".into()))
        }
        async fn read_file(
            &self,
            _i: u64,
            repo: &str,
            path: &str,
            git_ref: &str,
        ) -> Result<Option<String>, GitHubError> {
            self.check()?;
            self.record(Call::ReadFile {
                repo: repo.into(),
                path: path.into(),
                git_ref: git_ref.into(),
            });
            Ok(self
                .lock()
                .files
                .get(&(repo.into(), git_ref.into(), path.into()))
                .cloned())
        }
        async fn permission(
            &self,
            _i: u64,
            repo: &str,
            login: &str,
        ) -> Result<Permission, GitHubError> {
            self.check()?;
            self.record(Call::Permission {
                repo: repo.into(),
                login: login.into(),
            });
            Ok(self
                .lock()
                .permissions
                .get(&(repo.into(), login.into()))
                .copied()
                .unwrap_or(Permission::None))
        }
        async fn comment(
            &self,
            _i: u64,
            repo: &str,
            number: u64,
            body: &str,
        ) -> Result<u64, GitHubError> {
            self.check()?;
            let id = {
                let mut g = self.lock();
                g.next_id += 1;
                g.next_id
            };
            self.record(Call::Comment {
                repo: repo.into(),
                number,
                body: body.into(),
                id,
            });
            Ok(id)
        }
        async fn edit_comment(
            &self,
            _i: u64,
            repo: &str,
            comment_id: u64,
            body: &str,
        ) -> Result<(), GitHubError> {
            self.check()?;
            self.record(Call::EditComment {
                repo: repo.into(),
                comment_id,
                body: body.into(),
            });
            if self.lock().deleted.contains(&comment_id) {
                return Err(GitHubError::NotFound);
            }
            Ok(())
        }
        async fn react(
            &self,
            _i: u64,
            repo: &str,
            target: Target,
            content: &str,
        ) -> Result<(), GitHubError> {
            self.check()?;
            self.record(Call::React {
                repo: repo.into(),
                target,
                content: content.into(),
            });
            Ok(())
        }
        async fn review_comments(
            &self,
            _i: u64,
            repo: &str,
            number: u64,
            review_id: u64,
        ) -> Result<Vec<ReviewComment>, GitHubError> {
            self.check()?;
            self.record(Call::ReviewComments {
                repo: repo.into(),
                number,
                review_id,
            });
            Ok(self
                .lock()
                .reviews
                .get(&(repo.into(), review_id))
                .cloned()
                .unwrap_or_default())
        }
        async fn issue(&self, _i: u64, repo: &str, number: u64) -> Result<IssueInfo, GitHubError> {
            self.check()?;
            self.record(Call::Issue {
                repo: repo.into(),
                number,
            });
            Ok(self
                .lock()
                .issues
                .get(&(repo.into(), number))
                .cloned()
                .unwrap_or_else(|| IssueInfo {
                    url: format!("https://github.com/{repo}/issues/{number}"),
                    ..IssueInfo::default()
                }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Call, FakePort};
    use super::*;

    #[test]
    fn permission_prefers_role_name_and_write_is_the_boundary() {
        assert_eq!(
            Permission::parse(Some("maintain"), Some("write")),
            Permission::Maintain
        );
        assert_eq!(Permission::parse(None, Some("write")), Permission::Write);
        assert_eq!(Permission::parse(None, None), Permission::None);
        assert!(Permission::Admin.may_prompt() && Permission::Write.may_prompt());
        assert!(!Permission::Triage.may_prompt() && !Permission::Read.may_prompt());
    }

    #[tokio::test]
    async fn the_fake_records_mints_ids_and_fails_once() {
        let p = FakePort::new("balerix");
        assert_eq!(p.app_slug().await.unwrap(), "balerix");
        let id = p.comment(1, "acme/api", 12, "hi").await.unwrap();
        assert_eq!(id, 1001);
        p.fail_next(GitHubError::Other("boom".into()));
        assert!(p.comment(1, "acme/api", 12, "again").await.is_err());
        assert_eq!(p.comment(1, "acme/api", 12, "again").await.unwrap(), 1002);
        p.delete_comment(1001);
        assert_eq!(
            p.edit_comment(1, "acme/api", 1001, "x").await,
            Err(GitHubError::NotFound)
        );
        assert_eq!(
            p.read_file(1, "acme/api", ".balerix.yaml", "main")
                .await
                .unwrap(),
            None
        );
        p.set_file("acme/api", "main", ".balerix.yaml", "kind: Fleet");
        assert_eq!(
            p.read_file(1, "acme/api", ".balerix.yaml", "main")
                .await
                .unwrap()
                .as_deref(),
            Some("kind: Fleet")
        );
        assert!(matches!(p.calls()[0], Call::AppSlug));
        assert_eq!(p.calls().len(), 6);
    }
}
