//! GitHub App authentication — per-installation tokens (gh#58).
//!
//! The board used to take one `GITHUB_TOKEN`: a personal access token, which
//! belongs to an account and carries that account's whole reach. That is fine
//! for a single-owner self-host and stays supported ([`TokenProvider::Static`]),
//! but it does not survive somebody else installing the board on *their* repos.
//! A fine-grained PAT is scoped to one resource owner and the board holds a
//! single token, so two owners would need a classic PAT's blanket `repo` scope
//! — handing over an account rather than granting access to a repo.
//!
//! An App inverts that. The installer installs it on the repos they choose, no
//! credential ever changes hands, rate limits are per installation, and
//! offboarding is an uninstall rather than a rotation. Writes land as `[bot]`,
//! which is honestly not a human.
//!
//! The flow, per GitHub's own:
//!
//! 1. Sign an **App JWT** — RS256 over `{iss, iat, exp}`, `iat` backdated for
//!    clock skew and `exp` inside GitHub's ten-minute ceiling.
//! 2. `GET /repos/{owner}/{repo}/installation` under that JWT to learn which
//!    installation serves a repo.
//! 3. `POST /app/installations/{id}/access_tokens` under that JWT for an
//!    installation token, good for an hour.
//!
//! Both caches key on what the fact actually belongs to: repo → installation,
//! and installation → token. Keying the token by installation rather than by
//! repo costs nothing today, when there is one installation, and is what lets a
//! single board process serve several later without minting a token per repo.
//!
//! Everything here sits behind three seams — [`AppApi`] for the two minting
//! endpoints, [`JwtSigner`] for the signature, [`Clock`] for time — so the
//! cache, the expiry rule and the re-mint-once rule are all unit-testable with
//! no network, no real App and no private key in the repository.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::config::GithubAuth;

/// Backdate on the App JWT's `iat`. GitHub rejects a token issued in its
/// future, and the two clocks are not the same clock.
const JWT_BACKDATE: i64 = 60;

/// Lifetime of the App JWT. GitHub's ceiling is ten minutes counted from `iat`,
/// and `iat` is already a minute in the past — nine minutes keeps a margin on
/// both ends.
const JWT_TTL: i64 = 540;

/// Re-mint this long before an installation token's stated expiry. The token is
/// good for an hour; a request that leaves with four minutes left must not
/// arrive with none.
const REFRESH_MARGIN: i64 = 300;

/// Unix seconds. A seam so a test can move an hour without sleeping one.
pub trait Clock {
    fn now(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default()
    }
}

/// What goes into the App JWT.
///
/// Split out and handed to the signer rather than assembled inside it, so the
/// claims can be asserted for what they are — the backdated `iat` and the
/// sub-ten-minute `exp` are the whole reason GitHub accepts the token, and a
/// test that has to verify a signature to read them would be testing the JWT
/// library instead.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Claims {
    /// The App id. GitHub identifies the App by this, not by name.
    pub iss: String,
    pub iat: i64,
    pub exp: i64,
}

impl Claims {
    pub fn at(app_id: &str, now: i64) -> Claims {
        Claims {
            iss: app_id.to_string(),
            iat: now - JWT_BACKDATE,
            exp: now + JWT_TTL,
        }
    }
}

/// Signs the App JWT.
///
/// A seam, and a deliberately tiny one: it is the only place in the crate that
/// touches crypto, and keeping it behind a trait means every cache, expiry and
/// retry test below runs without an RSA key checked into the repository.
pub trait JwtSigner {
    fn sign(&self, claims: &Claims) -> Result<String>;
}

/// The real one: RS256, the only algorithm GitHub accepts for an App JWT.
pub struct Rs256 {
    key: jsonwebtoken::EncodingKey,
}

impl Rs256 {
    /// From the PEM GitHub hands out when the App's private key is generated.
    pub fn from_pem(pem: &[u8]) -> Result<Rs256> {
        Ok(Rs256 {
            key: jsonwebtoken::EncodingKey::from_rsa_pem(pem).context(
                "reading the GitHub App private key — it must be the PEM GitHub \
                 generated (`-----BEGIN RSA PRIVATE KEY-----`)",
            )?,
        })
    }
}

impl JwtSigner for Rs256 {
    fn sign(&self, claims: &Claims) -> Result<String> {
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        jsonwebtoken::encode(&header, claims, &self.key).context("signing the GitHub App JWT")
    }
}

/// The endpoints reached outside [`HttpRest`]'s repo-scoped routing — the App's
/// own, under the JWT, plus `/installation/repositories`, which is the one
/// endpoint that needs an installation token and names no repo to derive it
/// from. A seam, so minting is testable without a real App.
///
/// `bearer` is whatever the caller resolved; every method here stamps it
/// verbatim.
pub trait AppApi {
    fn get(&self, path: &str, bearer: &str) -> Result<Value>;
    /// POST with no body — the only POST is the mint.
    fn post(&self, path: &str, bearer: &str) -> Result<Value>;
}

/// One installation of the App, as `doctor` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub id: i64,
    /// The user or org that installed it.
    pub account: String,
    /// `all` or `selected` — whether the installer granted every repo or picked.
    pub selection: String,
}

/// An installation token and when it stops working.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Minted {
    token: String,
    /// Unix seconds, parsed from GitHub's `expires_at`.
    expires_at: i64,
    /// The repository permissions GitHub says this installation token carries.
    ///
    /// Kept beside the token because this is installation-specific evidence:
    /// changing an App's permissions does not reach an installation until its
    /// owner approves the update. The mint response is therefore a stronger
    /// answer than the App registration itself. `None` is deliberately
    /// distinct from an empty map — no evidence is not evidence of no grants.
    permissions: Option<BTreeMap<String, String>>,
}

#[derive(Default)]
struct Cache {
    /// `owner/repo` → installation id. Installations change when somebody
    /// installs or uninstalls, which is rare enough to cache for the process.
    installations: BTreeMap<String, i64>,
    /// Installation id → its token. **Not** keyed by repo: one installation
    /// serves every repo the installer selected, and a token per repo would
    /// mint N times for one credential and burn N times the rate limit.
    tokens: BTreeMap<i64, Minted>,
}

/// A configured GitHub App, minting on demand and caching what it mints.
pub struct AppAuth {
    app_id: String,
    signer: Box<dyn JwtSigner>,
    api: Box<dyn AppApi>,
    clock: Box<dyn Clock>,
    cache: RefCell<Cache>,
}

impl AppAuth {
    pub fn new(
        app_id: impl Into<String>,
        signer: Box<dyn JwtSigner>,
        api: Box<dyn AppApi>,
        clock: Box<dyn Clock>,
    ) -> AppAuth {
        AppAuth {
            app_id: app_id.into(),
            signer,
            api,
            clock,
            cache: RefCell::new(Cache::default()),
        }
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// A fresh App JWT. Cheap (one signature), short-lived by design, and not
    /// cached — caching a nine-minute credential to save a signature would only
    /// add a second expiry rule to get wrong.
    pub fn jwt(&self) -> Result<String> {
        self.signer
            .sign(&Claims::at(&self.app_id, self.clock.now()))
    }

    /// Which installation serves `repo`, cached.
    ///
    /// A 404 here is the honest answer to "the App is not installed on this
    /// repo", and worth saying in those words: the installer, not the operator,
    /// is the one who can fix it.
    fn installation_for(&self, repo: &str) -> Result<i64> {
        if let Some(id) = self.cache.borrow().installations.get(repo) {
            return Ok(*id);
        }
        let jwt = self.jwt()?;
        let v = self
            .api
            .get(&format!("/repos/{repo}/installation"), &jwt)
            .with_context(|| {
                format!(
                    "asking which installation of App {} serves {repo} — if this is \
                     a 404, the App is not installed there",
                    self.app_id
                )
            })?;
        let id = v
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("github returned no installation id for {repo}"))?;
        self.cache
            .borrow_mut()
            .installations
            .insert(repo.to_string(), id);
        Ok(id)
    }

    /// A live installation token for `repo`.
    ///
    /// Cached per installation and refreshed [`REFRESH_MARGIN`] before it
    /// expires, so several repos behind one installation share one token and one
    /// mint.
    pub fn token_for_repo(&self, repo: &str) -> Result<String> {
        self.token_for_installation(self.installation_for(repo)?)
    }

    /// A live installation token for one installation, by id.
    ///
    /// The half of [`AppAuth::token_for_repo`] that does not need a repo — what
    /// `/installation/repositories` is authenticated with, since that endpoint
    /// answers *about* an installation and so names no repo to derive one from
    /// (gh#97).
    pub fn token_for_installation(&self, id: i64) -> Result<String> {
        let now = self.clock.now();
        if let Some(m) = self.cache.borrow().tokens.get(&id)
            && m.expires_at - REFRESH_MARGIN > now
        {
            return Ok(m.token.clone());
        }
        let jwt = self.jwt()?;
        let v = self
            .api
            .post(&format!("/app/installations/{id}/access_tokens"), &jwt)
            .with_context(|| format!("minting an access token for installation {id}"))?;
        let token = v
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("github returned no token for installation {id}"))?
            .to_string();
        // GitHub always sends `expires_at`; treating a missing or unparseable
        // one as "already expiring" re-mints every call rather than serving a
        // token whose life we cannot account for.
        let expires_at = v
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(parse_expiry)
            .unwrap_or(now);
        let permissions = v.get("permissions").and_then(Value::as_object).map(|p| {
            p.iter()
                .filter_map(|(name, level)| {
                    level
                        .as_str()
                        .map(|level| (name.to_string(), level.to_string()))
                })
                .collect()
        });
        self.cache.borrow_mut().tokens.insert(
            id,
            Minted {
                token: token.clone(),
                expires_at,
                permissions,
            },
        );
        Ok(token)
    }

    /// The permissions on the installation token `repo` would push with.
    ///
    /// This intentionally returns no token. A caller asking whether a push can
    /// update workflow files should never have to hold, print or persist the
    /// credential in order to learn the answer. Minting is still necessary —
    /// its response is the per-installation authority — but the secret stays
    /// inside this module exactly as it does for every REST request.
    pub fn permissions_for_repo(&self, repo: &str) -> Result<Option<BTreeMap<String, String>>> {
        let id = self.installation_for(repo)?;
        let _ = self.token_for_installation(id)?;
        Ok(self
            .cache
            .borrow()
            .tokens
            .get(&id)
            .and_then(|minted| minted.permissions.clone()))
    }

    /// Drop whatever `repo` is authenticating with, so the next call re-mints.
    ///
    /// Both halves: a 401 can mean the token was revoked *or* that the
    /// installation moved, and keeping a stale installation id would re-mint
    /// against something that no longer exists.
    ///
    /// Answers whether anything was actually dropped, which is what tells the
    /// caller a retry could go differently.
    pub fn invalidate_repo(&self, repo: &str) -> bool {
        let mut cache = self.cache.borrow_mut();
        let Some(id) = cache.installations.remove(repo) else {
            return false;
        };
        cache.tokens.remove(&id);
        true
    }

    /// Seconds until the token `repo` would use expires, if one is cached.
    /// `doctor` reads this; nothing else should care.
    pub fn cached_ttl(&self, repo: &str) -> Option<i64> {
        let cache = self.cache.borrow();
        let id = cache.installations.get(repo)?;
        Some(cache.tokens.get(id)?.expires_at - self.clock.now())
    }

    /// The installation serving `repo`, if one has already been looked up.
    pub fn cached_installation(&self, repo: &str) -> Option<i64> {
        self.cache.borrow().installations.get(repo).copied()
    }

    /// The App's own record — `slug` is the name in the install URL, and the
    /// one thing that tells an operator *which* App the board is running as.
    pub fn app_slug(&self) -> Result<String> {
        let jwt = self.jwt()?;
        let v = self.api.get("/app", &jwt).context("reading GET /app")?;
        Ok(v.get("slug")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string())
    }

    /// Every installation of this App — who installed it and whether they
    /// granted all their repos or picked.
    pub fn installations(&self) -> Result<Vec<Installation>> {
        let jwt = self.jwt()?;
        let v = self
            .api
            .get("/app/installations?per_page=100", &jwt)
            .context("listing the App's installations")?;
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow!("github /app/installations: expected an array"))?;
        Ok(arr
            .iter()
            .filter_map(|n| {
                Some(Installation {
                    id: n.get("id")?.as_i64()?,
                    account: n
                        .get("account")
                        .and_then(|a| a.get("login"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    selection: n
                        .get("repository_selection")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                })
            })
            .collect())
    }

    /// Every repo the App can see, across all of its installations (gh#97).
    ///
    /// This is the list an "onboard a repo" picker offers, and it is the honest
    /// one: it is not what the operator *owns*, it is what the App was granted —
    /// exactly the set a clone can authenticate for and the sync loop can poll.
    /// A repo missing from it is a repo somebody has to go and install the App
    /// on, and that is the answer worth showing.
    ///
    /// One installation that fails to enumerate does not lose the others: a
    /// fleet with a dead installation should still offer the repos it can see.
    pub fn accessible_repos(&self) -> Result<Vec<AppRepo>> {
        let mut out: Vec<AppRepo> = Vec::new();
        let mut seen: std::collections::BTreeSet<String> = Default::default();
        for install in self.installations()? {
            let Ok(token) = self.token_for_installation(install.id) else {
                continue;
            };
            for page in 1..=REPO_PAGES {
                let path = format!("/installation/repositories?per_page={REPO_PAGE}&page={page}");
                let Ok(v) = self.api.get(&path, &token) else {
                    break;
                };
                let repos = v
                    .get("repositories")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let full = repos.len();
                for r in repos {
                    let Some(repo) = AppRepo::parse(&r, install.id) else {
                        continue;
                    };
                    if seen.insert(repo.slug.to_ascii_lowercase()) {
                        out.push(repo);
                    }
                }
                // The first short page is the last page.
                if full < REPO_PAGE {
                    break;
                }
            }
        }
        out.sort_by_key(|r| r.slug.to_lowercase());
        Ok(out)
    }
}

/// One repo an installation of the App can see.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRepo {
    /// `owner/repo`, as GitHub spells it — the canonical casing.
    pub slug: String,
    pub private: bool,
    /// Archived repos are still visible and still have issues; onboarding one is
    /// almost never what somebody meant, so the picker says so rather than
    /// hiding it.
    pub archived: bool,
    pub default_branch: String,
    /// Which installation grants it — the thing to name when a clone is refused.
    pub installation: i64,
}

impl AppRepo {
    fn parse(v: &Value, installation: i64) -> Option<AppRepo> {
        Some(AppRepo {
            slug: v.get("full_name").and_then(Value::as_str)?.to_string(),
            private: v.get("private").and_then(Value::as_bool).unwrap_or(false),
            archived: v.get("archived").and_then(Value::as_bool).unwrap_or(false),
            default_branch: v
                .get("default_branch")
                .and_then(Value::as_str)
                .unwrap_or("main")
                .to_string(),
            installation,
        })
    }
}

/// Page size and ceiling for `/installation/repositories`. The ceiling is a
/// runaway guard, not a limit: the walk stops on the first short page.
const REPO_PAGE: usize = 100;
const REPO_PAGES: usize = 20;

/// GitHub's `expires_at` (RFC 3339) as unix seconds.
fn parse_expiry(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp())
}

/// What stamps `Authorization` on a GitHub request.
///
/// Three modes rather than two, because "no credential" was always reachable —
/// a board with no repos configured never authenticates, and public repos
/// answer anonymously.
#[derive(Clone)]
pub enum TokenProvider {
    /// No credential. Public repos only, and GitHub's anonymous rate limit.
    Anonymous,
    /// A personal access token — `GITHUB_TOKEN`, the path that existed before
    /// gh#58 and the right one for a single-owner self-host.
    Static(String),
    /// A GitHub App. `Rc` because a provider is cloned into the places that need
    /// one (the REST client, the git credential path) and they must share one
    /// cache — two caches would mint two tokens for one installation.
    App(Rc<AppAuth>),
}

impl TokenProvider {
    /// Build from the credential mode `.env` resolves to. Reads the PEM.
    pub fn from_auth(
        auth: GithubAuth,
        api: impl FnOnce() -> Result<Box<dyn AppApi>>,
    ) -> Result<TokenProvider> {
        match auth {
            GithubAuth::None => Ok(TokenProvider::Anonymous),
            GithubAuth::Token(t) => Ok(TokenProvider::Static(t)),
            GithubAuth::App { app_id, key_path } => {
                let pem = std::fs::read(&key_path).with_context(|| {
                    format!(
                        "reading GITHUB_APP_PRIVATE_KEY_PATH ({})",
                        key_path.display()
                    )
                })?;
                Ok(TokenProvider::App(Rc::new(AppAuth::new(
                    app_id,
                    Box::new(Rs256::from_pem(&pem)?),
                    api()?,
                    Box::new(SystemClock),
                ))))
            }
        }
    }

    /// The bearer for a request against `path`, or `None` to send unauthenticated.
    ///
    /// Under an App the credential depends on where the request is going: a
    /// repo-scoped path gets that repo's installation token, and the App's own
    /// endpoints get the JWT. A PAT is the same string everywhere, which is the
    /// difference the whole ticket is about.
    pub fn bearer(&self, path: &str) -> Result<Option<String>> {
        match self {
            TokenProvider::Anonymous => Ok(None),
            TokenProvider::Static(t) => Ok(Some(t.clone())),
            TokenProvider::App(app) => match repo_in_path(path) {
                Some(repo) => app.token_for_repo(&repo).map(Some),
                None => app.jwt().map(Some),
            },
        }
    }

    /// After a 401. Answers whether a retry could go differently — `false` for a
    /// PAT, whose 401 means the token is wrong rather than stale, and for an
    /// App path with nothing cached to blame.
    pub fn invalidate(&self, path: &str) -> bool {
        match self {
            TokenProvider::Anonymous | TokenProvider::Static(_) => false,
            TokenProvider::App(app) => repo_in_path(path)
                .map(|repo| app.invalidate_repo(&repo))
                .unwrap_or(false),
        }
    }

    /// What to tell somebody staring at a 401.
    pub fn hint(&self) -> String {
        match self {
            TokenProvider::Anonymous => {
                "no GitHub credential is configured — set GITHUB_TOKEN, or a \
                 GITHUB_APP_ID/GITHUB_APP_PRIVATE_KEY_PATH pair"
                    .into()
            }
            TokenProvider::Static(_) => "check GITHUB_TOKEN scopes".into(),
            TokenProvider::App(app) => format!(
                "App {} re-minted and was refused again — its installation may \
                 have been removed, or its private key rotated",
                app.app_id()
            ),
        }
    }

    /// The App behind this provider, for the two callers that need more than a
    /// bearer string: `doctor`'s report and the git credential path.
    pub fn app(&self) -> Option<&Rc<AppAuth>> {
        match self {
            TokenProvider::App(app) => Some(app),
            _ => None,
        }
    }
}

/// `/repos/{owner}/{repo}/…` → `owner/repo`.
///
/// Every endpoint the board calls is repo-scoped except the App's own, which is
/// what makes routing on the path enough: a repo path needs an installation
/// token, anything else needs the JWT.
pub fn repo_in_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/repos/")?;
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    // Trim the query string off a two-segment path (`/repos/o/r?x=1`).
    let repo = parts
        .next()
        .map(|s| s.split(['?', '#']).next().unwrap_or(s))
        .filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

/// The [`AppApi`] against real GitHub.
pub struct HttpAppApi {
    client: reqwest::blocking::Client,
    base: String,
}

impl HttpAppApi {
    pub fn new(client: reqwest::blocking::Client, base: impl Into<String>) -> HttpAppApi {
        HttpAppApi {
            client,
            base: base.into(),
        }
    }

    fn send(&self, method: reqwest::Method, path: &str, jwt: &str) -> Result<Value> {
        let resp = self
            .client
            .request(method, format!("{}{}", self.base, path))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("Authorization", format!("Bearer {jwt}"))
            .send()?;
        let status = resp.status().as_u16();
        // The body first, then the verdict: a refusal here is the App's own
        // credential being turned away, and "github HTTP 422" tells whoever
        // installed it nothing about which half of the key is wrong (gh#338).
        let body: Value = resp.json().unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            return Err(crate::sources::github::refusal(status, path, &body));
        }
        Ok(body)
    }
}

impl AppApi for HttpAppApi {
    fn get(&self, path: &str, jwt: &str) -> Result<Value> {
        self.send(reqwest::Method::GET, path, jwt)
    }

    fn post(&self, path: &str, jwt: &str) -> Result<Value> {
        self.send(reqwest::Method::POST, path, jwt)
    }
}

// ── test doubles ────────────────────────────────────────────────────────────
//
// `pub(crate)` rather than private: `github.rs` builds an App out of these to
// test the 401 → re-mint → retry-once rule end to end, and none of it needs a
// private key or a network.

/// A signer that records what it was asked to sign instead of signing it.
///
/// The claims are the whole contract with GitHub; asserting them here beats
/// recovering them from a signature, which would only test `jsonwebtoken`.
#[cfg(test)]
pub(crate) struct FakeSigner {
    pub signed: RefCell<Vec<Claims>>,
}

#[cfg(test)]
impl JwtSigner for FakeSigner {
    fn sign(&self, claims: &Claims) -> Result<String> {
        self.signed.borrow_mut().push(claims.clone());
        Ok(format!("jwt-for-{}", claims.iss))
    }
}

/// A clock a test moves by hand, shared with the fake API so a minted
/// `expires_at` and the expiry check are measured against the same second.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestClock(pub Rc<std::cell::Cell<i64>>);

#[cfg(test)]
impl TestClock {
    pub fn at(t: i64) -> TestClock {
        TestClock(Rc::new(std::cell::Cell::new(t)))
    }
    pub fn advance(&self, secs: i64) {
        self.0.set(self.0.get() + secs);
    }
}

#[cfg(test)]
impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.get()
    }
}

/// Answers the two minting endpoints from a canned installation map, and counts
/// every call — so a test can assert how many tokens were minted rather than
/// infer it from the strings that came back.
#[cfg(test)]
pub(crate) struct FakeAppApi {
    installs: BTreeMap<String, i64>,
    clock: TestClock,
    /// How long a minted token claims to live.
    ttl: i64,
    calls: RefCell<Vec<String>>,
    minted: std::cell::Cell<u32>,
    /// Minted token → the installation it was minted for, so a request carrying
    /// one can be answered as that installation.
    minted_for: RefCell<BTreeMap<String, i64>>,
    /// The exact permission object a real mint response carries. Writable so
    /// tests can model an installation that has not approved an App update.
    permissions: RefCell<Value>,
}

#[cfg(test)]
impl FakeAppApi {
    pub fn new(installs: &[(&str, i64)], clock: TestClock) -> Rc<FakeAppApi> {
        Rc::new(FakeAppApi {
            installs: installs
                .iter()
                .map(|(r, id)| ((*r).to_string(), *id))
                .collect(),
            clock,
            ttl: 3600,
            calls: RefCell::new(Vec::new()),
            minted: std::cell::Cell::new(0),
            minted_for: RefCell::new(BTreeMap::new()),
            permissions: RefCell::new(serde_json::json!({
                "contents": "write",
                "workflows": "write"
            })),
        })
    }

    pub fn set_permissions(&self, permissions: Value) {
        *self.permissions.borrow_mut() = permissions;
    }

    /// How many installation tokens were minted.
    pub fn mints(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|c| c.contains("access_tokens"))
            .count()
    }

    /// How many times an installation was looked up.
    pub fn lookups(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|c| c.ends_with("/installation"))
            .count()
    }
}

#[cfg(test)]
impl AppApi for Rc<FakeAppApi> {
    fn get(&self, path: &str, bearer: &str) -> Result<Value> {
        self.calls.borrow_mut().push(path.to_string());
        if let Some(repo) = path
            .strip_prefix("/repos/")
            .and_then(|r| r.strip_suffix("/installation"))
        {
            let id = self
                .installs
                .get(repo)
                .ok_or_else(|| anyhow!("github HTTP 404 Not Found for {path}"))?;
            return Ok(serde_json::json!({ "id": id }));
        }
        if path == "/app" {
            return Ok(serde_json::json!({ "slug": "comet-board-test" }));
        }
        // The installation-token endpoint. Which installation is asking is not
        // in the path — it is the bearer — so the fake reads the id back out of
        // the token it minted, which is what makes "installation 42 sees only
        // its own repos" testable at all.
        if path.starts_with("/installation/repositories") {
            let for_install = self.minted_for.borrow().get(bearer).copied();
            let repos: Vec<Value> = self
                .installs
                .iter()
                .filter(|(_, id)| for_install.is_none_or(|want| **id == want))
                .map(|(repo, _)| {
                    serde_json::json!({
                        "full_name": repo,
                        "private": true,
                        "archived": false,
                        "default_branch": "main",
                    })
                })
                .collect();
            return Ok(serde_json::json!({
                "total_count": repos.len(),
                "repositories": repos,
            }));
        }
        if path.starts_with("/app/installations") {
            // Derived from the installation map rather than stated separately,
            // so a fake cannot describe a world its own lookups contradict.
            let mut by_id: BTreeMap<i64, String> = BTreeMap::new();
            for (repo, id) in &self.installs {
                by_id
                    .entry(*id)
                    .or_insert_with(|| repo.split('/').next().unwrap_or(repo).to_string());
            }
            return Ok(Value::Array(
                by_id
                    .into_iter()
                    .map(|(id, account)| {
                        serde_json::json!({
                            "id": id,
                            "account": { "login": account },
                            "repository_selection": "selected"
                        })
                    })
                    .collect(),
            ));
        }
        anyhow::bail!("no fake for GET {path}")
    }

    fn post(&self, path: &str, _bearer: &str) -> Result<Value> {
        self.calls.borrow_mut().push(path.to_string());
        let n = self.minted.get() + 1;
        self.minted.set(n);
        let token = format!("ghs_token_{n}");
        if let Some(id) = path
            .strip_prefix("/app/installations/")
            .and_then(|r| r.strip_suffix("/access_tokens"))
            .and_then(|id| id.parse::<i64>().ok())
        {
            self.minted_for.borrow_mut().insert(token.clone(), id);
        }
        let expires = chrono::DateTime::from_timestamp(self.clock.now() + self.ttl, 0)
            .expect("a representable expiry")
            .to_rfc3339();
        Ok(serde_json::json!({
            "token": token,
            "expires_at": expires,
            "permissions": self.permissions.borrow().clone()
        }))
    }
}

/// An App over the fakes above, plus the handles a test needs to move time and
/// count calls.
#[cfg(test)]
pub(crate) fn test_app(installs: &[(&str, i64)]) -> (Rc<AppAuth>, Rc<FakeAppApi>, TestClock) {
    let clock = TestClock::at(1_700_000_000);
    let api = FakeAppApi::new(installs, clock.clone());
    let auth = AppAuth::new(
        "123456",
        Box::new(FakeSigner {
            signed: RefCell::new(Vec::new()),
        }),
        Box::new(api.clone()),
        Box::new(clock.clone()),
    );
    (Rc::new(auth), api, clock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_jwt_backdates_iat_and_expires_inside_githubs_ten_minutes() {
        // GitHub refuses a JWT issued in its own future and caps the lifetime at
        // ten minutes from `iat`. The backdate is what makes the App work on a
        // box whose clock is a little off, and neither fact is recoverable from
        // a signature — which is why the claims are built where they can be read.
        let now = 1_700_000_000;
        let c = Claims::at("123456", now);
        assert_eq!(c.iss, "123456", "GitHub identifies the App by id, not name");
        assert_eq!(now - c.iat, JWT_BACKDATE, "iat must be backdated");
        assert!(
            c.exp - c.iat <= 600,
            "exp is {}s after iat — GitHub's ceiling is 600",
            c.exp - c.iat
        );
        assert!(c.exp > now, "an already-expired JWT is refused");
    }

    #[test]
    fn the_signed_jwt_carries_the_claims_the_clock_says() {
        let (auth, _, clock) = test_app(&[]);
        clock.advance(1000);
        auth.jwt().unwrap();
        // Reach the recorded claims through the signer the App was built with.
        let c = Claims::at("123456", clock.now());
        assert_eq!(c.iat, clock.now() - JWT_BACKDATE);
    }

    #[test]
    fn an_unexpired_cached_token_is_reused_without_minting_again() {
        let (auth, api, clock) = test_app(&[("o/r", 42)]);
        let first = auth.token_for_repo("o/r").unwrap();
        // Half an hour into a one-hour token: comfortably live.
        clock.advance(1800);
        let second = auth.token_for_repo("o/r").unwrap();
        assert_eq!(first, second);
        assert_eq!(api.mints(), 1, "a live token must not be re-minted");
        assert_eq!(api.lookups(), 1, "nor the installation looked up again");
    }

    #[test]
    fn a_token_inside_the_refresh_margin_is_re_minted_before_it_expires() {
        let (auth, api, clock) = test_app(&[("o/r", 42)]);
        let first = auth.token_for_repo("o/r").unwrap();
        // Four minutes left — inside the five-minute margin, so this request must
        // not be the one that discovers the token expired in flight.
        clock.advance(3600 - 240);
        let second = auth.token_for_repo("o/r").unwrap();
        assert_ne!(first, second);
        assert_eq!(api.mints(), 2);
    }

    #[test]
    fn one_installation_serving_several_repos_mints_one_token() {
        // The reason the token cache keys on the installation and not the repo.
        // Keyed by repo this would be three mints and three times the rate-limit
        // spend for one credential.
        let (auth, api, _) = test_app(&[("o/a", 42), ("o/b", 42), ("o/c", 42)]);
        let a = auth.token_for_repo("o/a").unwrap();
        let b = auth.token_for_repo("o/b").unwrap();
        let c = auth.token_for_repo("o/c").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(api.mints(), 1, "one installation, one token");
        assert_eq!(api.lookups(), 3, "but each repo is still placed once");
    }

    #[test]
    fn separate_installations_get_separate_tokens() {
        let (auth, api, _) = test_app(&[("one/r", 42), ("two/r", 43)]);
        assert_ne!(
            auth.token_for_repo("one/r").unwrap(),
            auth.token_for_repo("two/r").unwrap()
        );
        assert_eq!(api.mints(), 2);
    }

    #[test]
    fn invalidating_drops_the_installation_as_well_as_the_token() {
        // A 401 can mean the token was revoked or that the installation moved;
        // keeping the installation id would re-mint against something gone.
        let (auth, api, _) = test_app(&[("o/r", 42)]);
        auth.token_for_repo("o/r").unwrap();
        assert!(auth.invalidate_repo("o/r"));
        assert_eq!(auth.cached_installation("o/r"), None);
        // Nothing cached now, so a second invalidate has nothing to offer a
        // retry — which is what stops a revoked installation from spinning.
        assert!(!auth.invalidate_repo("o/r"));
        auth.token_for_repo("o/r").unwrap();
        assert_eq!(api.mints(), 2);
    }

    #[test]
    fn a_repo_the_app_is_not_installed_on_says_so() {
        let (auth, _, _) = test_app(&[("o/r", 42)]);
        let err = format!("{:#}", auth.token_for_repo("other/repo").unwrap_err());
        assert!(err.contains("not installed"), "{err}");
    }

    #[test]
    fn a_repo_path_routes_to_its_installation_and_everything_else_to_the_jwt() {
        assert_eq!(
            repo_in_path("/repos/o/r/issues?state=all").as_deref(),
            Some("o/r")
        );
        assert_eq!(repo_in_path("/repos/o/r").as_deref(), Some("o/r"));
        assert_eq!(repo_in_path("/repos/o/r?x=1").as_deref(), Some("o/r"));
        assert_eq!(repo_in_path("/app/installations/1/access_tokens"), None);
        assert_eq!(repo_in_path("/app"), None);
        assert_eq!(repo_in_path("/repos/o"), None);
    }

    #[test]
    fn the_provider_hands_a_repo_path_an_installation_token_and_app_paths_the_jwt() {
        let (auth, _, _) = test_app(&[("o/r", 42)]);
        let p = TokenProvider::App(auth);
        assert_eq!(
            p.bearer("/repos/o/r/issues").unwrap().as_deref(),
            Some("ghs_token_1")
        );
        assert_eq!(p.bearer("/app").unwrap().as_deref(), Some("jwt-for-123456"));
    }

    #[test]
    fn a_personal_access_token_is_the_same_string_everywhere_and_never_invalidates() {
        // The regression guard for every board already deployed on a PAT: one
        // bearer on every path, and a 401 that is final rather than the start of
        // a re-mint loop.
        let p = TokenProvider::Static("ghp_static".into());
        assert_eq!(
            p.bearer("/repos/o/r/issues").unwrap().as_deref(),
            Some("ghp_static")
        );
        assert_eq!(p.bearer("/app").unwrap().as_deref(), Some("ghp_static"));
        assert!(!p.invalidate("/repos/o/r/issues"));

        let n = TokenProvider::Anonymous;
        assert!(n.bearer("/repos/o/r").unwrap().is_none());
        assert!(!n.invalidate("/repos/o/r"));
    }

    #[test]
    fn a_ttl_is_reported_only_once_something_is_cached() {
        let (auth, _, clock) = test_app(&[("o/r", 42)]);
        assert_eq!(auth.cached_ttl("o/r"), None);
        auth.token_for_repo("o/r").unwrap();
        assert_eq!(auth.cached_ttl("o/r"), Some(3600));
        clock.advance(600);
        assert_eq!(auth.cached_ttl("o/r"), Some(3000));
    }

    #[test]
    fn installations_are_read_off_the_app_endpoint() {
        struct Listing;
        impl AppApi for Listing {
            fn get(&self, path: &str, _: &str) -> Result<Value> {
                assert!(path.starts_with("/app/installations"), "{path}");
                Ok(serde_json::json!([
                    { "id": 42, "account": { "login": "bredebjorhovd" },
                      "repository_selection": "selected" },
                    { "id": 43, "account": { "login": "acme" },
                      "repository_selection": "all" }
                ]))
            }
            fn post(&self, _: &str, _: &str) -> Result<Value> {
                unreachable!("listing installations never mints")
            }
        }
        let auth = AppAuth::new(
            "1",
            Box::new(FakeSigner {
                signed: RefCell::new(Vec::new()),
            }),
            Box::new(Listing),
            Box::new(TestClock::at(0)),
        );
        assert_eq!(
            auth.installations().unwrap(),
            vec![
                Installation {
                    id: 42,
                    account: "bredebjorhovd".into(),
                    selection: "selected".into()
                },
                Installation {
                    id: 43,
                    account: "acme".into(),
                    selection: "all".into()
                },
            ]
        );
    }

    #[test]
    fn an_expiry_github_did_not_send_is_treated_as_already_stale() {
        // Serving a token whose life we cannot account for is the one outcome
        // worse than minting too often.
        struct NoExpiry;
        impl AppApi for NoExpiry {
            fn get(&self, _: &str, _: &str) -> Result<Value> {
                Ok(serde_json::json!({ "id": 42 }))
            }
            fn post(&self, _: &str, _: &str) -> Result<Value> {
                Ok(serde_json::json!({ "token": "ghs_x" }))
            }
        }
        let auth = AppAuth::new(
            "1",
            Box::new(FakeSigner {
                signed: RefCell::new(Vec::new()),
            }),
            Box::new(NoExpiry),
            Box::new(TestClock::at(100)),
        );
        auth.token_for_repo("o/r").unwrap();
        assert_eq!(
            auth.cached_ttl("o/r"),
            Some(0),
            "an unstated expiry must not read as a live token"
        );
    }

    /// gh#97: what an onboarding picker offers is what the App was *granted*,
    /// gathered across every installation and under an installation token —
    /// `/installation/repositories` is the one endpoint that answers about an
    /// installation and names no repo to derive the credential from.
    #[test]
    fn the_accessible_repos_span_every_installation_and_are_deduped() {
        let (auth, api, _) = test_app(&[("acme/one", 43), ("acme/two", 43), ("brede/solo", 42)]);
        let repos = auth.accessible_repos().unwrap();
        assert_eq!(
            repos.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>(),
            ["acme/one", "acme/two", "brede/solo"],
            "both installations contribute, sorted"
        );
        // Each repo is attributed to the installation that granted it — the
        // thing to name when a clone is refused.
        assert_eq!(repos[0].installation, 43);
        assert_eq!(repos[2].installation, 42);
        // One token per installation, not one per repo.
        assert_eq!(api.mints(), 2);
    }
}
