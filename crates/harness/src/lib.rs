//! comet-harness — one interface over Claude Code / Codex (and a mock for tests).
//!
//! Integration decisions (docs/research/harness.md):
//! - Claude Code: spawn the installed `claude` CLI with
//!   `--input-format stream-json --output-format stream-json --verbose
//!    --include-partial-messages`, implement the control channel (can_use_tool →
//!   requestInput, interrupt, set_model), steer by writing user lines mid-run.
//! - Codex: spawn `codex app-server`, JSON-RPC 2.0 over stdio (thread/start, turn/start,
//!   turn/steer{expectedTurnId}, turn/interrupt, item/* + delta notifications).

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::{mpsc, oneshot};
pub use tokio_util::sync::CancellationToken;

use comet_proto::{
    AgentEvent, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode, UserInputAnswer,
    UserInputQuestion,
};

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("harness binary not found: {0}")]
    NotInstalled(String),
    #[error("harness protocol error: {0}")]
    Protocol(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A steer prompt pushed into a live run; delivered at the harness's steering boundary.
pub struct SteerMessage {
    pub prompt: String,
    pub message_id: Option<String>,
}

/// The agent account a run is pointed at: a config dir of its own, holding one
/// teammate's login (comet-engine's `agent_accounts`, gh#59).
///
/// Both CLIs relocate their whole config dir on one environment variable —
/// `CLAUDE_CONFIG_DIR` for Claude Code, `CODEX_HOME` for Codex — so pointing
/// the child at a per-account dir is all it takes for a dispatch to burn its
/// owner's subscription. The alternative the engine used to have (swapping the
/// credential files under `~`) is engine-wide and mutates what a live run is
/// reading; this is per-child and cannot.
#[derive(Debug, Clone)]
pub struct AgentAccount {
    /// Slot id, for logging and the attempt row.
    pub id: String,
    /// The harness the slot's login belongs to. A run only applies an account
    /// its own harness owns — a Claude slot says nothing about `CODEX_HOME`.
    pub harness: HarnessId,
    /// The materialized config dir the child is pointed at.
    pub dir: std::path::PathBuf,
}

impl AgentAccount {
    /// Stamp the child's env for `harness`, if this account is that harness's.
    pub(crate) fn apply(&self, cmd: &mut tokio::process::Command, harness: HarnessId) {
        if self.harness != harness {
            return;
        }
        match harness {
            HarnessId::ClaudeCode => {
                cmd.env("CLAUDE_CONFIG_DIR", &self.dir);
            }
            HarnessId::Codex => {
                cmd.env("CODEX_HOME", &self.dir);
            }
            _ => {}
        }
    }
}

/// Git and `gh` credentials for a dispatched agent's own pushes (comet-board's
/// `git_credentials`, gh#68).
///
/// Nothing in here is a secret: `env` is askpass wiring (which helper to run,
/// which repo it is for, terminal prompting off) and `bin_dir` holds a `gh`
/// wrapper. Both mint at the moment the tool asks, so a run that lasts longer
/// than the hour an installation token lives still pushes and still opens its
/// pull request. Absent means the agent uses the box's own git credentials,
/// which is every chat the board did not dispatch.
#[derive(Debug, Clone, Default)]
pub struct PushCredentials {
    /// `name=value` pairs stamped on the child.
    pub env: Vec<(String, String)>,
    /// Prepended to the child's PATH — the directory holding the `gh` shim.
    pub bin_dir: Option<std::path::PathBuf>,
}

impl PushCredentials {
    /// Stamp the child's env. Called last of everything that shapes PATH, so
    /// the shim lands in front of whatever the rest put there — neither a `gh`
    /// beside the harness CLI nor one beside the engine may win over the one
    /// that carries the credential.
    pub(crate) fn apply(&self, cmd: &mut tokio::process::Command) {
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        let Some(dir) = &self.bin_dir else { return };
        prepend_dirs_to_path(cmd, std::slice::from_ref(dir));
    }
}

/// Host-side controls handed to a run: input-request bridge + steering mailbox.
pub struct RunControls {
    /// The run sends questions and awaits answers (blocks the agent, mirrors comet).
    pub request_input: Box<
        dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync,
    >,
    /// Steer prompts consumed at step/turn boundaries.
    pub steering: mpsc::Receiver<SteerMessage>,
    /// Cancel to interrupt the live run: the harness sends its protocol-level
    /// interrupt, then escalates to SIGTERM/SIGKILL on the child after a grace
    /// period. The run's stream ends with `Done { status: Interrupted }`.
    pub interrupt: CancellationToken,
    /// The chat this run serves, exported into the harness child's env as
    /// `COMET_BOARD_CHAT_ID` — the identity `HERDR_PANE_ID` carried in herdr:
    /// a `comet-board dispatch` run by the agent inherits its own chat id
    /// without anyone passing ids by hand (§dispatch-pipeline). Every chat's
    /// runs get it, not only board-dispatched ones — an orchestrator the board
    /// never dispatched still names itself. `None` only for chat-less runs
    /// (title generation).
    pub chat_id: Option<String>,
    /// Which login this run spends. `None` = whatever the CLI's own config dir
    /// holds (the single-user default, and every run before gh#59).
    pub account: Option<AgentAccount>,
    /// What this run pushes with. `None` = the box user's git credentials,
    /// which is every chat the board did not dispatch (gh#68).
    pub push: Option<PushCredentials>,
    /// Directories prepended to the child's PATH so the agent can run the
    /// tools the engine shipped with — `comet-board` above all (gh#184).
    ///
    /// The engine's own app directory, in practice. On the box that is
    /// `~/.comet-native/app/<version>/`, which since gh#156 holds both
    /// binaries; nothing else puts it on an agent's PATH. `install.sh` links
    /// the CLI into `~/.local/bin`, which a systemd **user** service does not
    /// inherit and a non-interactive ssh shell never sources — so every
    /// `comet-board` an agent typed on the box was `command not found`, and an
    /// agent that cannot reach the board does not fail, it just quietly stops
    /// checking `dispatchable`, releasing sub-work, and waiting.
    ///
    /// Set on every run, not only board-dispatched ones: the skill is
    /// installed for the whole box, so an orchestrator the board never
    /// dispatched reads the same page of verbs. Empty when the engine cannot
    /// find a `comet-board` beside itself, which leaves PATH exactly as it was.
    ///
    /// Applied *before* [`PushCredentials`], whose `gh` shim has to stay in
    /// front of everything.
    pub bin_dirs: Vec<std::path::PathBuf>,
    /// Process-local MCP stdio servers for this chat (gh#273). Harness
    /// adapters translate this one shared description into their native
    /// launch configuration; account config dirs remain untouched.
    pub mcp_servers: Vec<comet_proto::McpServer>,
}

#[async_trait]
pub trait Harness: Send + Sync {
    fn id(&self) -> HarnessId;
    fn display_name(&self) -> &str;
    fn supports_steering(&self) -> bool;
    fn steering_mode(&self) -> SteeringMode;
    fn reasoning_levels(&self) -> &[ReasoningLevel];
    async fn models(&self) -> Result<Vec<Model>, HarnessError>;
    /// Run one (persistent) session; the stream ends with `AgentEvent::Done`.
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError>;
}

pub mod claude;
pub mod codex;
/// Shared JSON-RPC-over-stdio client: the codex app-server speaks it today,
/// and the Agent Client Protocol is the same wire shape (docs/research/acp.md).
pub mod jsonrpc;
pub mod mock;
pub mod opencode;

/// Where this device's CLI for a harness is, asked *without* spawning it
/// (gh#187).
///
/// Every adapter already resolves its binary the same way — env override,
/// PATH, then the install locations a GUI launch cannot see — and then fails
/// the run with [`HarnessError::NotInstalled`] when it finds nothing. That is
/// the right answer at the wrong time: by then a board dispatch has cut a
/// worktree and made a chat. This is the same lookup, one step earlier, for
/// anything that wants to know before it spends something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessCli {
    /// Found, at this path.
    Found(std::path::PathBuf),
    /// A CLI this harness needs, nowhere the resolver looks.
    Missing,
    /// The harness needs no CLI at all — the mock runs in-process.
    NotNeeded,
    /// This build has no adapter for the harness, so there is nothing to look
    /// for. `Cursor` is the whole of it today: it is in the id enum and in the
    /// board's runtime table, and no adapter has ever been written.
    Unsupported,
}

/// Locate the CLI a harness spawns. Cheap (stat calls), so callers answering a
/// picker may do it per option rather than caching.
pub fn locate_cli(harness: HarnessId) -> HarnessCli {
    let found = match harness {
        HarnessId::ClaudeCode => claude::resolve_claude_executable(),
        HarnessId::Codex => codex::resolve_codex_executable(),
        HarnessId::Opencode => opencode::resolve_opencode_executable(),
        HarnessId::Mock => return HarnessCli::NotNeeded,
        HarnessId::Cursor => return HarnessCli::Unsupported,
    };
    match found {
        Some(path) => HarnessCli::Found(path),
        None => HarnessCli::Missing,
    }
}

/// Bin directories where npm-installed CLIs land under Node version managers.
/// GUI launches never see these on PATH — the managers shape PATH in shell
/// init (fnm's per-shell multishells, nvm's shell function), which a
/// Dock/Finder-launched app never runs.
pub(crate) fn node_version_manager_bins() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = Vec::new();
    // fnm: `aliases/default` is a stable symlink to the active default
    // installation (the multishell PATH entries are ephemeral, per-shell).
    let mut fnm_roots: Vec<PathBuf> = std::env::var_os("FNM_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Some(home) = &home {
        fnm_roots.push(home.join(".local").join("share").join("fnm"));
        fnm_roots.push(home.join("Library").join("Application Support").join("fnm"));
        fnm_roots.push(home.join(".fnm"));
    }
    for root in fnm_roots {
        dirs.push(root.join("aliases").join("default").join("bin"));
    }
    if let Some(home) = &home {
        // volta / bun keep real shims in a fixed bin dir; pnpm has a global bin.
        dirs.push(home.join(".volta").join("bin"));
        dirs.push(home.join(".bun").join("bin"));
        dirs.push(home.join("Library").join("pnpm"));
        dirs.push(home.join(".local").join("share").join("pnpm"));
        // nvm: every installed version's bin, newest first.
        let nvm = home.join(".nvm").join("versions").join("node");
        if let Ok(entries) = std::fs::read_dir(&nvm) {
            let mut versions: Vec<PathBuf> =
                entries.flatten().map(|e| e.path().join("bin")).collect();
            versions.sort();
            versions.reverse();
            dirs.append(&mut versions);
        }
    }
    dirs
}

/// Prepend the resolved executable's directory to the child's PATH. npm-shim
/// CLIs are `#!/usr/bin/env node` scripts whose `node` lives beside them in
/// the version manager's bin dir — a dir the GUI process's own PATH lacks.
pub(crate) fn prepend_exe_dir_to_path(cmd: &mut tokio::process::Command, exe: &std::path::Path) {
    let Some(dir) = exe.parent().filter(|d| !d.as_os_str().is_empty()) else {
        return;
    };
    prepend_dirs_to_path(cmd, std::slice::from_ref(&dir.to_path_buf()));
}

/// Put `dirs` at the front of the PATH the child will be spawned with, in the
/// order given.
///
/// Everything that shapes a child's PATH goes through here, because the order
/// they run in is the order the entries end up in and each layer has to keep
/// the ones before it. The command's own `PATH` is read back first and only
/// then the process's: an earlier layer has already set one on the command,
/// and starting from the process's would silently discard it.
///
/// No dirs is not "an empty entry at the front" — it leaves PATH untouched.
pub(crate) fn prepend_dirs_to_path(cmd: &mut tokio::process::Command, dirs: &[std::path::PathBuf]) {
    if dirs.is_empty() {
        return;
    }
    let current = cmd
        .as_std()
        .get_envs()
        .find(|(k, _)| *k == std::ffi::OsStr::new("PATH"))
        .and_then(|(_, v)| v.map(|v| v.to_os_string()))
        .or_else(|| std::env::var_os("PATH"));
    let mut paths = dirs.to_vec();
    if let Some(path) = current {
        paths.extend(std::env::split_paths(&path));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env("PATH", joined);
    }
}

/// Rolling tail of a child's stderr, shared between the reader task and the
/// crash-message composer: an unexpected exit surfaces "<name> exited
/// unexpectedly (<status>): <last stderr lines>" instead of a bare shrug —
/// the proper background-crash message old comet showed (user requirement).
#[derive(Clone, Default)]
pub(crate) struct StderrTail(
    std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
);

impl StderrTail {
    const KEEP_LINES: usize = 6;
    const KEEP_BYTES: usize = 700;

    pub(crate) fn push(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let mut tail = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        tail.push_back(line.chars().take(Self::KEEP_BYTES).collect());
        while tail.len() > Self::KEEP_LINES {
            tail.pop_front();
        }
    }

    /// The captured tail as one display string, `None` when nothing arrived.
    pub(crate) fn snapshot(&self) -> Option<String> {
        let tail = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if tail.is_empty() {
            return None;
        }
        let mut joined = tail.iter().cloned().collect::<Vec<_>>().join("\n");
        joined.truncate(Self::KEEP_BYTES * 2);
        Some(joined)
    }
}

/// "exit code 137" / "signal 9 (killed)" / "unknown" — the status half of a
/// crash message, from a `try_wait` result after the stream ended.
pub(crate) fn describe_exit(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "still running".into();
    };
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    "unknown exit".into()
}

/// The full crash message: status plus the stderr tail when there is one.
pub(crate) fn crash_message(
    name: &str,
    status: Option<std::process::ExitStatus>,
    stderr: &StderrTail,
) -> String {
    let status = describe_exit(status);
    match stderr.snapshot() {
        Some(tail) => format!("{name} exited unexpectedly ({status}): {tail}"),
        None => format!("{name} exited unexpectedly ({status})"),
    }
}

pub use claude::ClaudeHarness;
pub use codex::CodexHarness;
pub use opencode::OpencodeHarness;

#[cfg(test)]
mod tests {
    use super::*;

    /// The env a command would spawn with, as `(name, value)` pairs.
    fn envs(cmd: &tokio::process::Command) -> Vec<(String, String)> {
        cmd.as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect()
    }

    fn account(harness: HarnessId) -> AgentAccount {
        AgentAccount {
            id: "8f2c1d0a7b6e4539".into(),
            harness,
            dir: std::path::PathBuf::from("/data/accounts/8f2c1d0a7b6e4539"),
        }
    }

    #[test]
    fn each_cli_gets_the_variable_that_relocates_its_config_dir() {
        let mut claude = tokio::process::Command::new("claude");
        account(HarnessId::ClaudeCode).apply(&mut claude, HarnessId::ClaudeCode);
        assert_eq!(
            envs(&claude),
            vec![(
                "CLAUDE_CONFIG_DIR".to_string(),
                "/data/accounts/8f2c1d0a7b6e4539".to_string()
            )]
        );

        let mut codex = tokio::process::Command::new("codex");
        account(HarnessId::Codex).apply(&mut codex, HarnessId::Codex);
        assert_eq!(
            envs(&codex),
            vec![(
                "CODEX_HOME".to_string(),
                "/data/accounts/8f2c1d0a7b6e4539".to_string()
            )]
        );
    }

    /// The shim has to win over every other `gh` the child can see — including
    /// the one that might sit beside the harness CLI, which the adapters have
    /// already put at the front of PATH by the time this runs.
    #[test]
    fn the_push_shim_goes_in_front_of_the_path_the_adapter_built() {
        let mut cmd = tokio::process::Command::new("claude");
        prepend_exe_dir_to_path(&mut cmd, std::path::Path::new("/opt/node/bin/claude"));
        PushCredentials {
            env: vec![("COMET_BOARD_ASKPASS_REPO".into(), "o/r".into())],
            bin_dir: Some(std::path::PathBuf::from("/data/board/state/bin")),
        }
        .apply(&mut cmd);

        let env: std::collections::BTreeMap<_, _> = envs(&cmd).into_iter().collect();
        assert_eq!(
            env.get("COMET_BOARD_ASKPASS_REPO").map(String::as_str),
            Some("o/r")
        );
        let path = env.get("PATH").expect("PATH");
        let dirs: Vec<&str> = path.split(':').collect();
        assert_eq!(dirs[0], "/data/board/state/bin");
        // …and the adapter's own entry is still there, behind it.
        assert_eq!(dirs[1], "/opt/node/bin");
    }

    /// A run with no credentials leaves PATH exactly as the adapter built it —
    /// no empty entry, no reordering.
    #[test]
    fn no_shim_means_no_path_change() {
        let mut cmd = tokio::process::Command::new("claude");
        prepend_exe_dir_to_path(&mut cmd, std::path::Path::new("/opt/node/bin/claude"));
        let before = envs(&cmd);
        PushCredentials {
            env: vec![("GIT_TERMINAL_PROMPT".into(), "0".into())],
            bin_dir: None,
        }
        .apply(&mut cmd);
        let after: std::collections::BTreeMap<_, _> = envs(&cmd).into_iter().collect();
        let before: std::collections::BTreeMap<_, _> = before.into_iter().collect();
        assert_eq!(after.get("PATH"), before.get("PATH"));
    }

    /// gh#184: the engine's own app directory reaches the child, so an agent
    /// can type `comet-board` — and the `gh` shim still wins, because the two
    /// are applied in the order the adapters apply them.
    #[test]
    fn the_engines_app_dir_reaches_the_child_behind_the_push_shim() {
        let mut cmd = tokio::process::Command::new("claude");
        prepend_exe_dir_to_path(&mut cmd, std::path::Path::new("/opt/node/bin/claude"));
        prepend_dirs_to_path(
            &mut cmd,
            &[std::path::PathBuf::from(
                "/home/comet/.comet-native/app/0.3.5",
            )],
        );
        PushCredentials {
            env: Vec::new(),
            bin_dir: Some(std::path::PathBuf::from("/data/board/state/bin")),
        }
        .apply(&mut cmd);

        let env: std::collections::BTreeMap<_, _> = envs(&cmd).into_iter().collect();
        let path = env.get("PATH").expect("PATH");
        let dirs: Vec<&str> = path.split(':').collect();
        assert_eq!(dirs[0], "/data/board/state/bin");
        assert_eq!(dirs[1], "/home/comet/.comet-native/app/0.3.5");
        assert_eq!(dirs[2], "/opt/node/bin");
    }

    /// An engine with no `comet-board` beside it hands over an empty list, and
    /// that must not put an empty entry — `:` — at the front of PATH, which
    /// every shell reads as the current directory.
    #[test]
    fn no_bin_dirs_means_no_path_change() {
        let mut cmd = tokio::process::Command::new("claude");
        prepend_exe_dir_to_path(&mut cmd, std::path::Path::new("/opt/node/bin/claude"));
        let before: std::collections::BTreeMap<_, _> = envs(&cmd).into_iter().collect();
        prepend_dirs_to_path(&mut cmd, &[]);
        let after: std::collections::BTreeMap<_, _> = envs(&cmd).into_iter().collect();
        assert_eq!(after.get("PATH"), before.get("PATH"));
    }

    /// A Claude login says nothing about `CODEX_HOME`. Pointing the wrong CLI
    /// at a dir holding the other one's credentials is worse than not pointing
    /// it anywhere: it would look configured and log in as nobody.
    #[test]
    fn an_account_is_never_applied_to_another_harness() {
        let mut codex = tokio::process::Command::new("codex");
        account(HarnessId::ClaudeCode).apply(&mut codex, HarnessId::Codex);
        assert!(envs(&codex).is_empty());

        // Nor to a harness with no account concept at all.
        let mut opencode = tokio::process::Command::new("opencode");
        account(HarnessId::Opencode).apply(&mut opencode, HarnessId::Opencode);
        assert!(envs(&opencode).is_empty());
    }
}
