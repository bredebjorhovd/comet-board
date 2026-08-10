//! GitHub source: read-only in v0 (impl spec §4.2). Linear stays the system of
//! record; GitHub contributes issues and, more importantly, the pull requests
//! that flip a task to `review`.

use crate::config::{Credentials, Paths};
use crate::db::UpsertTask;
use crate::model::{Source, UpstreamState};
use crate::sources::github_app::{HttpAppApi, TokenProvider};
use anyhow::{Result, anyhow, bail};
use serde_json::Value;

pub const API: &str = "https://api.github.com";

/// The HTTP client every GitHub call shares — the board's REST calls and the
/// App's minting calls alike. `reqwest::blocking::Client` is a handle over a
/// pooled inner, so cloning it hands out the same connections rather than a
/// second pool.
pub fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("herdr-board/0.1")
        .build()?)
}

/// The credential `.env` configures, built and ready to stamp requests.
///
/// One function rather than one per caller, because an App carries a *cache*:
/// the REST client and the git credential path have to share one provider or
/// they mint two tokens for one installation and each keeps half the picture.
pub fn provider(credentials: &Credentials) -> Result<TokenProvider> {
    TokenProvider::from_auth(credentials.github_auth(), || {
        Ok(Box::new(HttpAppApi::new(client()?, API)))
    })
}

/// GitHub's maximum page size. `issues` walks pages until one comes back short;
/// `open_issues` deliberately reads only the first, because it exists to preview
/// roughly how much a repo would put on the board rather than to enumerate it.
pub const PAGE: usize = 100;

/// How many pages of issues to walk before giving up.
///
/// A ceiling rather than a limit: the loop stops on the first short page, so
/// this only bites on a repo with thousands of issues — where returning a
/// partial set would be worse than an error, because reaping deletes whatever
/// is not in it.
pub const MAX_PAGES: usize = 20;

/// The REST surface we use. A seam, so tests never touch the network.
pub trait Rest {
    fn get(&self, path: &str) -> Result<Value>;
    /// POST with a JSON body (comments).
    fn post(&self, path: &str, body: &Value) -> Result<Value>;
    /// PATCH with a JSON body (closing an issue).
    fn patch(&self, path: &str, body: &Value) -> Result<Value>;
    /// PUT with a JSON body (merging a pull request).
    fn put(&self, path: &str, body: &Value) -> Result<Value>;
}

/// One authorized round trip, already carrying whichever bearer the caller
/// resolved.
///
/// A seam under [`HttpRest`], separate from [`Rest`]: `Rest` is the *endpoint*
/// surface tests fake, this is the *wire*. Splitting them is what makes the 401
/// → re-mint → retry-once rule testable, because that rule lives between the
/// two and needs a 401 that no fixture-level fake can produce.
pub trait Transport {
    fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        bearer: Option<&str>,
    ) -> Result<Reply>;
}

/// A status and a body — everything the retry rule reads.
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

/// The [`Transport`] against real GitHub.
pub struct HttpTransport {
    client: reqwest::blocking::Client,
    base: String,
}

impl Transport for HttpTransport {
    fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        bearer: Option<&str>,
    ) -> Result<Reply> {
        let mut req = self
            .client
            .request(method, format!("{}{}", self.base, path))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = bearer {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send()?;
        let status = resp.status().as_u16();
        Ok(Reply {
            status,
            // A 204 has no body; treat that as success rather than a parse error.
            body: resp.json().unwrap_or(Value::Null),
        })
    }
}

pub struct HttpRest {
    transport: Box<dyn Transport>,
    /// What stamps `Authorization`: a personal access token, a GitHub App
    /// minting per-installation tokens, or nothing at all (gh#58).
    auth: TokenProvider,
}

impl HttpRest {
    /// A personal access token, or none — the constructor that existed before
    /// gh#58, kept so every call site that only has a token keeps working.
    pub fn new(token: Option<String>) -> Result<HttpRest> {
        Self::with_auth(match token {
            Some(t) => TokenProvider::Static(t),
            None => TokenProvider::Anonymous,
        })
    }

    /// Whichever credential `.env` configures: the App when one is set up, the
    /// personal access token otherwise. Nobody self-hosting on a PAT is forced
    /// onto an App.
    pub fn from_credentials(credentials: &Credentials) -> Result<HttpRest> {
        Self::with_auth(provider(credentials)?)
    }

    /// As [`HttpRest::from_credentials`], reading `.env` itself — the shape the
    /// one-shot CLI paths want.
    pub fn from_paths(paths: &Paths) -> Result<HttpRest> {
        Self::from_credentials(&Credentials::load(paths))
    }

    pub fn with_auth(auth: TokenProvider) -> Result<HttpRest> {
        Ok(HttpRest {
            transport: Box::new(HttpTransport {
                client: client()?,
                base: API.to_string(),
            }),
            auth,
        })
    }

    /// Over an arbitrary transport. The seam the retry tests sit on.
    pub fn over(transport: Box<dyn Transport>, auth: TokenProvider) -> HttpRest {
        HttpRest { transport, auth }
    }

    /// The credential in force, for `doctor` and the git credential path.
    pub fn auth(&self) -> &TokenProvider {
        &self.auth
    }

    fn request(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value> {
        // A 401 against an App is usually a token that went stale under us — an
        // installation token lives an hour and the world can change inside it.
        // So: drop what we cached, mint once more, and try again. Exactly once.
        // A genuinely revoked installation answers 401 to the fresh token too,
        // and a board that kept re-minting through that would spin against
        // GitHub forever rather than fail and be fixed. A personal access token
        // never takes this path at all — `invalidate` has nothing to drop, so
        // its 401 is final, as it always was.
        let mut re_minted = false;
        loop {
            let bearer = self.auth.bearer(path)?;
            let reply = self
                .transport
                .send(method.clone(), path, body, bearer.as_deref())?;
            if reply.status == 401 && !re_minted && self.auth.invalidate(path) {
                re_minted = true;
                continue;
            }
            return Self::interpret(reply, path, &self.auth);
        }
    }

    fn interpret(reply: Reply, path: &str, auth: &TokenProvider) -> Result<Value> {
        if reply.status == 403 || reply.status == 429 {
            bail!("github rate limited ({})", reply.status);
        }
        if reply.status == 401 {
            bail!("github rejected the credential (401) — {}", auth.hint());
        }
        if !(200..300).contains(&reply.status) {
            bail!("github HTTP {} for {path}", reply.status);
        }
        Ok(reply.body)
    }
}

impl Rest for HttpRest {
    fn get(&self, path: &str) -> Result<Value> {
        self.request(reqwest::Method::GET, path, None)
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.request(reqwest::Method::POST, path, Some(body))
    }

    fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.request(reqwest::Method::PATCH, path, Some(body))
    }

    fn put(&self, path: &str, body: &Value) -> Result<Value> {
        self.request(reqwest::Method::PUT, path, Some(body))
    }
}

#[derive(Debug, Clone)]
pub struct GithubIssue {
    pub repo: String,
    pub number: i64,
    pub node_id: String,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    pub labels: Vec<String>,
    pub closed: bool,
    pub updated_at: String,
}

impl GithubIssue {
    pub fn task_id(&self) -> String {
        format!("gh:{}#{}", self.repo, self.number)
    }

    /// Display identifier, per the design spec's 8-cell id column.
    pub fn identifier(&self) -> String {
        format!("gh#{}", self.number)
    }

    pub fn to_upsert(&self) -> UpsertTask {
        UpsertTask {
            id: self.task_id(),
            source: Source::Github,
            source_id: self.node_id.clone(),
            identifier: self.identifier(),
            title: self.title.clone(),
            body: self.body.clone(),
            url: self.url.clone(),
            labels: self.labels.clone(),
            source_state: Some(if self.closed { "closed" } else { "open" }.into()),
            linear_team: None,
            linear_project: None,
            upstream: if self.closed {
                UpstreamState::Terminal
            } else {
                UpstreamState::Unstarted
            },
            updated_at: self.updated_at.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    pub head_ref: String,
    pub open: bool,
    /// Merged, as opposed to closed without merging — a real difference to
    /// whoever has to decide what happens next.
    pub merged: bool,
    pub draft: bool,
    pub updated_at: String,
}

impl PullRequest {
    /// `!` rather than `#` so a PR and an issue of the same number in one repo
    /// stay distinct rows.
    pub fn task_id(&self) -> String {
        format!("gh:{}!{}", self.repo, self.number)
    }

    /// Fits the design's 8-cell id column.
    pub fn identifier(&self) -> String {
        format!("gh!{}", self.number)
    }

    /// An open pull request is work waiting on a human, which is exactly what
    /// the board calls `review`; a closed or merged one is `done`.
    pub fn to_upsert(&self) -> UpsertTask {
        UpsertTask {
            id: self.task_id(),
            source: Source::Github,
            source_id: self.url.clone(),
            identifier: self.identifier(),
            title: self.title.clone(),
            body: self.body.clone(),
            url: self.url.clone(),
            labels: Vec::new(),
            source_state: Some(
                if !self.open {
                    "closed"
                } else if self.draft {
                    "draft"
                } else {
                    "open"
                }
                .into(),
            ),
            linear_team: None,
            linear_project: None,
            upstream: if self.open {
                UpstreamState::Unstarted
            } else {
                UpstreamState::Terminal
            },
            updated_at: self.updated_at.clone(),
        }
    }
}

/// Where a piece of review feedback came from.
///
/// Three endpoints, not one. An issue comment on the pull request is the
/// obvious channel and the only one the board already talks to; an inline
/// comment hangs off a line of the diff; and a review *submission* carries the
/// verdict — `changes requested` is the most actionable signal there is and is
/// not an issue comment at all. Each has its own id sequence, so a watermark
/// has to be kept per kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKind {
    /// `/issues/{n}/comments` — the pull request's conversation.
    Issue,
    /// `/pulls/{n}/comments` — anchored to a file and line.
    Inline,
    /// `/pulls/{n}/reviews` — a submission with a verdict.
    Review,
}

impl FeedbackKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FeedbackKind::Issue => "comment",
            FeedbackKind::Inline => "inline comment",
            FeedbackKind::Review => "review",
        }
    }
}

/// One comment or review on a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feedback {
    pub kind: FeedbackKind,
    /// Unique within its own endpoint, and increasing — which is what makes it
    /// usable as a watermark.
    pub id: i64,
    pub body: String,
    pub url: String,
    pub created_at: String,
    pub author: String,
    /// Inline comments only: the file the comment is anchored to.
    pub path: Option<String>,
    /// Inline comments only. GitHub reports `line` against the current diff and
    /// `original_line` against the commit that was reviewed; a comment on a
    /// line that has since moved keeps only the latter.
    pub line: Option<i64>,
    /// Review submissions only: `changes_requested`, `approved`, `commented`.
    pub state: Option<String>,
}

impl Feedback {
    /// A reviewer asked for changes. The single most actionable thing that can
    /// happen to a pull request, and worth saying out loud on delivery.
    pub fn requests_changes(&self) -> bool {
        self.state.as_deref() == Some("changes_requested")
    }
}

/// What onboarding needs to know about a repo before it clones it (gh#97).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoFacts {
    /// `owner/repo` as GitHub spells it — the canonical casing.
    pub slug: String,
    pub clone_url: String,
    pub default_branch: String,
    pub private: bool,
    pub archived: bool,
    /// Issues can be switched off on a repo, and then polling it is polling
    /// nothing. Worth a line in the report rather than a silent empty board.
    pub has_issues: bool,
}

/// One GitHub account, as much of it as `[users]` needs (gh#162).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GithubUser {
    /// The numeric id — the left half of the noreply address.
    pub id: i64,
    /// The login, spelled the way the account spells it.
    pub login: String,
}

pub struct Github<T: Rest> {
    pub rest: T,
}

impl<T: Rest> Github<T> {
    pub fn new(rest: T) -> Github<T> {
        Github { rest }
    }

    /// The account behind a login, for the one fact `[users]` needs (gh#162):
    /// the numeric id, which is the half of
    /// `<id>+<login>@users.noreply.github.com` nobody can type from memory.
    ///
    /// Public data, so any credential the board holds reaches it — but the
    /// board's App is the one that will be holding it on the box, which is why
    /// this is asked *there* rather than from the laptop running `member add`.
    /// A login GitHub has never heard of answers 404, and the caller says what
    /// to do about that: the noreply address can always be pasted instead.
    pub fn user(&self, login: &str) -> Result<GithubUser> {
        let v = self.rest.get(&format!("/users/{login}"))?;
        let id = v
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("github's reply for `{login}` carried no account id"))?;
        Ok(GithubUser {
            // GitHub's own casing, for the same reason `repo` takes it: the
            // address is minted from the login as the account spells it.
            login: v
                .get("login")
                .and_then(Value::as_str)
                .unwrap_or(login)
                .to_string(),
            id,
        })
    }

    /// What GitHub says about one repo, under whatever credential is in force
    /// (gh#97).
    ///
    /// The onboarding gate. A repo the board's credential cannot see answers 404
    /// here — GitHub does not distinguish "no such repo" from "not yours", and
    /// neither can we — so the *caller* supplies what to say about it, because
    /// only it knows whether the credential is an App (somebody has to install
    /// it) or a personal access token (somebody has to widen its scopes).
    ///
    /// Called before anything is cloned: a clone that fails halfway leaves a
    /// directory behind, and the answer was available for one round trip.
    pub fn repo(&self, repo: &str) -> Result<RepoFacts> {
        let v = self.rest.get(&format!("/repos/{repo}"))?;
        Ok(RepoFacts {
            // GitHub's own casing, not the caller's: `Florin-AS/Tripletex-MCP`
            // typed as `florin-as/tripletex-mcp` should land in routing.toml the
            // way the repo is actually spelled.
            slug: v
                .get("full_name")
                .and_then(Value::as_str)
                .unwrap_or(repo)
                .to_string(),
            clone_url: v
                .get("clone_url")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("https://github.com/{repo}.git")),
            default_branch: v
                .get("default_branch")
                .and_then(Value::as_str)
                .unwrap_or("main")
                .to_string(),
            private: v.get("private").and_then(Value::as_bool).unwrap_or(false),
            archived: v.get("archived").and_then(Value::as_bool).unwrap_or(false),
            has_issues: v.get("has_issues").and_then(Value::as_bool).unwrap_or(true),
        })
    }

    /// Issues for a repo. GitHub's issues endpoint also returns pull requests;
    /// those carry a `pull_request` key and are filtered out here.
    ///
    /// **Paginated, and it has to be.** This endpoint returns issues *and* pull
    /// requests, the PRs are discarded only after a page is cut, and it sorts by
    /// `created` — so every pull request the board opens is newer than every
    /// issue and pushes the oldest issues off the first page. On
    /// bredebjorhovd/itsm-agent after 30 dispatches: 100 items on page one, 44
    /// of them pull requests, 56 issues surviving, and 29 items on a second page
    /// nobody fetched. Seven open issues were absent, and because a task with no
    /// attempts is *deleted* when reaped, they vanished from the board with no
    /// `gone` flag and no error — `dispatch` just answered `no task` (gh#31).
    pub fn issues(&self, repo: &str, labels: &[String]) -> Result<Vec<GithubIssue>> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut path = format!("/repos/{repo}/issues?state=all&per_page={PAGE}&page={page}");
            if !labels.is_empty() {
                path.push_str(&format!("&labels={}", labels.join(",")));
            }
            // Length *before* the pull-request filter: a short page is what says
            // the sequence is exhausted, and filtering first would make a page
            // full of PRs look like the end of the issues.
            let (issues, raw) = self.issue_page_counted(repo, &path)?;
            out.extend(issues);
            if raw < PAGE {
                return Ok(out);
            }
        }
        // Ran out of pages before the API ran out of issues. Reaping on this
        // would delete whatever is beyond it, so say so rather than returning a
        // set that looks complete.
        bail!(
            "{repo} has more than {} issues and pull requests; refusing to \
             report a partial set",
            PAGE * MAX_PAGES
        )
    }

    /// Open issues only, unfiltered — what adopting a repo unfiltered would put
    /// on the board, before it is put there.
    ///
    /// Deliberately not [`Github::issues`]: that asks for `state=all`, so on a
    /// repo with years of closed issues the page would be mostly history and
    /// the count would say nothing about what is about to arrive.
    pub fn open_issues(&self, repo: &str) -> Result<Vec<GithubIssue>> {
        self.issue_page(
            repo,
            &format!("/repos/{repo}/issues?state=open&per_page={PAGE}"),
        )
    }

    fn issue_page(&self, repo: &str, path: &str) -> Result<Vec<GithubIssue>> {
        Ok(self.issue_page_counted(repo, path)?.0)
    }

    /// One page, plus how many items the API actually returned.
    ///
    /// The raw count is the only thing that says whether there is another page.
    /// The parsed count cannot: this endpoint mixes pull requests in with the
    /// issues, so a full page can yield very few issues and still have more
    /// behind it.
    fn issue_page_counted(&self, repo: &str, path: &str) -> Result<(Vec<GithubIssue>, usize)> {
        let v = self.rest.get(path)?;
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow!("github issues: expected an array"))?;
        let issues = arr
            .iter()
            .filter(|n| n.get("pull_request").is_none())
            .filter_map(|n| parse_issue(repo, n))
            .collect();
        Ok((issues, arr.len()))
    }

    /// Leave the same trail as Linear gets.
    pub fn comment(&self, repo: &str, number: i64, body: &str) -> Result<()> {
        self.rest.post(
            &format!("/repos/{repo}/issues/{number}/comments"),
            &serde_json::json!({ "body": body }),
        )?;
        Ok(())
    }

    /// Open an issue.
    pub fn create_issue(
        &self,
        repo: &str,
        title: &str,
        body: Option<&str>,
        labels: &[String],
    ) -> Result<(i64, String)> {
        let r = self.rest.post(
            &format!("/repos/{repo}/issues"),
            &serde_json::json!({ "title": title, "body": body, "labels": labels }),
        )?;
        let number = r
            .get("number")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("github issueCreate returned no number"))?;
        let url = r
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok((number, url))
    }

    /// Submit a review on a pull request (§gh#239) — a verdict and a body, as
    /// one submission rather than a loose comment.
    ///
    /// Answers with the id GitHub assigned, which is the whole reason this is
    /// not [`Github::comment`]: that id is what the caller watermarks so the
    /// board's own verdict can never be read back off the reviews endpoint and
    /// relayed into the chat it was just delivered into. `0` when GitHub
    /// answered without one — the caller's fallback is documented at
    /// [`crate::verdict::POSTED_MARK`], and it is a fallback rather than a
    /// failure because the review itself did land.
    pub fn post_review(&self, repo: &str, number: i64, event: &str, body: &str) -> Result<i64> {
        let r = self.rest.post(
            &format!("/repos/{repo}/pulls/{number}/reviews"),
            &serde_json::json!({ "event": event, "body": body }),
        )?;
        Ok(r.get("id").and_then(Value::as_i64).unwrap_or_default())
    }

    /// Whether a pull request can merge as it stands.
    ///
    /// The list endpoint does not carry this, so it is one call per open PR —
    /// worth it only on a full sweep, not every poll.
    ///
    /// GitHub answers `clean`, `behind` (master moved under it), `dirty`
    /// (conflicts), `blocked` (a required check or review), `unstable` (a
    /// non-required check failing), or `unknown` while it recomputes.
    pub fn mergeable_state(&self, repo: &str, number: i64) -> Option<String> {
        self.rest
            .get(&format!("/repos/{repo}/pulls/{number}"))
            .ok()?
            .get("mergeable_state")?
            .as_str()
            .map(str::to_string)
            .filter(|s| s != "unknown")
    }

    /// The commit a branch points at on GitHub, or `None` if there is no such
    /// branch (gh#69).
    ///
    /// `None` covers "GitHub says there is no such branch" and "GitHub could
    /// not be asked" alike, the way [`Github::mergeable_state`] does, and for
    /// the same reason: the one caller — the settle's push check — treats an
    /// unproven push as an unpushed one, so an outage costs an attempt that
    /// stays live a while longer rather than a row that claims work is
    /// reviewable. The branch is a path segment and GitHub takes slashes in it
    /// verbatim, which is what a `board/gh-69-comet-board` name needs.
    pub fn branch_head(&self, repo: &str, branch: &str) -> Option<String> {
        self.rest
            .get(&format!("/repos/{repo}/branches/{branch}"))
            .ok()?
            .get("commit")?
            .get("sha")?
            .as_str()
            .map(str::to_string)
    }

    /// Merge a pull request.
    ///
    /// Only ever from an explicit keypress with a confirmation — this is the
    /// one action the board takes that cannot be undone from the board.
    pub fn merge_pr(&self, repo: &str, number: i64) -> Result<()> {
        let r = self.rest.put(
            &format!("/repos/{repo}/pulls/{number}/merge"),
            &serde_json::json!({ "merge_method": "merge" }),
        )?;
        // GitHub answers 200 with `merged: false` when it declines — a dirty
        // mergeable_state, a required check, a review still outstanding.
        if r.get("merged").and_then(Value::as_bool) == Some(false) {
            bail!(
                "github refused the merge: {}",
                r.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given")
            );
        }
        Ok(())
    }

    /// Close on done. Without this, `d mark done` moves the row and the next
    /// poll recomputes `open` upstream and moves it straight back — a key that
    /// undoes itself.
    pub fn close_issue(&self, repo: &str, number: i64) -> Result<()> {
        self.rest.patch(
            &format!("/repos/{repo}/issues/{number}"),
            &serde_json::json!({ "state": "closed" }),
        )?;
        Ok(())
    }

    /// Everything anybody has said about one pull request.
    ///
    /// Three calls, so a caller has to have a reason before asking: the pull
    /// request list already reports `updated_at`, and only a pull request whose
    /// timestamp has moved can have anything new on it.
    ///
    /// Returned in the order it was written, across all three endpoints, so a
    /// reader gets the conversation rather than three piles of it.
    pub fn pr_feedback(&self, repo: &str, number: i64) -> Result<Vec<Feedback>> {
        let mut out = Vec::new();
        for (kind, path) in [
            (
                FeedbackKind::Issue,
                format!("/repos/{repo}/issues/{number}/comments?per_page={PAGE}"),
            ),
            (
                FeedbackKind::Inline,
                format!("/repos/{repo}/pulls/{number}/comments?per_page={PAGE}"),
            ),
            (
                FeedbackKind::Review,
                format!("/repos/{repo}/pulls/{number}/reviews?per_page={PAGE}"),
            ),
        ] {
            let v = self.rest.get(&path)?;
            let arr = v
                .as_array()
                .ok_or_else(|| anyhow!("github {path}: expected an array"))?;
            out.extend(arr.iter().filter_map(|n| parse_feedback(kind, n)));
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    pub fn pulls(&self, repo: &str) -> Result<Vec<PullRequest>> {
        let v = self
            .rest
            .get(&format!("/repos/{repo}/pulls?state=all&per_page=100"))?;
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow!("github pulls: expected an array"))?;
        Ok(arr.iter().filter_map(|n| parse_pull(repo, n)).collect())
    }
}

fn parse_issue(repo: &str, n: &Value) -> Option<GithubIssue> {
    Some(GithubIssue {
        repo: repo.to_string(),
        number: n.get("number")?.as_i64()?,
        node_id: n
            .get("node_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: n
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        body: n.get("body").and_then(Value::as_str).map(str::to_string),
        url: n
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        labels: n
            .get("labels")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        closed: n.get("state").and_then(Value::as_str) == Some("closed"),
        updated_at: n
            .get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn parse_feedback(kind: FeedbackKind, n: &Value) -> Option<Feedback> {
    let state = n
        .get("state")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    if kind == FeedbackKind::Review {
        // `pending` is a review its author has not submitted — GitHub only
        // returns your own, and it is not feedback until it is sent. `dismissed`
        // is a verdict somebody explicitly withdrew.
        if matches!(state.as_deref(), Some("pending") | Some("dismissed")) {
            return None;
        }
    }
    Some(Feedback {
        kind,
        id: n.get("id")?.as_i64()?,
        body: n
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        url: n
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        // A review carries `submitted_at`; the two comment endpoints carry
        // `created_at`. Same fact under two names.
        created_at: n
            .get("created_at")
            .or_else(|| n.get("submitted_at"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        author: n
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        path: n.get("path").and_then(Value::as_str).map(str::to_string),
        // `line` is null once the diff has moved under the comment; the line it
        // was written against still says where to look.
        line: n
            .get("line")
            .and_then(Value::as_i64)
            .or_else(|| n.get("original_line").and_then(Value::as_i64)),
        state: (kind == FeedbackKind::Review).then_some(state).flatten(),
    })
}

fn parse_pull(repo: &str, n: &Value) -> Option<PullRequest> {
    Some(PullRequest {
        repo: repo.to_string(),
        number: n.get("number")?.as_i64()?,
        title: n
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        body: n.get("body").and_then(Value::as_str).map(str::to_string),
        merged: n.get("merged_at").is_some_and(|v| !v.is_null()),
        draft: n.get("draft").and_then(Value::as_bool).unwrap_or(false),
        updated_at: n
            .get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        url: n
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        head_ref: n
            .get("head")
            .and_then(|h| h.get("ref"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        open: n.get("state").and_then(Value::as_str) == Some("open"),
    })
}

/// Does this PR belong to that attempt? Branch match is the primary link
/// (impl spec §4.2); a PR URL recorded on the Linear issue is the other.
pub fn pr_matches_branch(pr: &PullRequest, branch: &str) -> bool {
    !branch.is_empty() && pr.head_ref == branch
}

/// Replays recorded responses keyed by path prefix. Used in tests.
#[cfg(test)]
pub struct FixtureRest {
    pub routes: Vec<(String, Value)>,
    pub asked: std::cell::RefCell<Vec<String>>,
    pub wrote: std::cell::RefCell<Vec<(String, String, Value)>>,
}

#[cfg(test)]
impl FixtureRest {
    pub fn new(routes: Vec<(String, Value)>) -> FixtureRest {
        FixtureRest {
            routes,
            asked: std::cell::RefCell::new(Vec::new()),
            wrote: std::cell::RefCell::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl Rest for FixtureRest {
    fn get(&self, path: &str) -> Result<Value> {
        self.asked.borrow_mut().push(path.to_string());
        self.routes
            .iter()
            .find(|(p, _)| path.starts_with(p.as_str()))
            .map(|(_, v)| v.clone())
            .ok_or_else(|| anyhow!("no fixture for {path}"))
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.wrote
            .borrow_mut()
            .push(("POST".into(), path.into(), body.clone()));
        // A canned reply when the test recorded one under `POST <path>` — a
        // review post reads the id back out of it. Keyed with the verb because
        // several endpoints here answer a GET and a POST at the same path, and
        // a list of reviews is not the review you just wrote. Null otherwise,
        // which is what every write whose answer nobody looks at has always
        // got.
        let key = format!("POST {path}");
        Ok(self
            .routes
            .iter()
            .find(|(p, _)| key.starts_with(p.as_str()))
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Null))
    }

    fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.wrote
            .borrow_mut()
            .push(("PATCH".into(), path.into(), body.clone()));
        Ok(Value::Null)
    }

    fn put(&self, path: &str, body: &Value) -> Result<Value> {
        self.wrote
            .borrow_mut()
            .push(("PUT".into(), path.into(), body.clone()));
        Ok(serde_json::json!({ "merged": true }))
    }
}

/// Records every round trip and answers from a canned script of statuses. The
/// seam the re-mint rule is tested on: a fixture-level fake cannot produce a
/// 401, because a 401 is a fact about the wire and not about an endpoint.
#[cfg(test)]
struct ScriptedTransport {
    /// One status per call, in order; the last repeats once exhausted.
    statuses: Vec<u16>,
    /// `(path, bearer)` for every call made.
    seen: std::cell::RefCell<Vec<(String, Option<String>)>>,
}

#[cfg(test)]
impl ScriptedTransport {
    fn new(statuses: &[u16]) -> ScriptedTransport {
        ScriptedTransport {
            statuses: statuses.to_vec(),
            seen: std::cell::RefCell::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl Transport for std::rc::Rc<ScriptedTransport> {
    fn send(
        &self,
        _method: reqwest::Method,
        path: &str,
        _body: Option<&Value>,
        bearer: Option<&str>,
    ) -> Result<Reply> {
        let n = self.seen.borrow().len();
        self.seen
            .borrow_mut()
            .push((path.to_string(), bearer.map(str::to_string)));
        let status = *self
            .statuses
            .get(n)
            .or_else(|| self.statuses.last())
            .unwrap_or(&200);
        Ok(Reply {
            status,
            body: serde_json::json!([]),
        })
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use crate::sources::github_app::test_app;
    use std::rc::Rc;

    fn scripted(statuses: &[u16], auth: TokenProvider) -> (HttpRest, Rc<ScriptedTransport>) {
        let t = Rc::new(ScriptedTransport::new(statuses));
        (HttpRest::over(Box::new(t.clone()), auth), t)
    }

    #[test]
    fn a_401_under_an_app_re_mints_once_and_retries() {
        // An installation token lives an hour; the world can change inside one.
        // A stale token is worth exactly one re-mint before believing GitHub.
        let (app, api, _) = test_app(&[("o/r", 42)]);
        let (rest, wire) = scripted(&[401, 200], TokenProvider::App(app));
        rest.get("/repos/o/r/issues").unwrap();

        let seen = wire.seen.borrow();
        assert_eq!(seen.len(), 2, "one retry, not none");
        assert_eq!(seen[0].1.as_deref(), Some("ghs_token_1"));
        assert_eq!(
            seen[1].1.as_deref(),
            Some("ghs_token_2"),
            "the retry must carry a freshly minted token, not the refused one"
        );
        assert_eq!(api.mints(), 2);
    }

    #[test]
    fn a_revoked_installation_gives_up_after_one_re_mint_rather_than_spinning() {
        // The failure mode this guards: an uninstalled App answering 401 to
        // every token it can mint, and a board minting forever against it.
        let (app, api, _) = test_app(&[("o/r", 42)]);
        let (rest, wire) = scripted(&[401, 401, 401], TokenProvider::App(app));
        let err = rest.get("/repos/o/r/issues").unwrap_err().to_string();

        assert_eq!(wire.seen.borrow().len(), 2, "exactly one retry");
        assert_eq!(api.mints(), 2, "exactly one re-mint");
        assert!(err.contains("401"), "{err}");
        assert!(err.contains("installation may have been removed"), "{err}");
    }

    #[test]
    fn a_personal_access_token_does_not_retry_a_401() {
        // Byte-for-byte the old behaviour: a PAT's 401 means the token is wrong,
        // not stale, and retrying it is a wasted call and a wasted rate limit.
        let (rest, wire) = scripted(&[401, 200], TokenProvider::Static("ghp_x".into()));
        let err = rest.get("/repos/o/r/issues").unwrap_err().to_string();
        assert_eq!(wire.seen.borrow().len(), 1);
        assert!(err.contains("check GITHUB_TOKEN scopes"), "{err}");
    }

    #[test]
    fn a_personal_access_token_stamps_the_same_bearer_on_every_path() {
        let (rest, wire) = scripted(&[200], TokenProvider::Static("ghp_x".into()));
        rest.get("/repos/o/r/issues").unwrap();
        rest.get("/repos/other/repo/pulls").unwrap();
        let seen = wire.seen.borrow();
        assert!(seen.iter().all(|(_, b)| b.as_deref() == Some("ghp_x")));
    }

    #[test]
    fn several_repos_behind_one_installation_share_one_minted_token() {
        // The rate-limit consequence of keying the cache on the installation:
        // the board polls six repos a cycle, and this is why that is one mint.
        let (app, api, _) = test_app(&[("o/a", 7), ("o/b", 7), ("o/c", 7)]);
        let (rest, wire) = scripted(&[200], TokenProvider::App(app));
        for repo in ["o/a", "o/b", "o/c"] {
            rest.get(&format!("/repos/{repo}/issues")).unwrap();
        }
        assert_eq!(api.mints(), 1);
        let seen = wire.seen.borrow();
        assert!(
            seen.iter()
                .all(|(_, b)| b.as_deref() == Some("ghs_token_1"))
        );
    }

    #[test]
    fn an_anonymous_client_sends_no_authorization_header_at_all() {
        let (rest, wire) = scripted(&[200], TokenProvider::Anonymous);
        rest.get("/repos/o/r/issues").unwrap();
        assert_eq!(wire.seen.borrow()[0].1, None);
    }

    #[test]
    fn rate_limiting_still_reads_as_rate_limiting_rather_than_as_auth() {
        let (rest, _) = scripted(&[403], TokenProvider::Static("ghp_x".into()));
        let err = rest.get("/repos/o/r/issues").unwrap_err().to_string();
        assert!(err.contains("rate limited"), "{err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pull_requests_are_filtered_out_of_issues() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/issues".into(),
            json!([
                { "number": 87, "node_id": "n1", "title": "Bug", "html_url": "u",
                  "state": "open", "updated_at": "t", "labels": [{"name":"herd"}] },
                { "number": 88, "node_id": "n2", "title": "A PR", "html_url": "u2",
                  "state": "open", "updated_at": "t", "pull_request": { "url": "x" } }
            ]),
        )]));
        let issues = g.issues("o/r", &[]).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 87);
        assert_eq!(issues[0].task_id(), "gh:o/r#87");
        assert_eq!(issues[0].identifier(), "gh#87");
    }

    #[test]
    fn a_full_page_of_pull_requests_does_not_end_the_issue_walk() {
        // gh#31, measured on bredebjorhovd/itsm-agent: page one held 100 items,
        // 44 of them pull requests, leaving 56 issues — and 29 items sat on a
        // page nobody fetched. This endpoint sorts by `created`, so every PR the
        // board opens is newer than every issue and pushes the oldest issues off
        // page one. Seven open issues were absent, and a reaped task with no
        // attempts is *deleted*, so they left the board with no `gone` flag and
        // no error.
        //
        // The walk must key on the raw item count, not the issue count: a page
        // that is all pull requests yields zero issues and still has more behind
        // it.
        let mut page1: Vec<serde_json::Value> = (0..PAGE)
            .map(|i| {
                json!({ "number": 1000 + i, "node_id": "p", "title": "a pr",
                        "html_url": "u", "state": "open", "updated_at": "t",
                        "pull_request": { "url": "x" } })
            })
            .collect();
        page1.truncate(PAGE);
        let g = Github::new(FixtureRest::new(vec![
            (
                "/repos/o/r/issues?state=all&per_page=100&page=1".into(),
                json!(page1),
            ),
            (
                "/repos/o/r/issues?state=all&per_page=100&page=2".into(),
                json!([{ "number": 3, "node_id": "n", "title": "the old issue",
                         "html_url": "u", "state": "open", "updated_at": "t",
                         "labels": [] }]),
            ),
        ]));
        let issues = g.issues("o/r", &[]).unwrap();
        assert_eq!(
            issues.len(),
            1,
            "the issue on page two must be found even though page one yielded none"
        );
        assert_eq!(issues[0].number, 3);
    }

    #[test]
    fn an_unwalkable_repo_errors_rather_than_reporting_a_partial_set() {
        // Reaping deletes what is not in the returned set, so a set that might be
        // incomplete must not be returned as if it were complete. The caller
        // treats an error as "do not reap"; a short set would be treated as truth.
        let full: Vec<serde_json::Value> = (0..PAGE)
            .map(|i| {
                json!({ "number": 1000 + i, "node_id": "p", "title": "a pr",
                        "html_url": "u", "state": "open", "updated_at": "t",
                        "pull_request": { "url": "x" } })
            })
            .collect();
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/issues".into(),
            json!(full),
        )]));
        let err = g.issues("o/r", &[]).unwrap_err().to_string();
        assert!(err.contains("refusing to report a partial set"), "{err}");
    }

    #[test]
    fn closed_issues_map_to_terminal_upstream() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/issues".into(),
            json!([{ "number": 87, "node_id": "n1", "title": "Bug", "html_url": "u",
                     "state": "closed", "updated_at": "t", "labels": [] }]),
        )]));
        let up = g.issues("o/r", &[]).unwrap()[0].to_upsert();
        assert_eq!(up.upstream, UpstreamState::Terminal);
        assert_eq!(up.source_state.as_deref(), Some("closed"));
    }

    #[test]
    fn label_filter_reaches_the_query_string() {
        let g = Github::new(FixtureRest::new(vec![("/repos".into(), json!([]))]));
        g.issues("o/r", &["herd".into(), "bug".into()]).unwrap();
        assert!(g.rest.asked.borrow()[0].contains("labels=herd,bug"));
    }

    #[test]
    fn an_open_pull_request_becomes_a_review_row() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/pulls".into(),
            json!([{ "number": 508, "title": "Fix the gate", "html_url": "u",
                     "state": "open", "updated_at": "t", "draft": false,
                     "head": { "ref": "fix/rls" } }]),
        )]));
        let pr = &g.pulls("o/r").unwrap()[0];
        assert_eq!(pr.task_id(), "gh:o/r!508");
        assert_eq!(pr.identifier(), "gh!508");
        let up = pr.to_upsert();
        // Open: not terminal, so derivation with pr_open reaches `review`.
        assert_eq!(up.upstream, UpstreamState::Unstarted);
        assert_eq!(up.source_state.as_deref(), Some("open"));
    }

    #[test]
    fn a_merged_pull_request_is_done() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/pulls".into(),
            json!([{ "number": 508, "title": "t", "html_url": "u",
                     "state": "closed", "updated_at": "t",
                     "head": { "ref": "fix/rls" } }]),
        )]));
        assert_eq!(
            g.pulls("o/r").unwrap()[0].to_upsert().upstream,
            UpstreamState::Terminal
        );
    }

    #[test]
    fn a_pull_request_id_cannot_collide_with_an_issue_id() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/pulls".into(),
            json!([{ "number": 87, "title": "t", "html_url": "u",
                     "state": "open", "updated_at": "t",
                     "head": { "ref": "x" } }]),
        )]));
        // Issue 87 is `gh:o/r#87`; PR 87 must not be the same row.
        assert_eq!(g.pulls("o/r").unwrap()[0].task_id(), "gh:o/r!87");
    }

    #[test]
    fn a_comment_posts_to_the_issue() {
        let g = Github::new(FixtureRest::new(vec![]));
        g.comment("o/r", 87, "Dispatched to herdr").unwrap();
        let w = g.rest.wrote.borrow();
        assert_eq!(w[0].0, "POST");
        assert_eq!(w[0].1, "/repos/o/r/issues/87/comments");
        assert_eq!(w[0].2["body"], "Dispatched to herdr");
    }

    #[test]
    fn closing_an_issue_patches_its_state() {
        let g = Github::new(FixtureRest::new(vec![]));
        g.close_issue("o/r", 87).unwrap();
        let w = g.rest.wrote.borrow();
        assert_eq!(w[0].0, "PATCH");
        assert_eq!(w[0].1, "/repos/o/r/issues/87");
        assert_eq!(w[0].2["state"], "closed");
    }

    #[test]
    fn merging_puts_to_the_merge_endpoint() {
        let g = Github::new(FixtureRest::new(vec![]));
        g.merge_pr("o/r", 508).unwrap();
        let w = g.rest.wrote.borrow();
        assert_eq!(w[0].0, "PUT");
        assert_eq!(w[0].1, "/repos/o/r/pulls/508/merge");
    }

    #[test]
    fn a_refused_merge_is_an_error_not_a_success() {
        // GitHub answers 200 with merged:false when a check or review blocks it.
        struct Refuses;
        impl Rest for Refuses {
            fn get(&self, _: &str) -> Result<Value> {
                Ok(Value::Null)
            }
            fn post(&self, _: &str, _: &Value) -> Result<Value> {
                Ok(Value::Null)
            }
            fn patch(&self, _: &str, _: &Value) -> Result<Value> {
                Ok(Value::Null)
            }
            fn put(&self, _: &str, _: &Value) -> Result<Value> {
                Ok(json!({ "merged": false, "message": "required status check is pending" }))
            }
        }
        let g = Github::new(Refuses);
        let err = g.merge_pr("o/r", 508).unwrap_err().to_string();
        assert!(err.contains("required status check"), "{err}");
    }

    fn reviewed_pr() -> Github<FixtureRest> {
        Github::new(FixtureRest::new(vec![
            (
                "/repos/o/r/issues/14/comments".into(),
                json!([{ "id": 41, "body": "Also check the watermark.",
                         "html_url": "https://github.com/o/r/pull/14#issuecomment-41",
                         "created_at": "2026-07-28T11:10:00Z",
                         "user": { "login": "bredebjorhovd" } }]),
            ),
            (
                "/repos/o/r/pulls/14/comments".into(),
                json!([{ "id": 77, "body": "This drops a human comment.",
                         "html_url": "https://github.com/o/r/pull/14#discussion_r77",
                         "created_at": "2026-07-28T11:05:00Z",
                         "path": "src/review.rs", "line": 88,
                         "user": { "login": "bredebjorhovd" } }]),
            ),
            (
                "/repos/o/r/pulls/14/reviews".into(),
                json!([
                    { "id": 900, "body": "Two things.", "state": "CHANGES_REQUESTED",
                      "html_url": "https://github.com/o/r/pull/14#pullrequestreview-900",
                      "submitted_at": "2026-07-28T11:00:00Z",
                      "user": { "login": "bredebjorhovd" } },
                    // Never submitted, so not feedback yet.
                    { "id": 901, "body": "draft", "state": "PENDING",
                      "submitted_at": "2026-07-28T11:20:00Z",
                      "user": { "login": "bredebjorhovd" } },
                    // Explicitly withdrawn.
                    { "id": 902, "body": "old", "state": "DISMISSED",
                      "submitted_at": "2026-07-28T11:21:00Z",
                      "user": { "login": "bredebjorhovd" } }
                ]),
            ),
        ]))
    }

    /// The three sources, in one conversation. A review submission carries the
    /// verdict and is not an issue comment at all, which is why asking only the
    /// endpoint the board already writes to would miss the loudest signal.
    #[test]
    fn feedback_comes_from_all_three_endpoints_in_the_order_it_was_written() {
        let g = reviewed_pr();
        let f = g.pr_feedback("o/r", 14).unwrap();
        assert_eq!(
            f.iter().map(|x| (x.kind, x.id)).collect::<Vec<_>>(),
            vec![
                (FeedbackKind::Review, 900),
                (FeedbackKind::Inline, 77),
                (FeedbackKind::Issue, 41),
            ]
        );
        assert!(f[0].requests_changes());
        assert_eq!(f[0].state.as_deref(), Some("changes_requested"));
        // Enough to act on without a lookup.
        assert_eq!(f[1].path.as_deref(), Some("src/review.rs"));
        assert_eq!(f[1].line, Some(88));
        assert_eq!(f[2].body, "Also check the watermark.");
    }

    #[test]
    fn unsubmitted_and_withdrawn_reviews_are_not_feedback() {
        let g = reviewed_pr();
        let f = g.pr_feedback("o/r", 14).unwrap();
        assert!(
            !f.iter().any(|x| x.id == 901 || x.id == 902),
            "pending and dismissed reviews must not reach a pane"
        );
    }

    /// GitHub nulls `line` once the diff has moved under a comment. The line it
    /// was written against still says where to look.
    #[test]
    fn an_inline_comment_on_moved_code_keeps_the_line_it_was_written_against() {
        let g = Github::new(FixtureRest::new(vec![
            ("/repos/o/r/issues/14/comments".into(), json!([])),
            (
                "/repos/o/r/pulls/14/comments".into(),
                json!([{ "id": 77, "body": "here", "created_at": "t",
                         "path": "src/sync.rs", "line": null, "original_line": 412,
                         "user": { "login": "b" } }]),
            ),
            ("/repos/o/r/pulls/14/reviews".into(), json!([])),
        ]));
        let f = g.pr_feedback("o/r", 14).unwrap();
        assert_eq!(f[0].line, Some(412));
    }

    #[test]
    fn a_pr_links_to_an_attempt_by_branch() {
        let g = Github::new(FixtureRest::new(vec![(
            "/repos/o/r/pulls".into(),
            json!([{ "number": 291, "title": "Add retry",
                     "html_url": "https://github.com/o/r/pull/291",
                     "state": "open", "updated_at": "t",
                     "head": { "ref": "board/lin-142" } }]),
        )]));
        let prs = g.pulls("o/r").unwrap();
        assert!(pr_matches_branch(&prs[0], "board/lin-142"));
        assert!(!pr_matches_branch(&prs[0], "board/lin-999"));
        // An attempt with no branch must never match everything.
        assert!(!pr_matches_branch(&prs[0], ""));
        assert!(prs[0].open);
    }
}
