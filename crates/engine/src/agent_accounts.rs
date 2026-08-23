//! AgentAccounts — the Claude Code / Codex CLI logins on this device
//! (feature-inventory §3.7 "Agent accounts"; port of comet's `agent-accounts.ts`).
//!
//! Each CLI stores exactly one live login:
//!
//! - **Claude Code** — credentials in `~/.claude/.credentials.json`
//!   (`$CLAUDE_CONFIG_DIR` relocates the dir) or, on macOS, the Keychain item
//!   `Claude Code-credentials`; the account identity (`oauthAccount`, `userID`)
//!   lives in `~/.claude.json`.
//! - **Codex** — `$CODEX_HOME/auth.json` (default `~/.codex`): a ChatGPT OAuth
//!   token set (identity inside the `id_token` JWT) or a raw API key.
//!
//! Claude-swap mechanics:
//!
//! 1. **Detect** the live login of each CLI and auto-snapshot it into a slot
//!    under `{data_dir}/agent-accounts/{harness}/{slotId}.json` — the current
//!    session is always backed up before any swap, and refreshed tokens stay
//!    current.
//! 2. **Swap** (`activate`): overwrite the CLI's credential store (and, for
//!    Claude, merge the identity back into `~/.claude.json`) with a saved slot.
//! 3. **Add** (`start_login`…): drive an OAuth flow for a NEW account without
//!    touching the live one. Claude uses the public PKCE code flow (paste-code);
//!    Codex spawns `codex login` against a throwaway `CODEX_HOME` and polls
//!    until its loopback callback lands.
//!
//! **Codex on a device you are not sitting at** (gh#193): the accounts RPCs are
//! relay-forwardable, so Settings → Accounts with the switcher pointed at a box
//! runs that login *on the box* — and plain `codex login` binds a fixed loopback
//! port there (measured: `127.0.0.1:1455`) and waits for OpenAI to redirect to
//! it. The operator opens the authorize URL on their laptop, the redirect hits
//! their laptop's localhost, and the box polls a login that can never land.
//! So a login [`AgentAccounts::start_login`] is told is `remote` uses
//! `codex login --device-auth` instead: the CLI prints a one-time code, polls
//! OpenAI directly, and the operator enters the code at
//! `auth.openai.com/codex/device` from whatever device has a browser. Local
//! logins keep the loopback flow — it is nicer when the browser is right there.
//!
//! Per-run accounts (gh#59): a swap is engine-wide and mutates the dir a live
//! run is reading, so a run that wants a *specific* account never swaps.
//! [`AgentAccounts::materialize`] writes the slot into a config dir of its own
//! (`{data_dir}/accounts/{slotId}/`) and the engine stamps `CLAUDE_CONFIG_DIR` /
//! `CODEX_HOME` at it in the harness child's env — both CLIs already relocate
//! wholesale on those variables (see [`AgentAccountsConfig::detect`], which
//! reads the same two), so one box can run several teammates' subscriptions at
//! once and each dispatch burns its owner's limits. The dir is the live copy
//! from then on: refresh writebacks the CLI makes land there, `read_slots`
//! absorbs them back into the slot file, and usage probes read the result.
//!
//! Usage probes: both providers expose the rate-limit view their own CLIs render
//! (`/usage` in Claude Code, `/status` in Codex). Unlike comet (fetch on every
//! list, 60s cache), native only hits the network when `force_usage` is set —
//! the default list stays offline-fast and deterministic; the UI passes
//! `forceUsage` on page mount/refresh. Cached results (60s TTL) are served to
//! non-forced lists in between.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use comet_proto::{
    AgentAccount, AgentAccountHealth, AgentAccountState, AgentAccountWarning,
    AgentAccountsSnapshot, AgentAuthKind, AgentLoginMode, AgentLoginPoll, AgentLoginStart,
    AgentLoginStatus, AgentUsageWindow, HarnessId,
};

use crate::repos::home_dir;
use crate::{EngineError, new_id, now_ms};

// Claude Code's public OAuth client (the one the CLI itself uses for the manual
// "paste the code" flow — no secret involved, PKCE carries the proof).
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_REDIRECT: &str = "https://console.anthropic.com/oauth/code/callback";
const CLAUDE_SCOPES: &str = "org:create_api_key user:profile user:inference";
const CLAUDE_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// Where `codex login --device-auth` sends the operator. Only a fallback: the
/// URL is read off the child's own output, and this stands in for the one run
/// whose banner we somehow failed to scrape — the code is live for 15 minutes
/// either way, and a dead link would waste all of them.
const CODEX_DEVICE_AUTH_URL: &str = "https://auth.openai.com/codex/device";
/// Device code authorization is an account-side grant that can be switched off,
/// and when it is, ChatGPT refuses every code — which reads exactly like a
/// mistyped one. Nothing in the CLI's output says otherwise, so every
/// device-auth failure carries the possibility (gh#193).
const DEVICE_AUTH_HINT: &str = " If the code was refused: device code \
    authorization may be turned off for that ChatGPT account — enable it under \
    Settings → Security (on a Business or Enterprise workspace an admin has to).";

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

const USAGE_TTL: Duration = Duration::from_secs(60);
/// An abandoned login flow (dialog dismissed without Cancel) is reaped past this.
const FLOW_TTL: Duration = Duration::from_secs(15 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
/// An access token this close to its stamp is treated as already gone — the
/// same margin [`AgentAccounts::claude_usage`] applies before it will spend
/// one. A token with seconds left is not a verdict a doctor line should
/// print green and watch die.
const EXPIRY_MARGIN_MS: i64 = 30_000;
/// How long `start_login` waits for `codex` to print its banner. The loopback
/// flow's URL is built locally and appears at once; the device-code banner
/// costs a round trip to OpenAI first, and that one is worth waiting out —
/// returning without the code would leave the operator nothing to type.
const CODEX_BANNER_WAIT: Duration = Duration::from_secs(5);
const CODEX_DEVICE_CODE_WAIT: Duration = Duration::from_secs(20);

/// Filesystem knobs — env-resolved in production ([`AgentAccountsConfig::detect`]),
/// explicit in tests.
#[derive(Debug, Clone)]
pub struct AgentAccountsConfig {
    /// Engine data dir; slots live under `{data_dir}/agent-accounts/`.
    pub data_dir: PathBuf,
    /// Claude config dir (`$CLAUDE_CONFIG_DIR` or `~/.claude`) — holds `.credentials.json`.
    pub claude_config_dir: PathBuf,
    /// Claude identity file (`~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json`).
    pub claude_config_file: PathBuf,
    /// Codex home (`$CODEX_HOME` or `~/.codex`) — holds `auth.json`.
    pub codex_home: PathBuf,
    /// opencode's auth store (`$OPENCODE_AUTH_FILE`, `$XDG_DATA_HOME/opencode`,
    /// or `~/.local/share/opencode`) — holds `auth.json`: a map of provider id
    /// → credential. opencode has no per-account concept (its logins are
    /// provider keys), so detection only reports "signed in".
    pub opencode_auth_file: PathBuf,
}

impl AgentAccountsConfig {
    /// Production resolution: `CLAUDE_CONFIG_DIR` relocates both the Claude config
    /// json and the credentials file; `CODEX_HOME` relocates the Codex auth file.
    pub fn detect(data_dir: &Path) -> Self {
        let env_dir = |name: &str| {
            std::env::var_os(name)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        };
        let claude_dir = env_dir("CLAUDE_CONFIG_DIR");
        let claude_config_file = match &claude_dir {
            Some(dir) => dir.join(".claude.json"),
            None => home_dir().join(".claude.json"),
        };
        Self {
            data_dir: data_dir.to_path_buf(),
            claude_config_dir: claude_dir.unwrap_or_else(|| home_dir().join(".claude")),
            claude_config_file,
            codex_home: env_dir("CODEX_HOME").unwrap_or_else(|| home_dir().join(".codex")),
            opencode_auth_file: env_dir("OPENCODE_AUTH_FILE")
                .or_else(|| env_dir("XDG_DATA_HOME").map(|d| d.join("opencode").join("auth.json")))
                .unwrap_or_else(|| {
                    home_dir()
                        .join(".local")
                        .join("share")
                        .join("opencode")
                        .join("auth.json")
                }),
        }
    }

    fn claude_creds_file(&self) -> PathBuf {
        self.claude_config_dir.join(".credentials.json")
    }

    fn codex_auth_file(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }

    fn root_dir(&self) -> PathBuf {
        self.data_dir.join("agent-accounts")
    }

    /// Parent of the per-slot config dirs a run is pointed at (gh#59). Kept
    /// beside the slot files rather than inside them: `root_dir` is swept for
    /// `.login-*` leftovers at boot and holds only `{slotId}.json` records,
    /// while these are live CLI homes the harness children write into.
    fn accounts_dir(&self) -> PathBuf {
        self.data_dir.join("accounts")
    }
}

// ── slot storage ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SlotProfile {
    pub(crate) email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) plan: Option<String>,
    pub(crate) auth_kind: AgentAuthKind,
}

/// One saved login (`{slotId}.json`), same field surface as comet's slot files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Slot {
    pub(crate) id: String,
    pub(crate) harness: HarnessId,
    /// The provider-side identity the slot is keyed by (account uuid/email).
    pub(crate) account_key: String,
    pub(crate) profile: SlotProfile,
    /// Claude: the `.credentials.json`/Keychain payload. Codex: `auth.json`.
    pub(crate) credentials: serde_json::Value,
    /// Claude only: `{oauthAccount, userID}` merged into `~/.claude.json` on swap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) claude_config: Option<serde_json::Value>,
    pub(crate) saved_at: i64,
    /// First time this account was saved — the STABLE sort key, so switching the
    /// active account (which re-snapshots and bumps `saved_at`) never reorders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) created_at: Option<i64>,
}

/// A live detection result (before it's persisted into a slot).
#[derive(Debug, Clone)]
struct Detected {
    account_key: String,
    profile: SlotProfile,
    /// `None` ⇒ we know a login exists but couldn't read the secret.
    credentials: Option<serde_json::Value>,
    claude_config: Option<serde_json::Value>,
}

// ── login flows ─────────────────────────────────────────────────────────────

enum LoginFlow {
    Claude {
        verifier: String,
        started_at: Instant,
    },
    Codex {
        /// The `codex login` child; monitored (try_wait) + killable from cancel.
        child: Arc<Mutex<Option<tokio::process::Child>>>,
        /// Throwaway `CODEX_HOME` — the live `~/.codex` is never touched.
        home: PathBuf,
        started_at: Instant,
        output: Arc<Mutex<String>>,
        /// `Some(code)` once the child exited (`None` code = killed by signal).
        exit: Arc<Mutex<Option<Option<i32>>>>,
        /// `--device-auth` rather than the loopback flow (gh#193). Decides two
        /// things: this flow holds no loopback port, so it never has to be
        /// superseded; and its failures carry [`DEVICE_AUTH_HINT`].
        device_auth: bool,
    },
}

impl LoginFlow {
    fn started_at(&self) -> Instant {
        match self {
            LoginFlow::Claude { started_at, .. } | LoginFlow::Codex { started_at, .. } => {
                *started_at
            }
        }
    }
}

// ── service ─────────────────────────────────────────────────────────────────

/// Cached usage probe result: the windows (or a remembered miss) + fetch time.
type CachedUsage = (Option<Vec<AgentUsageWindow>>, Instant);

struct Inner {
    config: AgentAccountsConfig,
    http: reqwest::Client,
    flows: Mutex<HashMap<String, LoginFlow>>,
    /// `"{harness}:{accountKey}"` → cached usage windows.
    usage_cache: Mutex<HashMap<String, CachedUsage>>,
    /// Slots with a token refresh in flight — a second refresh of the same
    /// (commonly single-use) refresh token would revoke the family.
    inflight_refreshes: Mutex<std::collections::HashSet<String>>,
    /// Slot id → how many live runs are pointed at its dir. A CLI holding a
    /// token pair in memory is the owner of it; refreshing underneath one
    /// rotates a token it will still try to use and can force a re-login, so
    /// leased slots are skipped by the usage refresher exactly as the live
    /// login is.
    leases: Mutex<HashMap<String, usize>>,
    /// Does the `codex` on this device understand `login --device-auth`
    /// (gh#193)? Probed once — the answer changes only when the CLI is
    /// upgraded, and the probe is a process spawn on a path someone is
    /// waiting on. `None` = not asked yet.
    codex_device_auth: Mutex<Option<bool>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct AgentAccounts {
    inner: Arc<Inner>,
}

/// A live run's claim on an account slot, released on drop.
pub struct AccountLease {
    inner: Arc<Inner>,
    account_id: String,
}

/// What [`AgentAccounts::expired_login`] found: the login a dying run spent,
/// and when its stored token stopped being any good.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpiredLogin {
    pub email: String,
    /// Epoch ms at which the stored access token's stamp ran out.
    pub expired_at: i64,
}

impl Drop for AccountLease {
    fn drop(&mut self) {
        let mut leases = lock(&self.inner.leases);
        if let Some(count) = leases.get_mut(&self.account_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                leases.remove(&self.account_id);
            }
        }
    }
}

impl AgentAccounts {
    pub fn new(config: AgentAccountsConfig) -> Self {
        // Startup sweep: a previous process that crashed mid-login leaves
        // `.login-<uuid>` throwaway CODEX_HOME dirs — each may hold live OAuth
        // tokens — with no owner to clean them. Reclaim them at boot.
        let root = config.root_dir();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(".login-") {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            inner: Arc::new(Inner {
                config,
                http,
                flows: Mutex::new(HashMap::new()),
                usage_cache: Mutex::new(HashMap::new()),
                inflight_refreshes: Mutex::new(std::collections::HashSet::new()),
                leases: Mutex::new(HashMap::new()),
                codex_device_auth: Mutex::new(None),
            }),
        }
    }

    // ── per-run account dirs (gh#59) ────────────────────────────────────────

    /// The config dir a run pointed at `account_id` should use — created and
    /// seeded from the slot if it does not exist yet.
    ///
    /// The returned path is what `CLAUDE_CONFIG_DIR` / `CODEX_HOME` are set to
    /// for the harness child, so the CLI reads and refreshes *this* account's
    /// tokens and never touches `~/.claude` or `~/.codex`. Seeding is one-way
    /// and freshness-guarded: once the dir exists it is the live copy, and the
    /// slot file is only re-stamped over it when the slot holds strictly newer
    /// credentials (a re-login through the accounts UI).
    pub fn materialize(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<PathBuf, EngineError> {
        if !is_slot_id(account_id) {
            return Err(EngineError::Other(format!(
                "`{account_id}` is not an agent account id."
            )));
        }
        let slot = self
            .read_slots(harness)
            .into_iter()
            .find(|s| s.id == account_id)
            .ok_or_else(|| {
                EngineError::Other(format!(
                    "no saved {} login with id `{account_id}` — sign it in under Agent \
                     accounts first.",
                    harness.as_str()
                ))
            })?;
        let dir = self.account_dir(&slot.id);
        std::fs::create_dir_all(&dir)?;
        // Owner-only: every file below it is a live token set.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        match harness {
            HarnessId::ClaudeCode => {
                let file = dir.join(".credentials.json");
                if is_stale(&file, harness, &slot.credentials) {
                    write_file_atomic(&file, slot.credentials.to_string().as_bytes(), true)?;
                }
                // `CLAUDE_CONFIG_DIR` relocates the identity file too, so the
                // CLI reads the account out of this dir rather than the shared
                // `~/.claude.json` — which is what makes the run's account
                // independent of whatever is live engine-wide.
                let identity = dir.join(".claude.json");
                let existing = read_json(&identity);
                let mut cfg = existing.clone().unwrap_or_else(|| serde_json::json!({}));
                if let Some(map) = cfg.as_object_mut() {
                    map.insert("oauthAccount".into(), slot_oauth_account(&slot));
                    match slot
                        .claude_config
                        .as_ref()
                        .and_then(|c| c.get("userID"))
                        .filter(|v| v.is_string())
                    {
                        Some(user_id) => {
                            map.insert("userID".into(), user_id.clone());
                        }
                        None => {
                            map.remove("userID");
                        }
                    }
                }
                // Only when the identity actually changed. The CLI writes its
                // own state into this file (onboarding flags, project history)
                // and a second run on the same account re-materializing over a
                // write in flight would drop it.
                if existing.as_ref() != Some(&cfg) {
                    write_file_atomic(&identity, cfg.to_string().as_bytes(), false)?;
                }
                // The board's own skill, beside the credentials (gh#133).
                //
                // This dir *is* `CLAUDE_CONFIG_DIR` for every run pointed at
                // the slot, and Claude Code discovers skills under the config
                // dir — so the copy in the box user's `~/.claude` is invisible
                // to exactly the agents the board dispatches. Without this, a
                // dispatched agent is the one agent on the box that has never
                // heard of the board it came from, and the fix used to be a
                // by-hand loop over the slots that existed that night.
                //
                // Written on every materialize (so, every dispatch) and
                // byte-compared first: a slot tracks whatever binary is running
                // with no install step, and a re-materialize that changes
                // nothing writes nothing. Never fatal — a dispatch that can run
                // is worth more than a skill file that could not be written.
                if let Err(err) = comet_board::skill::install_into(&dir) {
                    tracing::warn!(slot = %slot.id, error = %err, "skill install into slot failed");
                }
            }
            // No skill for Codex: skills are a Claude Code discovery mechanism
            // and `CODEX_HOME` has no equivalent. What a Codex slot gets
            // instead is the conventions block in `AGENTS.md`, written by the
            // dispatch itself (`comet_board::conventions`, gh#272) rather than
            // here — it is a property of the *route*, which this call has never
            // heard of, and it is also how a Claude slot gets the same text
            // beside the skill above.
            HarnessId::Codex => {
                let file = dir.join("auth.json");
                if is_stale(&file, harness, &slot.credentials) {
                    let json = serde_json::to_string_pretty(&slot.credentials)
                        .map_err(|e| EngineError::Other(format!("serialize codex auth: {e}")))?;
                    write_file_atomic(&file, json.as_bytes(), true)?;
                }
            }
            other => {
                return Err(EngineError::Other(format!(
                    "agent accounts are not supported for {other:?}"
                )));
            }
        }
        Ok(dir)
    }

    /// Hold `account_id` for the life of a run. While held, the usage refresher
    /// leaves the slot's tokens alone — the CLI reading that dir owns them.
    pub fn lease(&self, account_id: &str) -> AccountLease {
        *lock(&self.inner.leases)
            .entry(account_id.to_string())
            .or_insert(0) += 1;
        AccountLease {
            inner: self.inner.clone(),
            account_id: account_id.to_string(),
        }
    }

    fn is_leased(&self, account_id: &str) -> bool {
        lock(&self.inner.leases)
            .get(account_id)
            .is_some_and(|n| *n > 0)
    }

    fn account_dir(&self, slot_id: &str) -> PathBuf {
        self.inner.config.accounts_dir().join(slot_id)
    }

    /// The config dir a run pointed at `account_id` reads — WITHOUT creating or
    /// seeding it, unlike [`AgentAccounts::materialize`].
    ///
    /// For questions *about* a run rather than preparation for one: what skills
    /// it can invoke (gh#134) is answered by looking in the dir the child's
    /// `CLAUDE_CONFIG_DIR` will point at, and asking that question must not
    /// mint an account dir for a chat nobody has run yet.
    pub fn config_dir_for(&self, account_id: &str) -> Option<PathBuf> {
        is_slot_id(account_id).then(|| self.account_dir(account_id))
    }

    /// The CLI's own config dir — `$CLAUDE_CONFIG_DIR` or `~/.claude` — which
    /// is what a run with no account named reads.
    pub fn default_config_dir(&self, harness: HarnessId) -> Option<PathBuf> {
        match harness {
            HarnessId::ClaudeCode => Some(self.inner.config.claude_config_dir.clone()),
            HarnessId::Codex => Some(self.inner.config.codex_home.clone()),
            _ => None,
        }
    }

    /// The live credentials file inside a materialized dir, when there is one.
    fn materialized(&self, slot: &Slot) -> Option<serde_json::Value> {
        let dir = self.account_dir(&slot.id);
        let file = match slot.harness {
            HarnessId::ClaudeCode => dir.join(".credentials.json"),
            HarnessId::Codex => dir.join("auth.json"),
            _ => return None,
        };
        read_json(&file)
    }

    // ── list ────────────────────────────────────────────────────────────────

    /// Detect both CLIs, auto-snapshot the live logins, and assemble the view.
    pub async fn list(&self, force_usage: bool) -> Result<AgentAccountsSnapshot, EngineError> {
        if force_usage {
            lock(&self.inner.usage_cache).clear();
        }
        let mut warnings: Vec<AgentAccountWarning> = Vec::new();
        let mut active_keys: HashMap<HarnessId, String> = HashMap::new();
        let mut unreadable: HashMap<HarnessId, Detected> = HashMap::new();

        let (claude, claude_warning) = self.detect_claude().await;
        if let Some(message) = claude_warning {
            warnings.push(AgentAccountWarning {
                harness: HarnessId::ClaudeCode,
                message,
            });
        }
        if let Some(detected) = claude {
            active_keys.insert(HarnessId::ClaudeCode, detected.account_key.clone());
            if detected.credentials.is_some() {
                self.snapshot_detected(HarnessId::ClaudeCode, &detected)?;
            } else {
                unreadable.insert(HarnessId::ClaudeCode, detected);
            }
        }
        if let Some(detected) = self.detect_codex() {
            active_keys.insert(HarnessId::Codex, detected.account_key.clone());
            self.snapshot_detected(HarnessId::Codex, &detected)?;
        }

        // Stable presentation order: provider, then slot creation order (never
        // active-first — switching must not reshuffle the cards).
        let mut accounts: Vec<AgentAccount> = Vec::new();
        for harness in [HarnessId::ClaudeCode, HarnessId::Codex] {
            let active_key = active_keys.get(&harness).cloned();
            let slots = self.read_slots(harness);
            for slot in &slots {
                let active = active_key.as_deref() == Some(slot.account_key.as_str());
                let usage = self.usage_for(harness, slot, active, force_usage).await;
                accounts.push(AgentAccount {
                    id: slot.id.clone(),
                    harness,
                    email: Some(slot.profile.email.clone()),
                    plan_label: slot.profile.plan.clone(),
                    active,
                    usage_windows: usage.unwrap_or_default(),
                    display_name: slot.profile.display_name.clone(),
                    organization: slot.profile.organization.clone(),
                    auth_kind: Some(slot.profile.auth_kind),
                    switchable: true,
                    saved_at: Some(slot.saved_at),
                });
            }
            // A live login whose credentials we couldn't read has no slot — still
            // show it (active, but not re-activatable until the Keychain relents).
            if let Some(u) = unreadable.get(&harness)
                && !slots.iter().any(|s| s.account_key == u.account_key)
            {
                accounts.push(AgentAccount {
                    id: slot_id_for(harness, &u.account_key),
                    harness,
                    email: Some(u.profile.email.clone()),
                    plan_label: u.profile.plan.clone(),
                    active: true,
                    usage_windows: Vec::new(),
                    display_name: u.profile.display_name.clone(),
                    organization: u.profile.organization.clone(),
                    auth_kind: Some(u.profile.auth_kind),
                    switchable: false,
                    saved_at: None,
                });
            }
        }

        // opencode: single detected "signed in" account when its provider auth
        // store is non-empty (no slots — comet has no opencode swap/login, so
        // it's always active and never switchable).
        if let Some(detected) = self.detect_opencode() {
            accounts.push(AgentAccount {
                id: slot_id_for(HarnessId::Opencode, &detected.account_key),
                harness: HarnessId::Opencode,
                email: Some(detected.profile.email.clone()),
                plan_label: detected.profile.plan.clone(),
                active: true,
                usage_windows: Vec::new(),
                display_name: detected.profile.display_name.clone(),
                organization: detected.profile.organization.clone(),
                auth_kind: Some(detected.profile.auth_kind),
                switchable: false,
                saved_at: None,
            });
        }
        Ok(AgentAccountsSnapshot { accounts, warnings })
    }

    // ── swap ────────────────────────────────────────────────────────────────

    /// Swap the CLI's live login to a saved slot. Detection runs first, so the
    /// CURRENT login is snapshotted into its slot before being overwritten (the
    /// claude-swap trick — a swap never strands the session it replaces).
    pub async fn activate(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        self.list(false).await?;
        let slot = self
            .read_slots(harness)
            .into_iter()
            .find(|s| s.id == account_id)
            .ok_or_else(|| {
                EngineError::Other(
                    "That saved login no longer exists — refresh and try again.".into(),
                )
            })?;
        match harness {
            HarnessId::ClaudeCode => self.activate_claude(&slot).await?,
            HarnessId::Codex => self.activate_codex(&slot)?,
            other => {
                return Err(EngineError::Other(format!(
                    "agent accounts are not supported for {other:?}"
                )));
            }
        }
        self.list(false).await
    }

    async fn activate_claude(&self, slot: &Slot) -> Result<(), EngineError> {
        self.write_claude_credentials(&slot.credentials).await?;
        // Merge the identity back into ~/.claude.json — everything else (caches,
        // project history, onboarding flags) is left untouched, which is all
        // Claude Code needs to treat this as a fresh login.
        //
        // GUARD the merge: a parse failure on an EXISTING file means "don't touch
        // it", not "start fresh" — writing only our identity fields would destroy
        // the user's entire Claude config. Only a missing file may start from {}.
        let file = &self.inner.config.claude_config_file;
        let cfg = read_json(file);
        if cfg.is_none() && file.exists() {
            return Err(EngineError::Other(
                "~/.claude.json exists but could not be parsed — not switching to avoid wiping \
                 it. Fix or remove the file and try again."
                    .into(),
            ));
        }
        let mut merged = cfg.unwrap_or_else(|| serde_json::json!({}));
        let map = merged.as_object_mut().ok_or_else(|| {
            EngineError::Other("~/.claude.json is not a JSON object — not switching.".into())
        })?;
        let user_id = slot
            .claude_config
            .as_ref()
            .and_then(|cc| cc.get("userID"))
            .cloned();
        map.insert("oauthAccount".into(), slot_oauth_account(slot));
        match user_id.filter(|v| v.is_string()) {
            Some(user_id) => {
                map.insert("userID".into(), user_id);
            }
            None => {
                map.remove("userID");
            }
        }
        // Atomic: Claude Code rewrites this file frequently — a torn write from
        // our side must never be readable as "empty config".
        write_file_atomic(file, merged.to_string().as_bytes(), false)
    }

    fn activate_codex(&self, slot: &Slot) -> Result<(), EngineError> {
        std::fs::create_dir_all(&self.inner.config.codex_home)?;
        let json = serde_json::to_string_pretty(&slot.credentials)
            .map_err(|e| EngineError::Other(format!("serialize codex auth: {e}")))?;
        write_file_atomic(&self.inner.config.codex_auth_file(), json.as_bytes(), true)
    }

    // ── forget ──────────────────────────────────────────────────────────────

    pub async fn forget(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        // Reject anything that isn't a slot id (16 lowercase hex) BEFORE touching
        // the filesystem: `account_id` is a raw RPC string that becomes a path,
        // so a crafted id (`../../…`) must never reach `remove_file`.
        if !is_slot_id(account_id) {
            return Err(EngineError::Other("Unknown account.".into()));
        }
        let snapshot = self.list(false).await?;
        let active = snapshot
            .accounts
            .iter()
            .any(|a| a.harness == harness && a.id == account_id && a.active);
        if active {
            return Err(EngineError::Other(
                "That's the live login — switch to another account first (it would just be \
                 re-detected)."
                    .into(),
            ));
        }
        let file = self.slots_dir(harness)?.join(format!("{account_id}.json"));
        if file.exists() {
            std::fs::remove_file(&file)?;
        }
        // The materialized dir holds the same tokens the slot file did —
        // forgetting an account has to take both, or the next dispatch naming
        // that id would find a live login the accounts page says is gone.
        let dir = self.account_dir(account_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        self.list(false).await
    }

    // ── add-account OAuth flows ─────────────────────────────────────────────

    /// `remote` = this login is being driven for a device the operator is not
    /// sitting at (gh#193) — the call carried a `targetDeviceId`, or it arrived
    /// over the relay rather than this device's own IPC. Only Codex cares: it
    /// is the difference between a loopback callback that can land and one that
    /// cannot. Claude's flow is a code the operator carries by hand in both
    /// directions, so it is already device-agnostic.
    pub async fn start_login(
        &self,
        harness: HarnessId,
        remote: bool,
    ) -> Result<AgentLoginStart, EngineError> {
        self.sweep_flows();
        match harness {
            HarnessId::ClaudeCode => Ok(self.start_claude_login()),
            HarnessId::Codex => self.start_codex_login(remote).await,
            other => Err(EngineError::Other(format!(
                "agent logins are not supported for {other:?}"
            ))),
        }
    }

    fn start_claude_login(&self) -> AgentLoginStart {
        let login_id = new_id();
        // PKCE: 32 random bytes (two v4 uuids) as the verifier, S256 challenge.
        let raw: Vec<u8> = uuid::Uuid::new_v4()
            .as_bytes()
            .iter()
            .chain(uuid::Uuid::new_v4().as_bytes())
            .copied()
            .collect();
        let verifier = BASE64_URL.encode(&raw);
        let challenge = BASE64_URL.encode(Sha256::digest(verifier.as_bytes()));
        let url = format!(
            "https://claude.ai/oauth/authorize?code=true&client_id={CLAUDE_CLIENT_ID}\
             &response_type=code&redirect_uri={}&scope={}&code_challenge={challenge}\
             &code_challenge_method=S256&state={verifier}",
            urlencode(CLAUDE_REDIRECT),
            urlencode(CLAUDE_SCOPES),
        );
        lock(&self.inner.flows).insert(
            login_id.clone(),
            LoginFlow::Claude {
                verifier,
                started_at: Instant::now(),
            },
        );
        AgentLoginStart {
            login_id,
            url,
            mode: AgentLoginMode::PasteCode,
            user_code: None,
        }
    }

    async fn start_codex_login(&self, remote: bool) -> Result<AgentLoginStart, EngineError> {
        // A login driven for another device has no browser on that device to
        // redirect to; `codex login --device-auth` is the flow that needs none
        // (gh#193). Version-gated rather than assumed: older `codex` builds
        // reject the flag, and silently falling back to loopback would put us
        // right back to polling a login that cannot land.
        let device_auth = remote
            && match self.codex_supports_device_auth().await {
                Some(true) => true,
                Some(false) => {
                    return Err(EngineError::Other(
                        "That device's `codex` is too old to sign in without a browser on \
                         it — `codex login --device-auth` is not in this build. Update codex \
                         there, or add the account from the device itself."
                            .into(),
                    ));
                }
                // The probe could not run at all (no `codex` on PATH, most
                // likely). Carry on and let the spawn below say so properly.
                None => true,
            };

        // At most ONE *loopback* codex login flow at a time: plain `codex login`
        // binds a fixed loopback OAuth port, so a lingering earlier flow makes
        // every retry exit on EADDRINUSE. Starting a new one supersedes — and
        // reaps — any pending. Device-auth flows bind nothing and are left alone.
        if !device_auth {
            let stale: Vec<String> = lock(&self.inner.flows)
                .iter()
                .filter(|(_, f)| {
                    matches!(
                        f,
                        LoginFlow::Codex {
                            device_auth: false,
                            ..
                        }
                    )
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in stale {
                self.cancel_login(&id);
            }
        }

        let login_id = new_id();
        // A throwaway CODEX_HOME isolates the new login completely — the live
        // ~/.codex session is never touched until the user explicitly switches.
        let home = self
            .inner
            .config
            .root_dir()
            .join(format!(".login-{login_id}"));
        std::fs::create_dir_all(&home)?;
        let mut command = tokio::process::Command::new("codex");
        command.arg("login");
        if device_auth {
            command.arg("--device-auth");
        }
        let mut child = match command
            .env("CODEX_HOME", &home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&home);
                return Err(EngineError::Other(
                    if err.kind() == std::io::ErrorKind::NotFound {
                        "The `codex` CLI was not found on this device — install it first.".into()
                    } else {
                        format!("Could not start codex login: {err}")
                    },
                ));
            }
        };

        // codex prints the authorize URL (to stderr as of 0.142 — scan both
        // streams) and usually opens the browser itself; grab it so the app can
        // open it too. Under `--device-auth` the same banner carries the
        // one-time code, which is the whole payload of that flow.
        let output = Arc::new(Mutex::new(String::new()));
        for pipe in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        ]
        .into_iter()
        .flatten()
        {
            let sink = output.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut pipe = pipe;
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    lock(&sink).push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            });
        }

        let child = Arc::new(Mutex::new(Some(child)));
        let exit: Arc<Mutex<Option<Option<i32>>>> = Arc::new(Mutex::new(None));
        {
            // Monitor: poll try_wait so the child is reaped without owning it —
            // the cancel path needs concurrent kill access.
            let child = child.clone();
            let exit = exit.clone();
            tokio::spawn(async move {
                loop {
                    {
                        let mut slot = lock(&child);
                        match slot.as_mut().map(|c| c.try_wait()) {
                            None => break,
                            Some(Ok(Some(status))) => {
                                *lock(&exit) = Some(status.code());
                                *slot = None;
                                break;
                            }
                            Some(Ok(None)) => {}
                            Some(Err(_)) => {
                                *lock(&exit) = Some(None);
                                *slot = None;
                                break;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            });
        }

        lock(&self.inner.flows).insert(
            login_id.clone(),
            LoginFlow::Codex {
                child,
                home,
                started_at: Instant::now(),
                output: output.clone(),
                exit: exit.clone(),
                device_auth,
            },
        );

        // Wait for the banner. The loopback flow only wants the URL and can
        // live without it (the CLI opened a browser itself); device auth wants
        // the code, and there is nothing to show without it.
        let deadline = Instant::now()
            + if device_auth {
                CODEX_DEVICE_CODE_WAIT
            } else {
                CODEX_BANNER_WAIT
            };
        let (url, user_code) = loop {
            let banner = strip_ansi(&lock(&output));
            let url = scan_openai_url(&banner);
            let code = device_auth.then(|| scan_device_code(&banner)).flatten();
            if (device_auth && code.is_some()) || (!device_auth && url.is_some()) {
                break (url, code);
            }
            if lock(&exit).is_some() || Instant::now() > deadline {
                break (url, code);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        if device_auth && user_code.is_none() {
            // No code means the operator has nothing to enter — polling from
            // here would be the exact silence gh#193 is about. Say so instead,
            // in codex's own words where it left any.
            let reason = last_output_line(&lock(&output))
                .unwrap_or_else(|| "codex printed no device code".to_string());
            self.cancel_login(&login_id);
            return Err(EngineError::Other(format!(
                "Could not start device sign-in on that device: {reason}.{DEVICE_AUTH_HINT}"
            )));
        }

        Ok(AgentLoginStart {
            login_id,
            url: url.unwrap_or_else(|| {
                if device_auth {
                    CODEX_DEVICE_AUTH_URL.to_string()
                } else {
                    String::new()
                }
            }),
            mode: if device_auth {
                AgentLoginMode::DeviceCode
            } else {
                AgentLoginMode::Browser
            },
            user_code,
        })
    }

    /// Does the `codex` on this device understand `login --device-auth`?
    /// `None` = the probe itself could not run, which is not the same answer as
    /// "no" — a missing CLI is the spawn path's error to report, with its own
    /// wording. Cached across calls (see [`Inner::codex_device_auth`]).
    async fn codex_supports_device_auth(&self) -> Option<bool> {
        if let Some(known) = *lock(&self.inner.codex_device_auth) {
            return Some(known);
        }
        let output = tokio::process::Command::new("codex")
            .args(["login", "--help"])
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .ok()?;
        // `--help` is not guaranteed to exit 0 on every build; the flag list is
        // what is being read, so take whichever stream carried it.
        let help = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let supported = help.contains("--device-auth");
        *lock(&self.inner.codex_device_auth) = Some(supported);
        Some(supported)
    }

    /// Exchange the pasted `code#state` for tokens and save the account as a slot
    /// (the live login is untouched — switching is an explicit, separate act).
    pub async fn complete_login(
        &self,
        login_id: &str,
        code: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        let verifier = match lock(&self.inner.flows).get(login_id) {
            Some(LoginFlow::Claude { verifier, .. }) => verifier.clone(),
            _ => {
                return Err(EngineError::Other(
                    "This sign-in attempt expired — start again.".into(),
                ));
            }
        };
        let (auth_code, state) = match code.trim().split_once('#') {
            Some((c, s)) => (c.to_string(), s.to_string()),
            None => (code.trim().to_string(), verifier.clone()),
        };
        if auth_code.is_empty() {
            return Err(EngineError::Other(
                "That code looks empty — paste the whole code.".into(),
            ));
        }
        let token = self
            .inner
            .http
            .post(CLAUDE_TOKEN_URL)
            .json(&serde_json::json!({
                "grant_type": "authorization_code",
                "code": auth_code,
                "state": state,
                "client_id": CLAUDE_CLIENT_ID,
                "redirect_uri": CLAUDE_REDIRECT,
                "code_verifier": verifier,
            }))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| EngineError::Other(format!("token exchange failed: {e}")))?;
        if !token.status().is_success() {
            let status = token.status();
            let body = token.text().await.unwrap_or_default();
            let excerpt: String = body.chars().take(200).collect();
            return Err(EngineError::Other(format!(
                "Anthropic rejected the code ({status}): {excerpt}"
            )));
        }
        let token: serde_json::Value = token
            .json()
            .await
            .map_err(|e| EngineError::Other(format!("token exchange returned junk: {e}")))?;

        let access_token = str_field(&token, "access_token");
        let refresh_token = str_field(&token, "refresh_token");
        let expires_in = token
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600);
        let (Some(access_token), Some(refresh_token)) = (access_token, refresh_token) else {
            return Err(EngineError::Other(
                "Anthropic returned no usable tokens — try signing in again.".into(),
            ));
        };

        // Best-effort profile fetch — fills in the plan/org the way Claude Code does.
        let profile: Option<serde_json::Value> = match self
            .inner
            .http
            .get(CLAUDE_PROFILE_URL)
            .bearer_auth(&access_token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => res.json().await.ok(),
            _ => None,
        };
        let empty = serde_json::json!({});
        let p_account = profile
            .as_ref()
            .and_then(|p| p.get("account"))
            .unwrap_or(&empty);
        let p_org = profile
            .as_ref()
            .and_then(|p| p.get("organization"))
            .unwrap_or(&empty);
        let t_account = token.get("account").unwrap_or(&empty);
        let t_org = token.get("organization").unwrap_or(&empty);

        let email = str_field(p_account, "email_address")
            .or_else(|| str_field(t_account, "email_address"))
            .ok_or_else(|| {
                EngineError::Other("Could not identify the signed-in account.".into())
            })?;
        let account_uuid = str_field(p_account, "uuid")
            .or_else(|| str_field(t_account, "uuid"))
            .unwrap_or_else(|| email.clone());
        let org_name = str_field(p_org, "name").or_else(|| str_field(t_org, "name"));
        let org_type = str_field(p_org, "organization_type");
        let rate_tier = str_field(p_org, "rate_limit_tier");
        let display_name =
            str_field(p_account, "display_name").or_else(|| str_field(p_account, "full_name"));
        let subscription_type = match org_type.as_deref() {
            Some("claude_max") => Some("max"),
            Some("claude_pro") => Some("pro"),
            Some("claude_team") => Some("team"),
            Some("claude_enterprise") => Some("enterprise"),
            _ => None,
        };

        let scopes: Vec<String> = str_field(&token, "scope")
            .unwrap_or_else(|| CLAUDE_SCOPES.to_string())
            .split(' ')
            .map(str::to_string)
            .collect();
        let mut oauth = serde_json::json!({
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": now_ms() + expires_in * 1000,
            "scopes": scopes,
        });
        if let (Some(sub), Some(map)) = (subscription_type, oauth.as_object_mut()) {
            map.insert("subscriptionType".into(), serde_json::json!(sub));
        }
        let mut oauth_account = serde_json::json!({
            "accountUuid": account_uuid,
            "emailAddress": email,
            "organizationUuid": str_field(p_org, "uuid").or_else(|| str_field(t_org, "uuid")),
            "organizationName": org_name,
            "displayName": display_name,
        });
        if let Some(map) = oauth_account.as_object_mut() {
            if let Some(t) = &org_type {
                map.insert("organizationType".into(), serde_json::json!(t));
            }
            if let Some(t) = &rate_tier {
                map.insert("organizationRateLimitTier".into(), serde_json::json!(t));
            }
        }

        self.write_slot(&Slot {
            id: slot_id_for(HarnessId::ClaudeCode, &account_uuid),
            harness: HarnessId::ClaudeCode,
            account_key: account_uuid.clone(),
            profile: SlotProfile {
                email,
                display_name,
                organization: org_name,
                plan: claude_plan(org_type.as_deref(), rate_tier.as_deref()),
                auth_kind: AgentAuthKind::Oauth,
            },
            credentials: serde_json::json!({ "claudeAiOauth": oauth }),
            claude_config: Some(serde_json::json!({ "oauthAccount": oauth_account })),
            saved_at: now_ms(),
            created_at: None,
        })?;
        lock(&self.inner.flows).remove(login_id);
        self.list(false).await
    }

    pub async fn poll_login(&self, login_id: &str) -> Result<AgentLoginPoll, EngineError> {
        self.sweep_flows();
        let (home, exit, output, device_auth) = match lock(&self.inner.flows).get(login_id) {
            None => {
                return Err(EngineError::Other(
                    "This sign-in attempt expired — start again.".into(),
                ));
            }
            Some(LoginFlow::Claude { .. }) => {
                return Ok(AgentLoginPoll {
                    status: AgentLoginStatus::Pending,
                    message: None,
                });
            }
            Some(LoginFlow::Codex {
                home,
                exit,
                output,
                device_auth,
                ..
            }) => (home.clone(), exit.clone(), output.clone(), *device_auth),
        };
        if let Some(detected) = read_json(&home.join("auth.json")).and_then(parse_codex_auth) {
            self.snapshot_detected(HarnessId::Codex, &detected)?;
            self.cancel_login(login_id);
            return Ok(AgentLoginPoll {
                status: AgentLoginStatus::Done,
                message: None,
            });
        }
        let exited = *lock(&exit);
        if let Some(code) = exited {
            self.cancel_login(login_id);
            let mut message = if code == Some(0) {
                "codex login finished without credentials.".to_string()
            } else {
                last_output_line(&lock(&output)).unwrap_or_else(|| "sign-in failed".to_string())
            };
            // A device-auth run that ends without credentials is most often the
            // grant, not the typing — and codex's own last line will not say so.
            if device_auth {
                message.push_str(DEVICE_AUTH_HINT);
            }
            return Ok(AgentLoginPoll {
                status: AgentLoginStatus::Error,
                message: Some(message),
            });
        }
        Ok(AgentLoginPoll {
            status: AgentLoginStatus::Pending,
            message: None,
        })
    }

    /// Drop a flow: kill a pending `codex login` child (a loopback one holds the
    /// fixed OAuth port; a device-auth one is sitting on a poll loop against
    /// OpenAI) and reclaim its throwaway home dir. Idempotent.
    pub fn cancel_login(&self, login_id: &str) {
        let flow = lock(&self.inner.flows).remove(login_id);
        if let Some(LoginFlow::Codex { child, home, .. }) = flow {
            if let Some(c) = lock(&child).as_mut() {
                let _ = c.start_kill();
            }
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    /// Engine shutdown: kill any in-flight login child so an orphan `codex login`
    /// can't survive the restart and brick the next attempt.
    pub fn shutdown(&self) {
        let ids: Vec<String> = lock(&self.inner.flows).keys().cloned().collect();
        for id in ids {
            self.cancel_login(&id);
        }
    }

    /// Lazy TTL sweep (comet uses a background fiber; native reaps on the next
    /// accounts call — same bound, no standing task).
    fn sweep_flows(&self) {
        let stale: Vec<String> = lock(&self.inner.flows)
            .iter()
            .filter(|(_, f)| f.started_at().elapsed() > FLOW_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.cancel_login(&id);
        }
    }

    // ── who a run bills (gh#101) ────────────────────────────────────────────

    /// Whose subscription a run under `harness` pointed at `account` would
    /// spend — the board's billing guard asks this before every dispatch.
    ///
    /// `None` for `account` is the device's own CLI login, which is what a run
    /// naming no slot actually reaches; the answer is that login's email, read
    /// straight off the CLI's own config file.
    ///
    /// Deliberately not [`AgentAccounts::list`]: that detects, snapshots, and
    /// probes usage over the network, and this runs on the board loop thread in
    /// the middle of a dispatch. It also asks for strictly less — an email, not
    /// a credential — so it never touches the Keychain, and a slot whose secret
    /// is unreadable still names its owner.
    ///
    /// Only Claude and Codex can answer. The rest have no per-account identity
    /// in comet (opencode's provider store is not an account, `cursor` and
    /// `mock` have none at all), and inventing one would have the guard accuse
    /// somebody on the strength of a placeholder.
    pub fn billed_email(&self, harness: HarnessId, account: Option<&str>) -> Option<String> {
        if !matches!(harness, HarnessId::ClaudeCode | HarnessId::Codex) {
            return None;
        }
        let email = match account.filter(|a| !a.is_empty()) {
            Some(slot) => self
                .read_slots(harness)
                .into_iter()
                .find(|s| s.id == slot)
                .map(|s| s.profile.email),
            None => match harness {
                HarnessId::ClaudeCode => read_json(&self.inner.config.claude_config_file)
                    .as_ref()
                    .and_then(|c| c.get("oauthAccount"))
                    .and_then(|oauth| str_field(oauth, "emailAddress")),
                _ => self.detect_codex().map(|d| d.profile.email),
            },
        };
        email.filter(|e| !e.is_empty())
    }

    /// Does this device's CLI for `harness` have a credential a run could
    /// spend (gh#187)? `None` = the harness has no login concept, so there is
    /// nothing to be signed out of.
    ///
    /// Sync and file-only, for [`AgentAccounts::billed_email`]'s reasons and
    /// one more: this answers a picker, and a picker must not make the operator
    /// wait — nor pop a Keychain prompt for a question nobody asked out loud.
    /// So the Claude arm reads the *identity* file rather than the secret,
    /// which is the same fact [`AgentAccounts::detect_claude`] gates the whole
    /// accounts page on, and it is the one that survives the credentials
    /// living in the macOS Keychain.
    ///
    /// Deliberately lenient: every arm also accepts the CLI's own API-key
    /// environment variable, because a false "signed out" would refuse a
    /// dispatch that would have worked, and this is a *refusal*. Absence of
    /// evidence is only reported where the evidence is the thing the CLI
    /// itself reads.
    pub fn signed_in(&self, harness: HarnessId) -> Option<bool> {
        let env_key = |name: &str| {
            std::env::var_os(name)
                .map(|v| !v.is_empty())
                .unwrap_or(false)
        };
        match harness {
            HarnessId::ClaudeCode => Some(
                read_json(&self.inner.config.claude_config_file)
                    .as_ref()
                    .and_then(|c| c.get("oauthAccount"))
                    .is_some()
                    || self.inner.config.claude_creds_file().exists()
                    || env_key("ANTHROPIC_API_KEY"),
            ),
            HarnessId::Codex => Some(self.detect_codex().is_some() || env_key("OPENAI_API_KEY")),
            // opencode's logins are provider keys rather than an account, and
            // it reads the same provider variables directly — so a box with an
            // Anthropic or OpenAI key in the environment and an empty auth
            // store is signed in as far as a run is concerned.
            HarnessId::Opencode => Some(
                self.detect_opencode().is_some()
                    || env_key("ANTHROPIC_API_KEY")
                    || env_key("OPENAI_API_KEY")
                    || env_key("OPENROUTER_API_KEY"),
            ),
            // No login concept in comet: the mock harness has none at all, and
            // cursor has no adapter to have one.
            HarnessId::Mock | HarnessId::Cursor => None,
        }
    }

    // ── credential freshness (gh#576) ───────────────────────────────────────

    /// Would a run under each saved login work right now?
    ///
    /// The verdict is minted where a timestamp cannot answer it. An access
    /// token whose expiry stamp is still ahead is ok on its face; one past
    /// its stamp is put to the provider with a read-only authenticated GET —
    /// Claude's profile view, Codex's usage view, the same calls their own
    /// CLIs render — before anything is claimed. Accepted ⇒ the login works,
    /// whatever the stamp says; refused ⇒ the next run under it dies at its
    /// first request, which is exactly the fact this exists to name.
    ///
    /// Deliberately read-only: no refresh grant is exercised from here.
    /// Refresh tokens are commonly single-use, so a probe that rotates one
    /// is a *write* to somebody's login, and a freshness check must be safe
    /// to run at any moment — mid-run against a slot an agent is spending
    /// included. Healing an expired-but-refreshable slot stays where it
    /// already lives (the usage refresher); replacing a dead one is what
    /// [`AgentAccounts::start_login`] is for.
    pub async fn health(&self, account_id: Option<&str>) -> Vec<AgentAccountHealth> {
        let snapshot = match self.list(false).await {
            Ok(snapshot) => snapshot,
            // Detection failing wholesale (a config dir gone unreadable) is
            // one unknown answer per known slot, not an error page: doctor
            // renders "could not ask", which is what happened.
            Err(err) => {
                return self
                    .all_slot_ids()
                    .filter(|(id, _)| account_id.is_none_or(|want| want == id.as_str()))
                    .map(|(id, harness)| AgentAccountHealth {
                        id,
                        harness,
                        email: None,
                        state: AgentAccountState::Unknown,
                        detail: format!("could not detect logins: {err}"),
                    })
                    .collect();
            }
        };
        let mut out = Vec::new();
        for account in &snapshot.accounts {
            if let Some(want) = account_id
                && account.id != want
            {
                continue;
            }
            out.push(self.account_health(account).await);
        }
        out
    }

    /// One row of [`AgentAccounts::health`]. Kept small: everything it decides
    /// is either a timestamp comparison or one probe.
    async fn account_health(&self, account: &AgentAccount) -> AgentAccountHealth {
        let health = |state, detail| AgentAccountHealth {
            id: account.id.clone(),
            harness: account.harness,
            email: account.email.clone(),
            state,
            detail,
        };
        match account.harness {
            // Provider keys, not logins: nothing here expires, so there is
            // nothing to verify — saying so beats inventing a green tick.
            HarnessId::Opencode => health(
                AgentAccountState::Ok,
                "provider API keys — nothing to expire".into(),
            ),
            // A live login we know exists but could not read (macOS Keychain
            // denied): the honest answer is that it was not checked.
            _ if !account.switchable && account.saved_at.is_none() => health(
                AgentAccountState::Unknown,
                "credentials present but unreadable — approve the macOS Keychain \
                 prompt, then re-run"
                    .into(),
            ),
            harness @ (HarnessId::ClaudeCode | HarnessId::Codex) => {
                let slot = self
                    .read_slots(harness)
                    .into_iter()
                    .find(|s| s.id == account.id);
                let Some(slot) = slot else {
                    return health(
                        AgentAccountState::Unknown,
                        "slot file missing — refresh the accounts list".into(),
                    );
                };
                if slot.profile.auth_kind == AgentAuthKind::ApiKey {
                    return health(AgentAccountState::Ok, "API key — nothing to expire".into());
                }
                match token_expiry(harness, &slot.credentials) {
                    Some(expires_at) if expires_at > now_ms() + EXPIRY_MARGIN_MS => health(
                        AgentAccountState::Ok,
                        format!("access token until {}", stamp(expires_at)),
                    ),
                    _ => {
                        let (state, detail) = self.probe(harness, &slot).await;
                        health(state, detail)
                    }
                }
            }
            other => health(
                AgentAccountState::Unknown,
                format!("{other:?} has no login flow to verify"),
            ),
        }
    }

    /// Put one slot's stored access token to its provider, read-only. The
    /// three-way outcome maps straight onto [`AgentAccountState`].
    async fn probe(&self, harness: HarnessId, slot: &Slot) -> (AgentAccountState, String) {
        let request = match harness {
            HarnessId::ClaudeCode => {
                let oauth = slot.credentials.get("claudeAiOauth");
                let Some(token) = oauth.and_then(|o| str_field(o, "accessToken")) else {
                    return (
                        AgentAccountState::Unknown,
                        "stored login carries no access token — sign in again".into(),
                    );
                };
                self.inner
                    .http
                    .get(CLAUDE_PROFILE_URL)
                    .bearer_auth(token)
                    .header("anthropic-beta", "oauth-2025-04-20")
            }
            HarnessId::Codex => {
                let tokens = slot.credentials.get("tokens");
                let Some(token) = tokens.and_then(|t| str_field(t, "access_token")) else {
                    return (
                        AgentAccountState::Unknown,
                        "stored login carries no access token — sign in again".into(),
                    );
                };
                self.inner
                    .http
                    .get(CODEX_USAGE_URL)
                    .bearer_auth(token)
                    .header(
                        "chatgpt-account-id",
                        tokens
                            .and_then(|t| str_field(t, "account_id"))
                            .unwrap_or_default(),
                    )
            }
            _ => return (AgentAccountState::Unknown, "nothing to probe".into()),
        };
        match request.send().await {
            Ok(res) if res.status().is_success() => (
                AgentAccountState::Ok,
                "verified: the provider accepted this login just now".into(),
            ),
            Ok(res) if res.status() == 401 || res.status() == 403 => (
                AgentAccountState::Stale,
                format!(
                    "{} refused this login — the next run under it fails. Sign in again",
                    match harness {
                        HarnessId::ClaudeCode => "Anthropic",
                        HarnessId::Codex => "ChatGPT",
                        _ => "the provider",
                    }
                ),
            ),
            Ok(res) => (
                AgentAccountState::Unknown,
                format!("could not ask ({}) — no verdict", res.status()),
            ),
            Err(err) => (
                AgentAccountState::Unknown,
                format!("could not ask ({err}) — no verdict"),
            ),
        }
    }

    /// The named slot's own expiry, OFFLINE — no detection, no probes, no
    /// writes beyond the slot-dir absorption [`AgentAccounts::read_slots`]
    /// already does on any read. What a dying run's transcript attribution is
    /// allowed to claim about the login it spent (gh#576): a fact off the
    /// stored token, never a guess about why the child failed.
    pub fn expired_login(&self, harness: HarnessId, account_id: &str) -> Option<ExpiredLogin> {
        let slot = self
            .read_slots(harness)
            .into_iter()
            .find(|s| s.id == account_id)?;
        let expired_at = token_expiry(harness, &slot.credentials).filter(|at| *at <= now_ms())?;
        Some(ExpiredLogin {
            email: slot.profile.email,
            expired_at,
        })
    }

    /// Every slot id this device holds, with its harness — the fallback
    /// answer space when detection itself failed (see [`AgentAccounts::health`]).
    fn all_slot_ids(&self) -> impl Iterator<Item = (String, HarnessId)> {
        [HarnessId::ClaudeCode, HarnessId::Codex]
            .into_iter()
            .flat_map(move |h| self.read_slots(h).into_iter().map(move |s| (s.id, h)))
            .collect::<Vec<_>>()
            .into_iter()
    }

    // ── detection ───────────────────────────────────────────────────────────

    async fn detect_claude(&self) -> (Option<Detected>, Option<String>) {
        let cfg = read_json(&self.inner.config.claude_config_file);
        let Some(oauth) = cfg.as_ref().and_then(|c| c.get("oauthAccount")).cloned() else {
            return (None, None);
        };
        let Some(email) = str_field(&oauth, "emailAddress") else {
            return (None, None);
        };
        let (credentials, warning) = self.read_claude_credentials().await;
        let user_id = cfg.as_ref().and_then(|c| c.get("userID")).cloned();
        let mut claude_config = serde_json::json!({ "oauthAccount": oauth });
        if let (Some(uid), Some(map)) = (user_id, claude_config.as_object_mut())
            && uid.is_string()
        {
            map.insert("userID".into(), uid);
        }
        (
            Some(Detected {
                account_key: str_field(&oauth, "accountUuid").unwrap_or_else(|| email.clone()),
                profile: SlotProfile {
                    email,
                    display_name: str_field(&oauth, "displayName"),
                    organization: str_field(&oauth, "organizationName"),
                    plan: claude_plan(
                        str_field(&oauth, "organizationType").as_deref(),
                        str_field(&oauth, "organizationRateLimitTier").as_deref(),
                    ),
                    auth_kind: AgentAuthKind::Oauth,
                },
                credentials,
                claude_config: Some(claude_config),
            }),
            warning,
        )
    }

    fn detect_codex(&self) -> Option<Detected> {
        read_json(&self.inner.config.codex_auth_file()).and_then(parse_codex_auth)
    }

    /// opencode detection: its auth store is a map of provider id → credential
    /// (`opencode auth login` manages them); there is no per-account concept,
    /// so a non-empty store is reported as a single detected (non-switchable)
    /// "signed in" account. Credentials stay unread — comet has no opencode
    /// swap/add flows (the CLI owns provider auth), so no secret is needed.
    fn detect_opencode(&self) -> Option<Detected> {
        let auth = read_json(&self.inner.config.opencode_auth_file)?;
        let providers: Vec<String> = auth
            .as_object()?
            .iter()
            .filter(|(_, v)| v.is_object() && v.get("type").is_some())
            .map(|(k, _)| k.clone())
            .collect();
        if providers.is_empty() {
            return None;
        }
        Some(Detected {
            account_key: "opencode".into(),
            profile: SlotProfile {
                email: "opencode CLI".into(),
                display_name: None,
                organization: None,
                plan: Some(format!("{} providers", providers.len())),
                auth_kind: AgentAuthKind::ApiKey,
            },
            credentials: None,
            claude_config: None,
        })
    }

    /// Persist a detected login into its slot (refreshing stored tokens).
    fn snapshot_detected(&self, harness: HarnessId, d: &Detected) -> Result<(), EngineError> {
        let Some(credentials) = &d.credentials else {
            return Ok(());
        };
        self.write_slot(&Slot {
            id: slot_id_for(harness, &d.account_key),
            harness,
            account_key: d.account_key.clone(),
            profile: d.profile.clone(),
            credentials: credentials.clone(),
            claude_config: d.claude_config.clone(),
            saved_at: now_ms(),
            created_at: None,
        })
    }

    // ── Claude credential store (Keychain on macOS, file elsewhere) ─────────

    /// Read the live Claude credentials. `None` payload + warning ⇒ we know a
    /// login exists but couldn't read the secret (Keychain denied us).
    async fn read_claude_credentials(&self) -> (Option<serde_json::Value>, Option<String>) {
        if let Some(creds) = read_json(&self.inner.config.claude_creds_file()) {
            return (Some(creds), None);
        }
        #[cfg(target_os = "macos")]
        {
            return keychain::read_credentials().await;
        }
        #[cfg(not(target_os = "macos"))]
        (None, None)
    }

    async fn write_claude_credentials(
        &self,
        credentials: &serde_json::Value,
    ) -> Result<(), EngineError> {
        let json = credentials.to_string();
        #[cfg(target_os = "macos")]
        {
            // claude-swap's primitive: update the Keychain item in place — but only
            // when no credentials FILE exists (the file wins when present).
            if !self.inner.config.claude_creds_file().exists() {
                return keychain::write_credentials(&json).await;
            }
        }
        std::fs::create_dir_all(&self.inner.config.claude_config_dir)?;
        // Atomic + owner-only from birth — live tokens.
        write_file_atomic(
            &self.inner.config.claude_creds_file(),
            json.as_bytes(),
            true,
        )
    }

    // ── slot files ──────────────────────────────────────────────────────────

    fn slots_dir(&self, harness: HarnessId) -> Result<PathBuf, EngineError> {
        let dir = self.inner.config.root_dir().join(harness.as_str());
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn read_slots(&self, harness: HarnessId) -> Vec<Slot> {
        let Ok(dir) = self.slots_dir(harness) else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut slots: Vec<Slot> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // One malformed slot file must skip THAT slot, not brick the page.
            if let Some(mut slot) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Slot>(&raw).ok())
            {
                // A materialized dir is the live copy: the CLI running against
                // it rotates its own tokens there, and a slot file left at the
                // seeding snapshot would hand every usage probe (and every
                // future re-seed) a refresh token the provider has revoked.
                if let Some(live) = self.materialized(&slot)
                    && live != slot.credentials
                    && freshness(harness, &live) >= freshness(harness, &slot.credentials)
                {
                    slot.credentials = live;
                    slot.saved_at = now_ms();
                    if let Err(err) = self.write_slot(&slot) {
                        tracing::warn!(slot = %slot.id, error = %err, "absorbing account dir failed");
                    }
                }
                slots.push(slot);
            }
        }
        // Creation order — stable across switches (saved_at churns on every
        // auto-snapshot; created_at never does).
        slots.sort_by_key(|s| s.created_at.unwrap_or(s.saved_at));
        slots
    }

    pub(crate) fn write_slot(&self, slot: &Slot) -> Result<(), EngineError> {
        let file = self
            .slots_dir(slot.harness)?
            .join(format!("{}.json", slot.id));
        let existing: Option<Slot> = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let mut full = slot.clone();
        full.created_at = existing
            .and_then(|e| e.created_at.or(Some(e.saved_at)))
            .or(slot.created_at)
            .or(Some(slot.saved_at));
        let json = serde_json::to_string_pretty(&full)
            .map_err(|e| EngineError::Other(format!("serialize slot: {e}")))?;
        // Atomic + 0600 from birth: tokens must never be world-readable, and a
        // crash mid-write must never leave torn JSON.
        write_file_atomic(&file, json.as_bytes(), true)
    }

    // ── remaining usage ─────────────────────────────────────────────────────

    async fn usage_for(
        &self,
        harness: HarnessId,
        slot: &Slot,
        is_active: bool,
        force: bool,
    ) -> Option<Vec<AgentUsageWindow>> {
        let key = format!("{}:{}", harness.as_str(), slot.account_key);
        if let Some((usage, at)) = lock(&self.inner.usage_cache).get(&key)
            && at.elapsed() < USAGE_TTL
        {
            return usage.clone();
        }
        if !force {
            // Non-forced lists never hit the network (see module docs).
            return None;
        }
        let usage = match harness {
            HarnessId::ClaudeCode => self.claude_usage(slot, is_active).await,
            HarnessId::Codex => self.codex_usage(slot).await,
            _ => None,
        };
        lock(&self.inner.usage_cache).insert(key, (usage.clone(), Instant::now()));
        usage
    }

    async fn claude_usage(&self, slot: &Slot, is_active: bool) -> Option<Vec<AgentUsageWindow>> {
        let oauth = slot.credentials.get("claudeAiOauth")?;
        let mut access_token = str_field(oauth, "accessToken")?;
        let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64());
        if let Some(expires_at) = expires_at
            && expires_at < now_ms() + 30_000
        {
            if is_active {
                // The CLI owns this token pair — rotating its refresh token out
                // from under a running Claude Code could force a re-login.
                return None;
            }
            access_token = self.refresh_claude_slot(slot).await?;
        }
        let body: serde_json::Value = self
            .inner
            .http
            .get(CLAUDE_USAGE_URL)
            .bearer_auth(&access_token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        let mut windows = Vec::new();
        for (key, label) in [("five_hour", "Session"), ("seven_day", "Week")] {
            if let Some(w) = body.get(key)
                && let Some(utilization) = w.get("utilization").and_then(|v| v.as_f64())
            {
                windows.push(AgentUsageWindow {
                    label: label.to_string(),
                    used_fraction: (utilization / 100.0) as f32,
                    resets_at: parse_when(w.get("resets_at")),
                });
            }
        }
        (!windows.is_empty()).then_some(windows)
    }

    async fn codex_usage(&self, slot: &Slot) -> Option<Vec<AgentUsageWindow>> {
        let tokens = slot.credentials.get("tokens")?;
        // api-key mode has no ChatGPT rate windows.
        let access_token = str_field(tokens, "access_token")?;
        let body: serde_json::Value = self
            .inner
            .http
            .get(CODEX_USAGE_URL)
            .bearer_auth(&access_token)
            .header(
                "chatgpt-account-id",
                str_field(tokens, "account_id").unwrap_or_default(),
            )
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        let rl = body.get("rate_limit")?;
        let mut windows = Vec::new();
        for key in ["primary_window", "secondary_window"] {
            if let Some(w) = rl.get(key)
                && let Some(used) = w.get("used_percent").and_then(|v| v.as_f64())
            {
                let span = w
                    .get("limit_window_seconds")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                windows.push(AgentUsageWindow {
                    label: if span > 86_400 { "Week" } else { "Session" }.to_string(),
                    used_fraction: (used / 100.0) as f32,
                    resets_at: parse_when(w.get("reset_at")),
                });
            }
        }
        (!windows.is_empty()).then_some(windows)
    }

    /// Refresh a saved Claude slot's expired access token so its usage stays
    /// queryable. NEVER called for the active login, nor for a slot a live run
    /// is pointed at (same reason: that CLI owns the token pair). Single-flight
    /// per slot: OAuth refresh tokens are commonly single-use, and a concurrent
    /// second POST of the same one would revoke the family and brick the slot.
    async fn refresh_claude_slot(&self, slot: &Slot) -> Option<String> {
        if self.is_leased(&slot.id) {
            return None;
        }
        if !lock(&self.inner.inflight_refreshes).insert(slot.id.clone()) {
            return None;
        }
        let result = self.refresh_claude_slot_once(slot).await;
        lock(&self.inner.inflight_refreshes).remove(&slot.id);
        result
    }

    async fn refresh_claude_slot_once(&self, slot: &Slot) -> Option<String> {
        let oauth = slot.credentials.get("claudeAiOauth")?.clone();
        let refresh_token = str_field(&oauth, "refreshToken")?;
        let body: serde_json::Value = self
            .inner
            .http
            .post(CLAUDE_TOKEN_URL)
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": CLAUDE_CLIENT_ID,
            }))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        let access_token = str_field(&body, "access_token")?;
        let expires_in = body
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600);
        let mut updated = oauth;
        if let Some(map) = updated.as_object_mut() {
            map.insert("accessToken".into(), serde_json::json!(access_token));
            map.insert(
                "refreshToken".into(),
                serde_json::json!(str_field(&body, "refresh_token").unwrap_or(refresh_token)),
            );
            map.insert(
                "expiresAt".into(),
                serde_json::json!(now_ms() + expires_in * 1000),
            );
        }
        let mut refreshed = slot.clone();
        refreshed.credentials = serde_json::json!({ "claudeAiOauth": updated });
        refreshed.saved_at = now_ms();
        if let Err(err) = self.write_slot(&refreshed) {
            tracing::warn!(slot = %slot.id, error = %err, "refreshed slot write failed");
        }
        // Keep a materialized dir in step: it is what the next run reads, and
        // leaving the rotated-out refresh token there would fail that run's
        // first refresh.
        if self.materialized(&refreshed).is_some()
            && let Err(err) = self.materialize(refreshed.harness, &refreshed.id)
        {
            tracing::warn!(slot = %slot.id, error = %err, "refreshed account dir write failed");
        }
        Some(access_token)
    }
}

// ── macOS Keychain (documented here; compiled only on macOS) ────────────────
//
// Claude Code stores its credentials in the login Keychain under the service
// `Claude Code-credentials`, account = the current username. Reads use
// `security find-generic-password` — two-step (existence probe needs no
// authorization, then `-w` for the secret) so a user denial is distinguishable
// from "not logged in". Writes use `add-generic-password -U` (update in place).
// Every call is bounded at 15s: an unanswered Keychain consent dialog blocks
// `security` INDEFINITELY, and this runs on every list.
#[cfg(target_os = "macos")]
mod keychain {
    use super::*;

    const EXEC_TIMEOUT: Duration = Duration::from_secs(15);

    async fn exec(args: &[&str]) -> (bool, String, String) {
        let run = tokio::process::Command::new("security")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output();
        match tokio::time::timeout(EXEC_TIMEOUT, run).await {
            Ok(Ok(out)) => (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).to_string(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            ),
            _ => (false, String::new(), "security timed out".into()),
        }
    }

    fn account() -> String {
        std::env::var("USER").unwrap_or_else(|_| "unknown".into())
    }

    pub(super) async fn read_credentials() -> (Option<serde_json::Value>, Option<String>) {
        let (probe_ok, ..) = exec(&["find-generic-password", "-s", KEYCHAIN_SERVICE]).await;
        if !probe_ok {
            return (None, None);
        }
        let (ok, stdout, _) = exec(&[
            "find-generic-password",
            "-a",
            &account(),
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
        ])
        .await;
        if !ok {
            return (
                None,
                Some(
                    "A Claude Code login exists, but macOS Keychain denied access to it — \
                     approve the prompt (choose “Always Allow”) and refresh to enable switching."
                        .into(),
                ),
            );
        }
        match serde_json::from_str(stdout.trim()) {
            Ok(creds) => (Some(creds), None),
            Err(_) => (
                None,
                Some("The Claude Code Keychain entry could not be parsed.".into()),
            ),
        }
    }

    pub(super) async fn write_credentials(json: &str) -> Result<(), EngineError> {
        let (ok, _, stderr) = exec(&[
            "add-generic-password",
            "-U",
            "-a",
            &account(),
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
            json,
        ])
        .await;
        if ok {
            Ok(())
        } else {
            Err(EngineError::Other(format!(
                "Keychain write failed: {}",
                if stderr.trim().is_empty() {
                    "unknown error"
                } else {
                    stderr.trim()
                }
            )))
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn read_json(file: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(file).ok()?;
    serde_json::from_str(&raw)
        .ok()
        .filter(serde_json::Value::is_object)
}

fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Decode a JWT payload without verifying — we only mine identity claims from a
/// token the user's own CLI already trusts.
fn jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = BASE64_URL
        .decode(payload)
        .or_else(|_| BASE64.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Is this a slot id (16 lowercase hex) — the only shape allowed to become a
/// path segment? `account_id` reaches us as a raw RPC string.
fn is_slot_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The `oauthAccount` object for a slot's `.claude.json` — the stored one when
/// the login was captured with it, rebuilt from the profile otherwise.
fn slot_oauth_account(slot: &Slot) -> serde_json::Value {
    slot.claude_config
        .as_ref()
        .and_then(|c| c.get("oauthAccount"))
        .cloned()
        .unwrap_or_else(|| {
            serde_json::json!({
                "accountUuid": slot.account_key,
                "emailAddress": slot.profile.email,
                "organizationName": slot.profile.organization,
                "displayName": slot.profile.display_name,
            })
        })
}

/// How current a credential payload is, in epoch ms — Claude stamps the access
/// token's expiry, Codex the time of its last refresh. `None` for a payload
/// carrying neither (an API key, which never goes stale), which compares as
/// older than anything dated: a dated payload IS newer news.
fn freshness(harness: HarnessId, credentials: &serde_json::Value) -> Option<i64> {
    match harness {
        HarnessId::ClaudeCode => credentials
            .get("claudeAiOauth")
            .and_then(|o| o.get("expiresAt"))
            .and_then(|v| v.as_i64()),
        HarnessId::Codex => credentials
            .get("last_refresh")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp_millis()),
        _ => None,
    }
}

/// When a credential payload's access token stops working, in epoch ms —
/// the freshness question asked of one payload rather than of two.
///
/// Claude stamps it plainly (`claudeAiOauth.expiresAt`). Codex stamps only
/// its own refresh bookkeeping (`last_refresh`), so the expiry is mined out
/// of the access token's own JWT claims, unverified — the same trade
/// [`jwt_claims`] makes: identity facts off a token the CLI already trusts,
/// never an authentication decision.
///
/// `None` = no dated token in the payload (an API key, which does not
/// expire) or no readable stamp — "unknown", which callers report as such
/// rather than as either verdict.
fn token_expiry(harness: HarnessId, credentials: &serde_json::Value) -> Option<i64> {
    match harness {
        HarnessId::ClaudeCode => credentials
            .get("claudeAiOauth")
            .and_then(|o| o.get("expiresAt"))
            .and_then(|v| v.as_i64()),
        HarnessId::Codex => {
            let token = str_field(credentials.get("tokens")?, "access_token")?;
            jwt_claims(&token)?
                .get("exp")
                .and_then(|v| v.as_i64())
                .map(|secs| secs * 1000)
        }
        _ => None,
    }
}

/// A wall-clock moment a person can read. Health lines and re-login
/// transcripts quote these; epoch millis are for machines.
pub(crate) fn stamp(expires_at_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(expires_at_ms)
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| format!("{expires_at_ms}"))
}

/// Should `file` be (re)written from a slot holding `credentials`? Missing is
/// stale; present-and-at-least-as-fresh is not — once the dir exists the CLI
/// owns it, and only a genuinely newer slot (a re-login) may stamp over it.
fn is_stale(file: &Path, harness: HarnessId, credentials: &serde_json::Value) -> bool {
    let Some(live) = read_json(file) else {
        return true;
    };
    if live == *credentials {
        return false;
    }
    freshness(harness, credentials) > freshness(harness, &live)
}

pub(crate) fn slot_id_for(harness: HarnessId, account_key: &str) -> String {
    let digest = Sha256::digest(format!("{}:{account_key}", harness.as_str()).as_bytes());
    crate::repos::hex(&digest)[..16].to_string()
}

/// Pretty plan label from Claude's org type + rate-limit tier ("Max 20×").
fn claude_plan(org_type: Option<&str>, tier: Option<&str>) -> Option<String> {
    let base = match org_type {
        Some("claude_max") => "Max",
        Some("claude_pro") => "Pro",
        Some("claude_team") => "Team",
        Some("claude_enterprise") => "Enterprise",
        _ => return None,
    };
    // "…_20x" style tiers carry a multiplier suffix.
    let mult = tier.and_then(|t| {
        let stem = t.strip_suffix('x')?;
        let digits: String = stem
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let preceded = stem.len() > digits.len()
            && stem.as_bytes().get(stem.len() - digits.len() - 1) == Some(&b'_');
        (!digits.is_empty() && preceded).then_some(digits)
    });
    Some(match mult {
        Some(mult) => format!("{base} {mult}×"),
        None => base.to_string(),
    })
}

fn codex_plan(plan: Option<&str>) -> Option<String> {
    let plan = plan?;
    let mut chars = plan.chars();
    let first = chars.next()?;
    Some(format!(
        "ChatGPT {}{}",
        first.to_uppercase(),
        chars.as_str()
    ))
}

/// Parse a codex `auth.json` (the live one or a fresh login's).
fn parse_codex_auth(auth: serde_json::Value) -> Option<Detected> {
    if let Some(id_token) = auth
        .get("tokens")
        .and_then(|t| t.get("id_token"))
        .and_then(|v| v.as_str())
    {
        let claims = jwt_claims(id_token).unwrap_or_else(|| serde_json::json!({}));
        let oa = claims
            .get("https://api.openai.com/auth")
            .cloned()
            .unwrap_or_default();
        let email = str_field(&claims, "email")?;
        return Some(Detected {
            account_key: str_field(&oa, "chatgpt_account_id").unwrap_or_else(|| email.clone()),
            profile: SlotProfile {
                email,
                display_name: str_field(&claims, "name"),
                organization: None,
                plan: codex_plan(str_field(&oa, "chatgpt_plan_type").as_deref()),
                auth_kind: AgentAuthKind::Oauth,
            },
            credentials: Some(auth),
            claude_config: None,
        });
    }
    let api_key = str_field(&auth, "OPENAI_API_KEY")?;
    let digest = Sha256::digest(api_key.as_bytes());
    let tail: String = api_key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Some(Detected {
        account_key: format!("api-key:{}", &crate::repos::hex(&digest)[..12]),
        profile: SlotProfile {
            email: format!("API key ·…{tail}"),
            display_name: None,
            organization: None,
            plan: Some("API key".into()),
            auth_kind: AgentAuthKind::ApiKey,
        },
        credentials: Some(auth),
        claude_config: None,
    })
}

/// ISO string (Claude) or unix seconds (Codex) → timestamp.
fn parse_when(value: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    match value? {
        serde_json::Value::Number(n) => DateTime::<Utc>::from_timestamp(n.as_i64()?, 0),
        serde_json::Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc)),
        _ => None,
    }
}

fn scan_openai_url(output: &str) -> Option<String> {
    let start = output.find("https://auth.openai.com/")?;
    let rest = &output[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// The one-time code out of a `codex login --device-auth` banner. As of 0.146
/// that banner reads:
///
/// ```text
/// 2. Enter this one-time code (expires in 15 minutes)
///    IELQ-BRG2G
/// ```
///
/// Anchored on the sentence rather than on the code's shape, so a stray
/// uppercase token elsewhere in the output can never be mistaken for it; the
/// shape check then keeps a reworded banner from handing back prose. Expects
/// the ANSI already stripped — codex paints the code cyan.
fn scan_device_code(output: &str) -> Option<String> {
    let rest = output.split_once("one-time code")?.1;
    rest.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| is_device_code(line))
        .map(str::to_string)
}

/// `IELQ-BRG2G`: uppercase alphanumerics in `-`-separated groups. Deliberately
/// loose on group count and length (OpenAI's is 4-5 today) and strict on the
/// alphabet, which is what separates a code from a sentence.
fn is_device_code(line: &str) -> bool {
    let groups: Vec<&str> = line.split('-').collect();
    groups.len() >= 2
        && line.len() <= 24
        && groups.iter().all(|g| {
            g.len() >= 3
                && g.chars()
                    .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
        })
}

/// Terminal colour and cursor control out of captured child output, so the
/// scanners match on text and the app never renders an escape sequence. codex
/// paints both the URL and the code, and `\e[0m` runs right up against them —
/// a URL scan that stops at whitespace would otherwise carry the reset along.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI (`\e[…` terminated by @-~) and OSC (`\e]…` terminated by BEL or
        // ST) cover everything the CLIs emit; any other escape is two chars.
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The last thing a child said, as a human-readable line — what a failed login
/// gets to explain itself with. Stripped and trimmed; `None` when it said
/// nothing at all.
fn last_output_line(output: &str) -> Option<String> {
    strip_ansi(output)
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
}

/// Minimal percent-encoding for OAuth query params (matches `encodeURIComponent`
/// for the constant inputs used here).
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Atomic write via a same-dir temp file + rename; `secret` = 0600 from birth.
fn write_file_atomic(file: &Path, bytes: &[u8], secret: bool) -> Result<(), EngineError> {
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if secret {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = secret;
        let mut handle = options.open(&tmp)?;
        handle.write_all(bytes)?;
    }
    std::fs::rename(&tmp, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_labels() {
        assert_eq!(
            claude_plan(Some("claude_max"), Some("default_claude_max_20x")).as_deref(),
            Some("Max 20×")
        );
        assert_eq!(
            claude_plan(Some("claude_pro"), None).as_deref(),
            Some("Pro")
        );
        assert_eq!(
            claude_plan(Some("claude_team"), Some("weird")).as_deref(),
            Some("Team")
        );
        assert_eq!(claude_plan(Some("free"), None), None);
        assert_eq!(codex_plan(Some("plus")).as_deref(), Some("ChatGPT Plus"));
        assert_eq!(codex_plan(None), None);
    }

    #[test]
    fn openai_url_scan() {
        assert_eq!(
            scan_openai_url("open https://auth.openai.com/authorize?x=1 in your browser\n")
                .as_deref(),
            Some("https://auth.openai.com/authorize?x=1")
        );
        assert_eq!(scan_openai_url("nothing here"), None);
    }

    /// Verbatim `codex login --device-auth` (0.146.0), colour and all — the
    /// banner these scanners exist to read. Captured, not composed.
    const DEVICE_AUTH_BANNER: &str = "\nWelcome to Codex [v\u{1b}[90m0.146.0\u{1b}[0m]\n\
        \u{1b}[90mOpenAI's command-line coding agent\u{1b}[0m\n\n\
        Follow these steps to sign in with ChatGPT using device code authorization:\n\n\
        1. Open this link in your browser and sign in to your account\n   \
        \u{1b}[94mhttps://auth.openai.com/codex/device\u{1b}[0m\n\n\
        2. Enter this one-time code \u{1b}[90m(expires in 15 minutes)\u{1b}[0m\n   \
        \u{1b}[94mIELQ-BRG2G\u{1b}[0m\n\n\
        \u{1b}[90mContinue only if you started this login in Codex. If a website or another \
        person gave you this code, cancel.\u{1b}[0m\n\n";

    #[test]
    fn device_auth_banner_yields_code_and_url() {
        let banner = strip_ansi(DEVICE_AUTH_BANNER);
        assert_eq!(scan_device_code(&banner).as_deref(), Some("IELQ-BRG2G"));
        // The reset sequence sits flush against the URL — without stripping,
        // "stop at whitespace" swallows it and the link is dead.
        assert_eq!(
            scan_openai_url(&banner).as_deref(),
            Some("https://auth.openai.com/codex/device")
        );
        assert!(
            scan_openai_url(DEVICE_AUTH_BANNER)
                .unwrap()
                .contains('\u{1b}')
        );
    }

    #[test]
    fn device_code_scan_is_anchored_and_shaped() {
        // Uppercase tokens before the anchor are not the code.
        assert_eq!(
            scan_device_code("Welcome to CODEX-CLI\none-time code\n  WXYZ-12345\n").as_deref(),
            Some("WXYZ-12345")
        );
        // A reworded banner that stops printing a code hands back nothing
        // rather than a sentence.
        assert_eq!(
            scan_device_code("Enter this one-time code in your browser to continue\n"),
            None
        );
        // Loopback output has no anchor at all.
        assert_eq!(scan_device_code("Starting local login server…"), None);
    }

    #[test]
    fn ansi_stripping_and_last_line() {
        assert_eq!(strip_ansi("\u{1b}[94mplain\u{1b}[0m"), "plain");
        // OSC 8 hyperlinks (BEL- and ST-terminated) leave only their label.
        assert_eq!(
            strip_ansi("\u{1b}]8;;https://x\u{7}label\u{1b}]8;;\u{1b}\\"),
            "label"
        );
        assert_eq!(
            last_output_line("\u{1b}[31mrequest failed\u{1b}[0m\n\n").as_deref(),
            Some("request failed")
        );
        assert_eq!(last_output_line("   \n\n"), None);
    }

    #[test]
    fn urlencode_matches_encode_uri_component() {
        assert_eq!(
            urlencode("org:create_api_key user:profile"),
            "org%3Acreate_api_key%20user%3Aprofile"
        );
        assert_eq!(urlencode("https://a/b"), "https%3A%2F%2Fa%2Fb");
    }

    // ── per-run account dirs (gh#59) ────────────────────────────────────────

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "comet-accounts-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An accounts service whose every path is inside `dir` — nothing in these
    /// tests may see, let alone write, the developer's real `~/.claude`.
    fn accounts(dir: &Path) -> AgentAccounts {
        AgentAccounts::new(AgentAccountsConfig {
            data_dir: dir.to_path_buf(),
            claude_config_dir: dir.join("live-claude"),
            claude_config_file: dir.join("live-claude").join(".claude.json"),
            codex_home: dir.join("live-codex"),
            opencode_auth_file: dir.join("live-opencode").join("auth.json"),
        })
    }

    fn claude_creds(expires_at: i64, refresh: &str) -> serde_json::Value {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "at",
                "refreshToken": refresh,
                "expiresAt": expires_at,
            }
        })
    }

    fn claude_slot(id_key: &str, credentials: serde_json::Value) -> Slot {
        Slot {
            id: slot_id_for(HarnessId::ClaudeCode, id_key),
            harness: HarnessId::ClaudeCode,
            account_key: id_key.to_string(),
            profile: SlotProfile {
                email: format!("{id_key}@example.com"),
                display_name: None,
                organization: None,
                plan: None,
                auth_kind: AgentAuthKind::Oauth,
            },
            credentials,
            claude_config: None,
            saved_at: 1,
            created_at: Some(1),
        }
    }

    /// The whole point: a slot becomes a config dir of its own, holding both
    /// files `CLAUDE_CONFIG_DIR` relocates.
    #[test]
    fn materializing_a_slot_seeds_its_own_config_dir() {
        let tmp = TempDir::new("materialize");
        let service = accounts(&tmp.0);
        let slot = claude_slot("teammate-a", claude_creds(9_000, "r1"));
        service.write_slot(&slot).unwrap();

        let dir = service
            .materialize(HarnessId::ClaudeCode, &slot.id)
            .unwrap();
        assert_eq!(dir, tmp.0.join("accounts").join(&slot.id));
        assert_eq!(
            read_json(&dir.join(".credentials.json")).unwrap(),
            slot.credentials
        );
        // The identity file too — without it the CLI in that dir has tokens
        // but no account, and re-onboards.
        let identity = read_json(&dir.join(".claude.json")).unwrap();
        assert_eq!(
            identity["oauthAccount"]["emailAddress"],
            "teammate-a@example.com"
        );
        // And the live config dir is untouched: that is the swap this replaces.
        assert!(!tmp.0.join("live-claude").exists());
    }

    /// gh#133: the slot dir is the dispatched agent's whole `CLAUDE_CONFIG_DIR`,
    /// so the board's skill has to be in it — the copy under `~/.claude` is the
    /// one thing a slot run cannot see.
    #[test]
    fn a_materialized_slot_carries_the_board_skill() {
        let tmp = TempDir::new("skill");
        let service = accounts(&tmp.0);
        let slot = claude_slot("teammate-a", claude_creds(9_000, "r1"));
        service.write_slot(&slot).unwrap();

        let dir = service
            .materialize(HarnessId::ClaudeCode, &slot.id)
            .unwrap();
        assert!(comet_board::skill::status_of(&dir).is_current());
        // Same call again on the next dispatch: nothing rewritten under a CLI
        // that may be reading it.
        let before = std::fs::metadata(comet_board::skill::path_in(&dir))
            .and_then(|m| m.modified())
            .unwrap();
        service
            .materialize(HarnessId::ClaudeCode, &slot.id)
            .unwrap();
        let after = std::fs::metadata(comet_board::skill::path_in(&dir))
            .and_then(|m| m.modified())
            .unwrap();
        assert_eq!(before, after);
    }

    /// Two teammates, two dirs. One box, two subscriptions.
    #[test]
    fn each_slot_gets_a_dir_of_its_own() {
        let tmp = TempDir::new("two-slots");
        let service = accounts(&tmp.0);
        let a = claude_slot("teammate-a", claude_creds(9_000, "ra"));
        let b = claude_slot("teammate-b", claude_creds(9_000, "rb"));
        service.write_slot(&a).unwrap();
        service.write_slot(&b).unwrap();

        let dir_a = service.materialize(HarnessId::ClaudeCode, &a.id).unwrap();
        let dir_b = service.materialize(HarnessId::ClaudeCode, &b.id).unwrap();
        assert_ne!(dir_a, dir_b);
        assert_eq!(
            read_json(&dir_a.join(".credentials.json")).unwrap()["claudeAiOauth"]["refreshToken"],
            "ra"
        );
        assert_eq!(
            read_json(&dir_b.join(".credentials.json")).unwrap()["claudeAiOauth"]["refreshToken"],
            "rb"
        );
    }

    /// Once the dir exists the CLI owns it: re-materializing must not stamp a
    /// stale refresh token over the one the CLI rotated to mid-run.
    #[test]
    fn re_materializing_does_not_clobber_the_clis_own_refresh() {
        let tmp = TempDir::new("no-clobber");
        let service = accounts(&tmp.0);
        let slot = claude_slot("teammate-a", claude_creds(1_000, "old"));
        service.write_slot(&slot).unwrap();
        let dir = service
            .materialize(HarnessId::ClaudeCode, &slot.id)
            .unwrap();

        // The CLI refreshes: newer expiry, new refresh token.
        let rotated = claude_creds(50_000, "rotated");
        write_file_atomic(
            &dir.join(".credentials.json"),
            rotated.to_string().as_bytes(),
            true,
        )
        .unwrap();

        service
            .materialize(HarnessId::ClaudeCode, &slot.id)
            .unwrap();
        assert_eq!(read_json(&dir.join(".credentials.json")).unwrap(), rotated);

        // …and the slot file absorbs it, so the next probe (and the next
        // re-seed) uses the token that actually works.
        let stored = service
            .read_slots(HarnessId::ClaudeCode)
            .into_iter()
            .find(|s| s.id == slot.id)
            .unwrap();
        assert_eq!(stored.credentials, rotated);
    }

    /// A genuinely newer slot — a re-login through the accounts UI — does win.
    #[test]
    fn a_fresher_slot_re_seeds_the_dir() {
        let tmp = TempDir::new("re-seed");
        let service = accounts(&tmp.0);
        let mut slot = claude_slot("teammate-a", claude_creds(1_000, "old"));
        service.write_slot(&slot).unwrap();
        let dir = service
            .materialize(HarnessId::ClaudeCode, &slot.id)
            .unwrap();

        slot.credentials = claude_creds(99_000, "fresh");
        service.write_slot(&slot).unwrap();
        service
            .materialize(HarnessId::ClaudeCode, &slot.id)
            .unwrap();
        assert_eq!(
            read_json(&dir.join(".credentials.json")).unwrap()["claudeAiOauth"]["refreshToken"],
            "fresh"
        );
    }

    #[test]
    fn an_unknown_or_malformed_account_is_refused_by_name() {
        let tmp = TempDir::new("unknown");
        let service = accounts(&tmp.0);
        let err = service
            .materialize(HarnessId::ClaudeCode, "0123456789abcdef")
            .unwrap_err()
            .to_string();
        assert!(err.contains("0123456789abcdef"), "{err}");
        // A path traversal never reaches the filesystem.
        let err = service
            .materialize(HarnessId::ClaudeCode, "../../etc")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not an agent account id"), "{err}");
        assert!(!tmp.0.join("accounts").join("..").exists());
    }

    /// Forgetting an account has to take the live copy too, or the id keeps
    /// working for dispatches after the page says it is gone.
    #[tokio::test]
    async fn forgetting_an_account_removes_its_dir() {
        let tmp = TempDir::new("forget");
        let service = accounts(&tmp.0);
        let slot = claude_slot("teammate-a", claude_creds(9_000, "r1"));
        service.write_slot(&slot).unwrap();
        let dir = service
            .materialize(HarnessId::ClaudeCode, &slot.id)
            .unwrap();
        assert!(dir.exists());

        service
            .forget(HarnessId::ClaudeCode, &slot.id)
            .await
            .unwrap();
        assert!(!dir.exists());
    }

    /// A run holds its account: the usage refresher must not rotate a token
    /// the CLI in that dir is still using.
    #[test]
    fn a_leased_account_is_left_alone_and_released_on_drop() {
        let tmp = TempDir::new("lease");
        let service = accounts(&tmp.0);
        assert!(!service.is_leased("abc"));
        {
            let _lease = service.lease("abc");
            assert!(service.is_leased("abc"));
            // Two runs on one account: the first to finish must not release it.
            let _second = service.lease("abc");
            assert!(service.is_leased("abc"));
        }
        assert!(!service.is_leased("abc"));
    }

    #[test]
    fn freshness_reads_both_providers_stamps() {
        assert_eq!(
            freshness(HarnessId::ClaudeCode, &claude_creds(42, "r")),
            Some(42)
        );
        assert_eq!(
            freshness(
                HarnessId::Codex,
                &serde_json::json!({ "last_refresh": "2026-08-04T10:00:00Z" })
            ),
            Some(1_785_837_600_000)
        );
        // An API key carries no stamp and never goes stale.
        assert_eq!(
            freshness(
                HarnessId::Codex,
                &serde_json::json!({ "OPENAI_API_KEY": "k" })
            ),
            None
        );
    }

    // ── credential freshness (gh#576) ───────────────────────────────────────

    /// A codex access token whose `exp` claim is the only stamp there is:
    /// mined unverified, like every other identity fact off a JWT.
    #[test]
    fn codex_expiry_comes_from_the_tokens_own_claims() {
        // exp = 2100-01-01, base64url payload {"exp":4070908800}.
        let jwt = "header.eyJleHAiOjQwNzA5MDg4MDB9.signature";
        assert_eq!(
            token_expiry(
                HarnessId::Codex,
                &serde_json::json!({ "tokens": { "access_token": jwt } })
            ),
            Some(4_070_908_800_000)
        );
        // No JWT, no verdict — reported as unknown, never as either answer.
        assert_eq!(
            token_expiry(
                HarnessId::Codex,
                &serde_json::json!({ "tokens": { "access_token": "garbage" } })
            ),
            None
        );
    }

    /// The offline half of the transcript attribution: a slot whose stored
    /// token is past its stamp is named; one that still has time on it, an
    /// API key, or an id nothing on this device holds is not.
    #[test]
    fn expired_login_names_only_a_past_its_stamp_slot() {
        let tmp = TempDir::new("expired-login");
        let service = accounts(&tmp.0);
        let dead = claude_slot("teammate-a", claude_creds(1_000, "ra"));
        let alive = claude_slot("teammate-b", claude_creds(now_ms() + 3_600_000, "rb"));
        service.write_slot(&dead).unwrap();
        service.write_slot(&alive).unwrap();

        let expired = service
            .expired_login(HarnessId::ClaudeCode, &dead.id)
            .expect("the stale slot is named");
        assert_eq!(expired.email, "teammate-a@example.com");
        assert_eq!(expired.expired_at, 1_000);

        assert_eq!(
            service.expired_login(HarnessId::ClaudeCode, &alive.id),
            None,
            "a token with time left is not expired"
        );
        assert_eq!(
            service.expired_login(HarnessId::ClaudeCode, "nonexistent000"),
            None,
            "an id this device does not hold claims nothing"
        );
    }

    /// An API-key slot never reads as expired, whatever its file looks like.
    #[test]
    fn an_api_key_slot_is_never_expired() {
        let tmp = TempDir::new("api-key-slot");
        let service = accounts(&tmp.0);
        let mut slot = claude_slot(
            "keyed",
            serde_json::json!({ "claudeAiOauth": { "accessToken": "at" } }),
        );
        slot.profile.auth_kind = AgentAuthKind::ApiKey;
        service.write_slot(&slot).unwrap();
        // The payload carries no expiry at all, so there is nothing to be past.
        assert_eq!(service.expired_login(HarnessId::ClaudeCode, &slot.id), None);
    }
}
