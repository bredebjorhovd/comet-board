//! `comet-board` — the board's command line, and the agents' entry point
//! (§board-cli; the operator trio landed with §adopt-doctor-init).
//!
//! A thin binary over `crates/board`, attaching to the local engine exactly as
//! any viewport does: the engine owns the board loop and the workspace doc, and
//! this speaks the typed RPC on the localhost IPC WebSocket — `WatchBoard` for
//! `list`/`wait`, `DispatchTask`/`CancelTask` for the verbs, `WatchSpaces` for
//! the one thing the setup commands cannot know (which spaces exist on this
//! device). Everything else (config, detection, the routing.toml writer, the
//! report, ticket writing) is library code with tests, plus [`ops`] here.
//!
//! The board it drives need not be this device's. `--device` names the box that
//! hosts it, and the board RPCs are relay-forwardable (gh#55), so the dial
//! stays localhost and the local engine forwards — the same thing the desktop
//! panel does (gh#73).

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use comet_board::adopt;
use comet_board::config::Paths;
use comet_board::doctor::EngineStatus;
use comet_board::log::Logger;
use comet_proto::Space;
use comet_rpc::{RpcClient, connect_ws, methods};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

mod ops;
// Test-only: it renders the shipped skill's verb table from the clap tree
// below and fails the build when the committed file disagrees. Nothing at
// runtime reads it — `skill show` prints the asset, which this keeps honest.
#[cfg(test)]
mod skill_doc;
// Test-only, same job one document over: every `§` reference in the repo has
// to name a file in `docs/board/` (gh#203).
#[cfg(test)]
mod board_docs;

/// Same default as `apps/comet`.
const DEFAULT_IPC_PORT: u16 = 27654;

/// The engine answers a snapshot immediately; anything slower than this is a
/// listener that is not the engine.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(
    name = "comet-board",
    version,
    about = "Task board over comet — Linear/GitHub issues in, coding-agent chats out"
)]
// `version` exists for everything *outside* this binary (gh#156). `doctor` has
// never needed it — a process knows its own `CARGO_PKG_VERSION` — but nothing
// else could ask: `install.sh` could see a binary in the way and not say which
// one, and `comet status` could not tell whether the CLI beside it was the one
// the release installed. A stale copy answering "unexpected argument
// '--version'" is itself the answer, since the flag lands with the first
// release that ships this binary at all.
struct Cli {
    /// Localhost IPC port the engine serves (env: COMET_IPC_PORT).
    #[arg(long, global = true)]
    port: Option<u16>,
    /// Engine data directory (env: COMET_DATA_DIR).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Which device hosts the board — its name or id (env:
    /// COMET_BOARD_DEVICE). Omit for this device's own board.
    ///
    /// The board store lives on exactly one device, usually the always-on box,
    /// and the board RPCs are relay-forwarded: this still dials the local
    /// engine, which passes the call on. Applies to the verbs that ask the
    /// engine — list, dispatch, retry, cancel, wait, `new --dispatch`,
    /// `claim` and `review`, whose attempt rows and checkouts live on the host
    /// (§gh#183), `routes`, which reads and writes the host's routing.toml
    /// (gh#75), and `onboard`, whose clone, space and route all land on the
    /// host (gh#97).
    /// The rest (doctor, init, adopt, stats) read this device's own files
    /// either way, and are unaffected.
    #[arg(long, global = true)]
    device: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List what is on the board. `--json` for orchestrating agents.
    List {
        /// Only this state: blocked, working, ready, review, failed, done.
        #[arg(long)]
        state: Option<String>,
        /// Only this source: linear or github.
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Release a task into a coding-agent chat.
    Dispatch {
        #[arg(long)]
        task: String,
        /// The dispatching chat's id — provenance, never authority. Normally
        /// omitted: a dispatch from inside a board-dispatched chat reads its
        /// own id from COMET_BOARD_CHAT_ID. Pass this only when releasing work
        /// on behalf of a chat that is not you.
        #[arg(long)]
        via: Option<String>,
        /// Override the route's configured runtime for this dispatch — e.g.
        /// claude-code, opencode, codex, cursor. `comet-board list --json`
        /// shows each row's default. An unknown one is refused, naming the
        /// set the engine offers.
        #[arg(long)]
        runtime: Option<String>,
        /// Override the harness's default model for this dispatch. Checked
        /// against that runtime's catalog before anything is dispatched; an
        /// unknown one is refused, naming the catalog.
        #[arg(long)]
        model: Option<String>,
        /// Agent-account slot id to run under — whose Claude/Codex
        /// subscription this dispatch spends. Defaults to the route's
        /// `account`, and failing that the device's own CLI login.
        /// `comet-board doctor` lists the ids this device has saved.
        #[arg(long)]
        account: Option<String>,
        /// Spend an account that is not yours, on purpose (gh#101).
        ///
        /// Under `[defaults] billing_guard = "require-own"` a dispatch that
        /// would charge somebody else's subscription is refused; this is the
        /// acknowledgement that releases it, and it has to NAME the payer —
        /// the agent-account slot id (which also selects it, so `--account` is
        /// then redundant) or the email on the login being spent, which is the
        /// only spelling there is when that login is the box's own. Under
        /// `warn` and `off` it changes nothing but which slot is picked.
        #[arg(long)]
        bill: Option<String>,
        /// Ask for a stack: have the agent decompose the task into layered
        /// pull requests with `gh stack` (gh#287), one dependent concern per
        /// layer, reviewed in parallel instead of as one wall of diff.
        ///
        /// Off unless asked for. Layer 1 is the attempt's own branch and the
        /// ones above it are that name with `-2`, `-3` on the end, which is how
        /// the board tells they are one attempt's work. Needs the `gh-stack`
        /// extension on the dispatching box; the brief tells the agent how to
        /// install it if it is missing.
        #[arg(long)]
        stack: bool,
        /// Stack this release on another task (gh#285): cut its branch from
        /// the branch that task's attempt holds, and open its pull request
        /// against that branch instead of trunk. Takes a task id or the
        /// identifier the board prints.
        ///
        /// The parent has to have PUSHED — a dispatch branches from origin,
        /// never from a local checkout, so an unpushed parent branch refuses
        /// the release rather than silently cutting from trunk.
        #[arg(long)]
        onto: Option<String>,
        /// Cut this release from that branch instead of the route's `base`.
        /// The escape hatch for a branch no task on the board holds — a
        /// release branch, somebody else's branch. Use `--onto` for a
        /// sibling: it records which attempt this was cut from, and a branch
        /// name does not. Passing both is refused.
        #[arg(long)]
        base: Option<String>,
    },
    /// Release a task again — the desktop panel's Retry, from a shell.
    ///
    /// On a blocked row this ends the live attempt and releases a fresh one in
    /// the same call, which cancel-then-dispatch cannot do: between the two the
    /// row is `ready` and anything else may take its slot. On a failed or ready
    /// row nothing is live, so it is an ordinary dispatch.
    Retry {
        #[arg(long)]
        task: String,
        /// The retrying chat's id — provenance, never authority. Normally
        /// omitted; see `dispatch --via`.
        #[arg(long)]
        via: Option<String>,
        /// Override the route's configured runtime for this attempt.
        #[arg(long)]
        runtime: Option<String>,
        /// Override the harness's default model for this attempt.
        #[arg(long)]
        model: Option<String>,
        /// Agent-account slot id to run under.
        #[arg(long)]
        account: Option<String>,
        /// Spend an account that is not yours, on purpose (gh#101).
        ///
        /// Under `[defaults] billing_guard = "require-own"` a dispatch that
        /// would charge somebody else's subscription is refused; this is the
        /// acknowledgement that releases it, and it has to NAME the payer —
        /// the agent-account slot id (which also selects it, so `--account` is
        /// then redundant) or the email on the login being spent, which is the
        /// only spelling there is when that login is the box's own. Under
        /// `warn` and `off` it changes nothing but which slot is picked.
        #[arg(long)]
        bill: Option<String>,
        /// Ask for a stack on this attempt — see `dispatch --stack` (gh#287).
        /// Per attempt, not remembered from the last one: a retry is a fresh
        /// brief, and the shape the first attempt was asked for is often
        /// exactly what is being reconsidered.
        #[arg(long)]
        stack: bool,
        /// Stack this attempt on another task's branch (gh#285) — see
        /// `dispatch --onto`. A retry only re-reads this when the branch is
        /// actually cut: an existing branch is reused as it stands, so a
        /// retry of an already-stacked task keeps the parent it had.
        #[arg(long)]
        onto: Option<String>,
        /// Cut this attempt from that branch instead of the route's `base` —
        /// see `dispatch --base`.
        #[arg(long)]
        base: Option<String>,
    },
    /// Cancel a task's live attempt. The issue stays open.
    Cancel {
        #[arg(long)]
        task: String,
    },
    /// Block until watched work settles. The counterpart to `dispatch`.
    Wait {
        /// Task to watch; repeat for several. Omit to watch everything in
        /// flight right now.
        #[arg(long)]
        task: Vec<String>,
        /// States that count as settled. Defaults to review, failed and done.
        #[arg(long)]
        state: Vec<String>,
        /// Also return when a watched task goes `blocked` — its agent is
        /// waiting on an answer, and nothing else will settle until it gets
        /// one. Adds to the settle states rather than replacing them.
        #[arg(long)]
        blocked_is_settled: bool,
        /// Give up after this many seconds.
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Say what your attempt did, in claims a reviewer can check.
    ///
    /// One claim per line: `what you did :: path/one.rs path/two.rs`. The paths
    /// are the point — a sentence with no files after `::` is prose, and prose
    /// is refused, because the board checks every claim against the diff and
    /// then tells you which of your changes no claim accounts for. That answer
    /// is what comes back, and it is worth reading: it is the part of your own
    /// work you did not notice you had done.
    ///
    /// Pipe a block on stdin, or pass `--claim` once per claim. Submitting
    /// again replaces the set.
    Claim {
        /// The task whose attempt these claims are about.
        #[arg(long)]
        task: String,
        /// One claim, in the format above. Repeat for several; omit and the
        /// block is read from stdin.
        #[arg(long = "claim")]
        claims: Vec<String>,
        /// Print the whole review as JSON instead of the summary.
        #[arg(long)]
        json: bool,
    },
    /// What an attempt was asked to do, what it says it did, and what it did
    /// not account for.
    ///
    /// The last of those is the one to read: a dependency nobody mentioned and
    /// a function edited in passing are what you would have missed, and they
    /// are computed by the board from the branch diff rather than asserted by
    /// the agent.
    Review {
        /// The task to review.
        #[arg(long)]
        task: String,
        /// Which attempt, by id. Defaults to the task's latest — a retry makes
        /// claims of its own.
        #[arg(long)]
        attempt: Option<i64>,
        /// Print brief, claims, evidence and remainder as one JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Give the verdict: record it, hand it to the agent still standing in the
    /// checkout, and post it on the pull request.
    ///
    /// The changes no claim accounts for ride along on both copies — the board
    /// derives them from the diff, so there is nothing to retype and nothing to
    /// get wrong. The agent is prompted in the chat it wrote the branch in, so
    /// the fix is a commit on that branch rather than a new dispatch.
    ///
    /// A GitHub that refuses the review does not lose the verdict: it stands,
    /// the agent still has it, and the receipt says it is not on the pull
    /// request. Re-run the same verdict to try the posting again.
    ///
    /// Sending the same verdict twice sends it once: the submission is
    /// recorded, and a retry finishes whichever half failed.
    Verdict {
        /// The task whose pull request this is about.
        #[arg(long)]
        task: String,
        /// Which attempt, by id. Defaults to the task's latest.
        #[arg(long)]
        attempt: Option<i64>,
        /// What to say. `-` reads it from stdin. Required for everything but
        /// `--approve`, because a verdict with nothing in it tells an agent to
        /// change something unnamed.
        #[arg(long)]
        comment: Option<String>,
        /// Approve, rather than comment.
        #[arg(long, conflicts_with = "changes")]
        approve: bool,
        /// Request changes — the one verdict that is actionable with or without
        /// prose.
        #[arg(long = "request-changes")]
        changes: bool,
        /// Print the receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Write a ticket. Cheaper than not writing one.
    New {
        title: String,
        /// Description. `-` reads it from stdin.
        #[arg(long)]
        body: Option<String>,
        /// Linear team key. Only needed when you have more than one.
        #[arg(long)]
        team: Option<String>,
        /// Labels to apply — this is what routes it.
        #[arg(long)]
        label: Vec<String>,
        /// Which tracker to write to: linear or github. Defaults to
        /// `[defaults] new_source`.
        #[arg(long)]
        source: Option<String>,
        /// `owner/repo` when writing to GitHub and more than one is configured.
        #[arg(long)]
        repo: Option<String>,
        /// Dispatch it as soon as it reaches the board.
        #[arg(long)]
        dispatch: bool,
    },
    /// What the board knows about its own throughput.
    Stats {
        /// Only the last N days. Omit for everything.
        #[arg(long)]
        since_days: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// Check the environment: keys, engine, routes, repos. Exits non-zero on
    /// any failing check.
    Doctor,
    /// Generate a starter routing.toml from the spaces on this device.
    Init {
        /// Overwrite an existing routing.toml.
        #[arg(long)]
        force: bool,
    },
    /// Read and change the board's `routing.toml` — over the RPC, so `--device`
    /// reaches the box that hosts the board (gh#75).
    ///
    /// The counterpart to `adopt`, which does the same writing against *this*
    /// device's files. Use this one whenever the board is not yours: it needs no
    /// shell on the box, and every write is validated and backed up before it
    /// lands, exactly as `adopt`'s is.
    Routes {
        #[command(subcommand)]
        command: RoutesCommand,
    },
    /// Who else drives this board — the `[users]` map that decides whose name
    /// a teammate's dispatched commits carry (gh#162).
    ///
    /// Over the RPC like `routes`, so `--device` reaches the box: the map is a
    /// file on the board's host, and the credential that resolves a GitHub
    /// login into an address is the board's App.
    ///
    /// `docs/teammate.md` is the sequence this verb is step three of.
    Member {
        #[command(subcommand)]
        command: MemberCommand,
    },
    /// Put a repo the board has never seen on the board: clone it, give it a
    /// space, and route it — one verb (gh#97).
    ///
    /// Everything happens on the board's device, which is what makes this
    /// runnable from a laptop that is not the box and has no GitHub credential
    /// of its own: the checkout has to be where the agents run, the space has to
    /// belong to that device, and the credential that decides whether the repo
    /// is reachable at all is the board's App.
    ///
    /// Idempotent at every step, because the failure it exists to remove is a
    /// half-onboarded repo. Re-running is the repair: an existing checkout of
    /// the same repo is reused, an existing space is reused, and a repo already
    /// polled and routed is left alone. A directory holding something else is
    /// refused rather than cloned over.
    ///
    /// With no repo: list what the board's App can see.
    Onboard {
        /// `owner/repo`, or the repository's github.com URL.
        slug: Option<String>,
        /// Where the checkout goes **on the board's device** — the folder that
        /// ends up holding the repo, not a parent to clone into. Defaults to the
        /// engine's own clone root. Quote a `~` so it means the box's home
        /// rather than your shell's.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Poll only issues carrying one of these labels (comma-separated), so a
        /// roadmap-sized backlog does not land on the board whole.
        #[arg(long, value_delimiter = ',')]
        labels: Option<Vec<String>>,
        /// Poll every open issue, said out loud (writes `labels = []`,
        /// overriding a narrower global filter).
        #[arg(long, conflicts_with = "labels")]
        all_issues: bool,
        /// Onboard it even though another board on this account already polls
        /// it (gh#343).
        ///
        /// Refused without this, because both boards would then see the same
        /// issue as ready and either could dispatch it — two agents on one
        /// ticket, invisible until two pull requests appear. Say this when the
        /// sharing is intended: a second board that polls and never dispatches
        /// is a real setup.
        #[arg(long)]
        force: bool,
        /// The whole result as JSON — every step's outcome, for an orchestrating
        /// agent that has to decide what to do next.
        #[arg(long)]
        json: bool,
    },
    /// Offer git-detected spaces the board is not watching; adopt one by slug.
    ///
    /// With no slug: list what could be adopted. With one: write the missing
    /// `[[route]]` / `[github] repos` halves (validated, with a .bak beside).
    Adopt {
        /// `owner/repo` from the list `comet-board adopt` prints.
        slug: Option<String>,
        /// Poll only issues carrying one of these labels (comma-separated) —
        /// writes a `[[github.repo]]` filter so a roadmap-sized backlog does
        /// not land on the board whole.
        #[arg(long, value_delimiter = ',')]
        labels: Option<Vec<String>>,
        /// Poll every open issue, said out loud (writes `labels = []`,
        /// overriding a narrower global filter).
        #[arg(long, conflicts_with = "labels")]
        all_issues: bool,
        /// Stop offering this repo — you are only reading it.
        #[arg(long, requires = "slug")]
        ignore: bool,
    },
    /// Install this skill — the one you are reading — where agents on this
    /// machine will find it.
    ///
    /// The skill is compiled into this binary, so the copy it writes documents
    /// exactly the flags above. A box installs it from the setup wizard and
    /// each agent-account slot gets its own on every dispatch; this is the verb
    /// for the third case — a laptop that drives the board with `--device` and
    /// whose sessions should speak the same conventions.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// git's askpass helper: print the credential for pushing to the repo named
    /// by COMET_BOARD_ASKPASS_REPO (gh#58).
    ///
    /// Not for people. `git` runs this itself when `GIT_ASKPASS` points at it,
    /// which is how an App's installation token reaches a push without being
    /// written into `.git/config`, argv, or the environment.
    #[command(hide = true)]
    GitAskpass {
        /// The prompt git is asking — "Username for …" or "Password for …".
        prompt: Option<String>,
    },
    /// Print a GitHub token for the repo named by COMET_BOARD_ASKPASS_REPO
    /// (gh#68).
    ///
    /// Not for people either: the `gh` shim on a dispatched agent's PATH runs
    /// this so `gh pr create` authenticates as the board's App installation
    /// with a token minted for that invocation, rather than one that was
    /// already an hour old when the agent got round to opening the PR.
    #[command(hide = true)]
    GhToken,
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Write it into a Claude config dir (default `$CLAUDE_CONFIG_DIR`, else
    /// `~/.claude`).
    Install {
        /// The config dir to install into, when it is not this shell's.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Where the skill is installed, and whether it matches this binary.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Print the skill this binary ships, stamped with its version.
    Show,
}

#[derive(Subcommand)]
enum MemberCommand {
    /// Map a teammate's sign-in email to their GitHub identity, so their
    /// dispatches commit as them.
    ///
    /// Idempotent: re-running with the same person changes nothing, and
    /// re-running with a different address corrects the entry rather than
    /// adding a second. Until this exists for someone, every task they release
    /// commits under the box's own git identity — which on a repo behind a
    /// contributor gate is a refused deploy, not a cosmetic error.
    Add {
        /// The email they sign in to comet with. That is what a dispatch
        /// arrives as, and it is the only thing the board can key the map on.
        email: String,
        /// Their GitHub login (`ana`), or the
        /// `<id>+<login>@users.noreply.github.com` address from
        /// <https://github.com/settings/emails>. A bare login is resolved
        /// through the board's GitHub credential; an address is taken as
        /// written, and `Name <address>` carries a name with it.
        #[arg(long)]
        github: String,
        /// The name their commits show. Defaults to the GitHub login, which is
        /// what the address alone would have said anyway.
        #[arg(long)]
        name: Option<String>,
    },
    /// The map, the box's agent-account slots, and who has one without the
    /// other.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Take somebody out of the map — offboarding, or an entry for the wrong
    /// account.
    ///
    /// Their dispatches go back to committing under the box's own identity.
    /// Nothing else about them changes: their agent-account slot, their org
    /// membership and the chats they already have are all elsewhere.
    Remove {
        /// The sign-in email, as `member list` prints it.
        email: String,
    },
}

#[derive(Subcommand)]
enum RoutesCommand {
    /// The routes in force, what is wrong with the config, and what is not
    /// routed yet.
    List {
        /// The whole reply as JSON — text, parse, problems, and what is
        /// unadopted.
        #[arg(long)]
        json: bool,
    },
    /// Print `routing.toml` verbatim. Comments and all: this is the file.
    Show,
    /// Route a repo that has a space on the board's device but nothing
    /// watching it — the `[[route]]` and `[github] repos` halves, written
    /// together.
    Add {
        /// `owner/repo`, as `comet-board routes list` offers it.
        slug: String,
        /// Poll only issues carrying one of these labels (comma-separated), so
        /// a roadmap-sized backlog does not land on the board whole.
        #[arg(long, value_delimiter = ',')]
        labels: Option<Vec<String>>,
        /// Poll every open issue, said out loud (writes `labels = []`).
        #[arg(long, conflicts_with = "labels")]
        all_issues: bool,
        /// Route it even though another board on this account already polls it
        /// (gh#343) — see `comet-board onboard --help` for what that costs.
        #[arg(long)]
        force: bool,
    },
    /// Stop offering a repo — you are only reading it.
    Ignore { slug: String },
    /// Set one key on one route: `routes set 2 account brede-personal`.
    ///
    /// The number is the one `routes list` prints. Anything this cannot
    /// express — a prompt, a match, the order of the routes — is `routes edit`.
    Set {
        /// Route number, as printed by `routes list` (1-based).
        route: usize,
        /// One of: name, workspace, repo, runtime, account, branch_template,
        /// base, max_concurrent, max_duration, max_tool_failures,
        /// max_tool_calls, archive_chats, billing_guard,
        /// agent_instructions.
        key: String,
        /// The new value. Omit with `--unset` to remove the key, which falls
        /// the route back to `[defaults]`.
        value: Option<String>,
        /// Remove the key instead of setting it.
        #[arg(long, conflicts_with = "value")]
        unset: bool,
    },
    /// Set one key under `[defaults]`: `routes defaults max_duration 4h`.
    Defaults {
        /// One of: max_concurrent_per_workspace, branch_template, base,
        /// notify, notify_dispatcher, orchestrator_chat, new_source,
        /// max_duration, max_tool_failures, max_tool_calls, retain_worktrees,
        /// retain_build_output, archive_chats, billing_guard,
        /// agent_instructions.
        key: String,
        value: Option<String>,
        #[arg(long, conflicts_with = "value")]
        unset: bool,
    },
    /// Open `routing.toml` in `$EDITOR` and write it back, validated.
    ///
    /// The escape hatch that keeps this surface honest: everything the typed
    /// edits cannot say is still one command away, on a board you have no shell
    /// on. A file that would not validate is refused and the box keeps the
    /// config it had; an edit that lands on a file somebody changed meanwhile
    /// is refused too, rather than quietly overwriting them.
    Edit,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let port = cli
        .port
        .or_else(|| {
            std::env::var("COMET_IPC_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
        })
        .unwrap_or(DEFAULT_IPC_PORT);
    let paths = match &cli.data_dir {
        Some(dir) => Paths::under(dir)?,
        None => Paths::discover()?,
    };
    // Which device's board the verbs drive. The environment carries it so an
    // orchestrator points its whole shell at the box once, instead of
    // threading the flag through every call it makes.
    let device = cli
        .device
        .or_else(|| std::env::var("COMET_BOARD_DEVICE").ok())
        .filter(|d| !d.is_empty());
    let device = device.as_deref();

    // One-shot commands on a current-thread runtime; the async work is the IPC
    // round-trips (`wait` holds its subscription open, but it is still the
    // only thing running). The tracker clients in `new` are blocking and run
    // outside the runtime on purpose.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    match cli.command {
        // Answered before anything else touches the engine: git runs this
        // synchronously in the middle of a push, and it has no business dialling
        // an IPC port to print one line.
        Command::GitAskpass { prompt } => {
            let repo = std::env::var(comet_board::git_credentials::ASKPASS_REPO_ENV).ok();
            let prompt = prompt.unwrap_or_default();
            let chat = std::env::var(ops::CHAT_ID_ENV).ok();
            match comet_board::git_credentials::askpass(&paths, &prompt, repo.as_deref()) {
                Ok(secret) => {
                    // A username prompt is answered off a constant, so it is
                    // not a mint and must not be recorded as one — an alibi
                    // for a push has to be a credential that was issued.
                    if !prompt.to_ascii_lowercase().contains("username") {
                        comet_board::credential_ledger::minted(
                            &paths,
                            "git-askpass",
                            repo.as_deref().unwrap_or_default(),
                            chat.as_deref(),
                        );
                    }
                    // Straight to stdout, which is the pipe git is holding.
                    // Nowhere else.
                    println!("{secret}");
                    Ok(())
                }
                // git swallows this helper's stderr into a push that fails
                // with "could not read Password", so stderr alone is not a
                // record anybody will find (gh#233). The ledger is.
                Err(e) => {
                    comet_board::credential_ledger::failed(
                        &paths,
                        "git-askpass",
                        repo.as_deref().unwrap_or_default(),
                        chat.as_deref(),
                        &e.to_string(),
                    );
                    Err(e)
                }
            }
        }
        // Same deal for `gh`, and for the same reason it is answered before
        // anything dials the engine: this runs inside every `gh` the agent
        // invokes.
        Command::GhToken => {
            let repo = std::env::var(comet_board::git_credentials::ASKPASS_REPO_ENV)
                .ok()
                .filter(|r| !r.is_empty())
                .with_context(|| {
                    format!(
                        "{} is not set — nothing to mint for",
                        comet_board::git_credentials::ASKPASS_REPO_ENV
                    )
                })?;
            let chat = std::env::var(ops::CHAT_ID_ENV).ok();
            match comet_board::git_credentials::token(&paths, &repo) {
                Ok(token) => {
                    comet_board::credential_ledger::minted(
                        &paths,
                        "gh-token",
                        &repo,
                        chat.as_deref(),
                    );
                    println!("{token}");
                    Ok(())
                }
                // The shim discards this too (`2>/dev/null`, so a box with its
                // own `gh auth login` keeps working), which makes the ledger
                // the only place a failed mint is written down.
                Err(e) => {
                    comet_board::credential_ledger::failed(
                        &paths,
                        "gh-token",
                        &repo,
                        chat.as_deref(),
                        &e.to_string(),
                    );
                    Err(e)
                }
            }
        }
        // Also answered without the engine: the skill is compiled in, and a
        // laptop installing it has no board of its own to dial (gh#133).
        Command::Skill { command } => skill(command),
        Command::List {
            state,
            source,
            json,
        } => {
            ops::validate_filters(state.as_deref(), source.as_deref())?;
            let rows = runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                ops::board_rows(&board).await
            })?;
            ops::print_tasks(
                &ops::filter_rows(rows, state.as_deref(), source.as_deref()),
                json,
            )
        }
        Command::Dispatch {
            task,
            via,
            runtime: runtime_flag,
            model,
            account,
            bill,
            stack,
            onto,
            base,
        } => {
            let via = ops::provenance(via);
            let opts = ops::DispatchOpts {
                via: via.as_deref(),
                runtime: runtime_flag.as_deref(),
                model: model.as_deref(),
                account: account.as_deref(),
                bill: bill.as_deref(),
                stack,
                onto: onto.as_deref(),
                base: base.as_deref(),
                // Filled in from the engine below — this shell has no way to
                // know who is signed in without asking.
                via_user: None,
                replace: false,
            };
            let d = runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                // Who this shell is, before anything else: it rides on the
                // dispatch as `viaUser` (gh#74) and is the other half of the
                // billing comparison (gh#101).
                let me = ops::signed_in_email(&board).await;
                let opts = ops::DispatchOpts {
                    via_user: me.as_deref(),
                    ..opts
                };
                warn_if_cross_billed(&board, &task, opts).await;
                ops::dispatch_checked(&board, &task, opts).await
            })?;
            println!(
                "dispatched {task} → chat {} (attempt {}, {})",
                d.chat_id, d.attempt, d.cwd
            );
            if let Some(v) = &via {
                println!("released by chat {v}");
            }
            print_overrides(&opts);
            Ok(())
        }
        Command::Retry {
            task,
            via,
            runtime: runtime_flag,
            model,
            account,
            bill,
            stack,
            onto,
            base,
        } => {
            let via = ops::provenance(via);
            let opts = ops::DispatchOpts {
                via: via.as_deref(),
                runtime: runtime_flag.as_deref(),
                model: model.as_deref(),
                account: account.as_deref(),
                bill: bill.as_deref(),
                stack,
                onto: onto.as_deref(),
                base: base.as_deref(),
                // `replace` is not set here: `retry` reads the row and decides,
                // because ending a live attempt is not something a verb should
                // do without looking at what it is ending.
                ..Default::default()
            };
            let r = runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                let me = ops::signed_in_email(&board).await;
                let opts = ops::DispatchOpts {
                    via_user: me.as_deref(),
                    ..opts
                };
                // A retry bills someone too — it is a fresh attempt on the same
                // account, and the one that ends a live agent is exactly the
                // one nobody wants to have spent on the wrong plan.
                warn_if_cross_billed(&board, &task, opts).await;
                ops::retry(&board, &task, opts).await
            })?;
            println!(
                "retried {task} → chat {} (attempt {}, {})",
                r.dispatched.chat_id, r.dispatched.attempt, r.dispatched.cwd
            );
            // A retry that ended a live agent has to say so — that is the
            // difference between this and a first dispatch, and the operator
            // is the only one who can tell whether the answer it was waiting
            // for is now lost.
            if r.replaced {
                println!("replaced the live attempt the {} row was stuck on", r.was);
            }
            if let Some(v) = &via {
                println!("released by chat {v}");
            }
            print_overrides(&opts);
            Ok(())
        }
        Command::Cancel { task } => {
            runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                // The row first: `CancelTask` answers `{ok}`, and the parent
                // that will not be notified is on the row. Best-effort — a
                // failed read must not block the cancel it precedes.
                let parent = ops::board_rows(&board)
                    .await
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|r| r.id == task))
                    .and_then(|r| r.dispatched_by_chat);
                ops::cancel(&board, &task).await?;
                println!("cancelled {task} — the issue is still open");
                // Nothing tells the parent. Say so where the caller will see
                // it, rather than leaving a waiting agent to be discovered.
                if let Some(p) = parent {
                    println!("dispatched by chat {p} — not notified");
                }
                Ok(())
            })
        }
        Command::Wait {
            task,
            state,
            blocked_is_settled,
            timeout,
            json,
        } => {
            let states = ops::settle_states(&state, blocked_is_settled);
            let rows = runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                ops::wait_for(&board, &task, &states, timeout.map(Duration::from_secs)).await
            })?;
            ops::print_tasks(&rows, json)
        }
        Command::Claim { task, claims, json } => {
            let text = claim_text(&claims)?;
            let review = runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                ops::submit_claims(&board, &task, &text).await
            })?;
            ops::print_claim_result(&review, json)
        }
        Command::Review {
            task,
            attempt,
            json,
        } => {
            let review = runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                ops::attempt_review(&board, &task, attempt).await
            })?;
            ops::print_review(&review, json)
        }
        Command::Verdict {
            task,
            attempt,
            comment,
            approve,
            changes,
            json,
        } => {
            let comment = match comment.as_deref() {
                Some("-") => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                    buf
                }
                Some(text) => text.to_string(),
                None => String::new(),
            };
            let kind = if approve {
                "approve"
            } else if changes {
                "changes_requested"
            } else {
                "comment"
            };
            let receipt = runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                ops::submit_verdict(&board, &task, attempt, kind, &comment).await
            })?;
            ops::print_verdict(&receipt, json)
        }
        Command::New {
            title,
            body,
            team,
            label,
            source,
            repo,
            dispatch: and_dispatch,
        } => {
            let body = match body.as_deref() {
                Some("-") => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                    Some(buf)
                }
                other => other.map(str::to_string),
            };
            // Unvalidated on purpose, same as adopt: writing a ticket must stay
            // usable while some unrelated route is broken. An absent file still
            // works — the defaults say where new tickets go.
            let routing = paths.routing();
            let cfg = if routing.exists() {
                comet_board::config::RoutingConfig::load_unvalidated(&routing)?
            } else {
                comet_board::config::RoutingConfig::default()
            };
            let (identifier, url) = ops::new_task(
                &paths,
                &cfg,
                &ops::NewTask {
                    title: &title,
                    body: body.as_deref(),
                    team: team.as_deref(),
                    labels: &label,
                    source: source.as_deref(),
                    repo: repo.as_deref(),
                },
            )?;
            println!("{identifier}  {url}");

            if and_dispatch {
                // It has to be on the board before it can be dispatched, and
                // the engine's sync loop is what puts it there — wait for its
                // next poll rather than failing on the race.
                // `AGE-14` is Linear; `owner/repo#87` is GitHub.
                let id = if identifier.contains('/') {
                    format!("gh:{identifier}")
                } else {
                    format!("linear:{identifier}")
                };
                let pickup = Duration::from_secs(cfg.sync.interval_secs() * 2 + 30);
                let via = ops::provenance(None);
                let d = runtime.block_on(async {
                    let board = ops::attach(port, device).await?;
                    ops::await_row(&board, &id, pickup).await?;
                    let me = ops::signed_in_email(&board).await;
                    let opts = ops::DispatchOpts {
                        via: via.as_deref(),
                        via_user: me.as_deref(),
                        ..Default::default()
                    };
                    // The route's own account, on a ticket that did not exist a
                    // second ago — the case where nobody has had a chance to
                    // notice whose plan it lands on (gh#101).
                    warn_if_cross_billed(&board, &id, opts).await;
                    ops::dispatch(&board, &id, opts).await
                })?;
                println!(
                    "dispatched {id} → chat {} (attempt {})",
                    d.chat_id, d.attempt
                );
            }
            Ok(())
        }
        Command::Stats { since_days, json } => {
            let log = Arc::new(Logger::new(paths.logfile(), false));
            let s = comet_board::stats::run(&paths, log, since_days)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                comet_board::stats::print(&s);
            }
            Ok(())
        }
        Command::Doctor => {
            let (engine, spaces, accounts, edge, members, runtimes) =
                match runtime.block_on(fetch_spaces(port)) {
                    Ok(info) => (
                        EngineStatus {
                            reachable: true,
                            detail: format!(
                                "listening on 127.0.0.1:{port} (device {}, {} space(s) here)",
                                info.device,
                                info.spaces.len()
                            ),
                            version: info.version,
                            update: info.update,
                            // Its own conversation, on its own clock (gh#195):
                            // every call in it is a relay round trip to another
                            // machine, and the local report must not wait on a
                            // box that is asleep. A sweep that fails outright
                            // is `None` — "not checked", never "no other
                            // board".
                            peers: runtime
                                .block_on(sweep_boards(port, &info.device, info.edge.as_ref()))
                                .ok(),
                        },
                        Some(info.spaces),
                        Some(info.accounts),
                        info.edge,
                        info.members,
                        Some(info.runtimes),
                    ),
                    Err(e) => (
                        EngineStatus {
                            reachable: false,
                            detail: format!(
                                "not reachable on 127.0.0.1:{port} ({e:#}) — start `comet` or \
                                 `comet headless`"
                            ),
                            version: None,
                            update: None,
                            // Nothing was asked, so nothing may be claimed
                            // about the other devices either (gh#195).
                            peers: None,
                        },
                        None,
                        None,
                        None,
                        None,
                        None,
                    ),
                };
            let checks = comet_board::doctor::doctor(
                &paths,
                &engine,
                spaces.as_deref(),
                accounts.as_deref(),
                edge.as_ref(),
                members,
                runtimes.as_deref(),
            )?;
            if !comet_board::doctor::print_doctor(&checks) {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Init { force } => {
            let info = runtime.block_on(fetch_spaces(port)).with_context(|| {
                format!(
                    "listing spaces from the engine on 127.0.0.1:{port} — \
                     start `comet` or `comet headless` first"
                )
            })?;
            comet_board::init::init(&paths, &info.spaces, adopt::probe, force)
        }
        Command::Routes { command } => runtime.block_on(async {
            let board = ops::attach(port, device).await?;
            routes(&board, command).await
        }),
        Command::Member { command } => {
            // Read before the round trip, for the same reason `onboard` parses
            // its slug here: an argument that is neither an address nor a
            // login costs an error naming what to type, not a call to the box
            // that comes back with the same news. The box checks both halves
            // again — this is where the *words* are good, not where the rule
            // lives.
            if let MemberCommand::Add { email, github, .. } = &command {
                comet_board::members::parse_github(github)?;
                if !comet_board::git_identity::plausible_email(email.trim()) {
                    bail!(
                        "`{email}` is not a sign-in email. The first argument is the \
                         address they sign in to comet with — that is what a dispatch \
                         arrives as — and their GitHub login goes in --github."
                    );
                }
            }
            runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                member(&board, command).await
            })
        }
        Command::Onboard {
            slug,
            dir,
            labels,
            all_issues,
            force,
            json,
        } => {
            // Parsed before the round trip: a typo costs an error naming what to
            // type, not a call to the box that comes back with the same news.
            let slug = slug
                .as_deref()
                .map(comet_board::onboard::parse_slug)
                .transpose()?;
            let labels = comet_board::onboard::label_filter(labels, all_issues);
            let dir = dir.map(|d| d.to_string_lossy().to_string());
            runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                let Some(slug) = slug else {
                    let repos = ops::app_repos(&board).await?;
                    return ops::print_candidates(&repos, board.host(), json);
                };
                let done =
                    ops::onboard(&board, &slug, dir.as_deref(), labels.as_deref(), force).await?;
                ops::print_onboarded(&done, board.host(), json)
            })
        }
        Command::Adopt {
            slug,
            labels,
            all_issues,
            ignore,
        } => {
            let routing = paths.routing();
            if let (Some(slug), true) = (&slug, ignore) {
                adopt::ignore(&routing, slug)?;
                println!("{slug} will not be offered again (see [adopt] ignore)");
                return Ok(());
            }

            let spaces = runtime
                .block_on(fetch_spaces(port))
                .with_context(|| {
                    format!(
                        "listing spaces from the engine on 127.0.0.1:{port} — \
                         start `comet` or `comet headless` first"
                    )
                })?
                .spaces;
            // Unvalidated on purpose, same as doctor: adoption must stay
            // usable while some unrelated route is broken.
            let cfg = comet_board::config::RoutingConfig::load_unvalidated(&routing).with_context(
                || {
                    format!(
                        "reading {} — run `comet-board init` if it does not exist yet",
                        routing.display()
                    )
                },
            )?;
            let found = adopt::detect(&spaces, &cfg, adopt::probe);

            let Some(slug) = slug else {
                if found.is_empty() {
                    println!("every space with a GitHub remote is on the board");
                } else {
                    for u in &found {
                        println!("{:<40} {}{}", u.slug, u.label, u.missing.note());
                    }
                    println!(
                        "\nadopt one:   comet-board adopt <owner/repo> [--labels a,b | --all-issues]\
                         \nstop offers: comet-board adopt <owner/repo> --ignore"
                    );
                }
                return Ok(());
            };

            let Some(u) = found.iter().find(|u| u.slug.eq_ignore_ascii_case(&slug)) else {
                bail!(
                    "`{slug}` is not on the unadopted list — run `comet-board adopt` to see it; \
                     already-adopted and ignored repos are not offered"
                );
            };

            // What this is about to pull, before it pulls it. Best-effort: no
            // credential or no network degrades to adopting without the numbers.
            let preview = comet_board::sources::github::HttpRest::from_paths(&paths)
                .ok()
                .map(comet_board::sources::github::Github::new)
                .and_then(|gh| adopt::preview(&gh, &u.slug).ok());
            if let Some(p) = &preview {
                println!("{}: {}", u.slug, p.count_phrase());
                for (label, n) in p.labels.iter().take(8) {
                    println!("  {n:>4}  {label}");
                }
            }

            let labels: Option<Vec<String>> = if all_issues { Some(Vec::new()) } else { labels };
            let done = adopt::adopt_with(&routing, u, labels.as_deref())?;

            let mut wrote = Vec::new();
            if done.wrote_route {
                wrote.push("a [[route]]".to_string());
            }
            if done.wrote_repo {
                wrote.push("[github] repos".to_string());
            }
            if let Some(l) = &done.labels {
                wrote.push(if l.is_empty() {
                    "a [[github.repo]] polling every open issue".to_string()
                } else {
                    format!("a [[github.repo]] filter: {}", l.join(", "))
                });
            }
            println!("adopted {} → wrote {}", u.slug, wrote.join(" + "));
            if labels.is_none()
                && let Some(p) = &preview
                && p.open_issues > 20
            {
                println!(
                    "note: the global [github] labels filter applies — {} would arrive \
                     unfiltered; re-run with --labels to narrow what this repo contributes",
                    p.count_phrase()
                );
            }
            println!(
                "a commented `label = \"{}\"` route was left for Linear issues — edit it if \
                 that guess is wrong",
                done.suggested_label
            );
            Ok(())
        }
    }
}

/// The claim block to submit: the `--claim` flags if there are any, else
/// whatever is on stdin (§gh#183).
///
/// A terminal with no flags is somebody who typed the verb to see what it does,
/// and the honest answer is the format rather than a hang on a pipe nobody is
/// writing to. Everything else — a heredoc, a file, a `--claim` — goes through
/// unparsed; the board's host owns the format.
fn claim_text(claims: &[String]) -> Result<String> {
    use std::io::IsTerminal;
    if !claims.is_empty() {
        return Ok(claims.join("\n"));
    }
    if std::io::stdin().is_terminal() {
        bail!(
            "nothing to record. Pass --claim once per claim, or pipe a block:\n\
             \n  comet-board claim --task <id> <<'EOF'\n  \
             what you did :: path/one.rs path/two.rs\n  EOF\n\n\
             The paths are the contract: a claim with none cannot be checked \
             against the diff, and the diff is what says which of your changes \
             no claim accounts for."
        );
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
    Ok(buf)
}

/// `comet-board skill …` — put the shipped skill where agents read it (gh#133).
///
/// No engine, no board, no network: everything here is a file the binary is
/// carrying. That is deliberate — the machine most in need of this verb is a
/// teammate's laptop, which has no board of its own and reaches the box with
/// `--device`.
fn skill(command: SkillCommand) -> Result<()> {
    use comet_board::skill;
    match command {
        SkillCommand::Install { dir } => {
            let this_shells = dir.is_none();
            let dir = dir.unwrap_or_else(skill::user_config_dir);
            let was = skill::status_of(&dir);
            let installed = skill::install_into(&dir)
                .with_context(|| format!("installing the skill into {}", dir.display()))?;
            let path = installed.path.display();
            match (installed.changed, was) {
                (false, _) => println!("already v{} · {path}", skill::VERSION),
                (true, skill::State::Missing) => {
                    println!("installed v{} · {path}", skill::VERSION)
                }
                (true, _) => println!("updated to v{} · {path}", skill::VERSION),
            }
            // Only when this is the dir sessions here actually read: told a
            // `--dir` it would be a claim about somebody else's machine.
            if this_shells {
                println!(
                    "Sessions on this machine discover it on their next start. \
                     Agent-account slots get their own copy on every dispatch."
                );
            }
            Ok(())
        }
        SkillCommand::Status { json } => {
            let dir = skill::user_config_dir();
            let slots = comet_board::config::agent_account_dirs();
            if json {
                let row = |d: &std::path::Path| {
                    serde_json::json!({
                        "dir": d.display().to_string(),
                        "path": skill::path_in(d).display().to_string(),
                        "state": describe(&skill::status_of(d)),
                    })
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "version": skill::VERSION,
                        "user": row(&dir),
                        "slots": slots.iter().map(|d| row(d)).collect::<Vec<_>>(),
                    }))?
                );
                return Ok(());
            }
            println!("comet-board skill v{} (this binary)", skill::VERSION);
            println!(
                "  {:<9} {}",
                describe(&skill::status_of(&dir)),
                skill::path_in(&dir).display()
            );
            for slot in &slots {
                println!(
                    "  {:<9} {} (agent-account slot)",
                    describe(&skill::status_of(slot)),
                    skill::path_in(slot).display()
                );
            }
            Ok(())
        }
        SkillCommand::Show => {
            print!("{}", skill::rendered());
            Ok(())
        }
    }
}

/// One word per state, the same word in `--json` and in the printed column.
fn describe(state: &comet_board::skill::State) -> String {
    use comet_board::skill::State;
    match state {
        State::Current => "current".into(),
        State::Stale { version: Some(v) } => format!("v{v}"),
        State::Stale { version: None } => "edited".into(),
        State::Missing => "missing".into(),
        State::Unreadable(e) => format!("unreadable ({e})"),
    }
}

/// `comet-board member …` — the `[users]` map, written rather than hand-edited
/// (gh#162).
///
/// The same RPC surface `routes` drives, so every write re-parses, re-validates
/// and leaves a `.bak`; what is different is that the *value* may need a GitHub
/// round trip, which happens on the board's host because that is where the
/// credential is.
async fn member(board: &ops::Board, command: MemberCommand) -> Result<()> {
    match command {
        MemberCommand::Add {
            email,
            github,
            name,
        } => {
            // Read first so the result can say what actually changed. Adding
            // somebody who is already mapped must not read as having done
            // something — that is what makes re-running the documented
            // sequence safe.
            let before = ops::read_config(board).await?;
            let was = mapped(&before, &email);
            let after = ops::write_config(
                board,
                serde_json::json!({
                    "op": "member", "user": email, "github": github, "name": name
                }),
            )
            .await?;
            let now = mapped(&after, &email);
            let unchanged = was.is_some() && was == now;
            match (&was, &now) {
                (Some(_), Some(now)) if unchanged => {
                    println!("{email} → {now} (already mapped; nothing changed)")
                }
                (Some(was), Some(now)) => println!("{email} → {now} (was {was})"),
                (_, Some(now)) => println!("{email} → {now}"),
                // The write validated, so the entry is there; only a config
                // that stopped parsing could get here.
                (_, None) => println!("{email} mapped"),
            }
            if !unchanged {
                ops::print_write_result(&after);
            }
            // What is still missing for this person, said now rather than
            // discovered when their first run lands on somebody else's plan.
            let accounts = ops::agent_accounts(board).await;
            if let Some(cfg) = &after.routing.config {
                let roster = comet_board::members::roster(cfg, accounts.as_deref());
                if roster
                    .members
                    .iter()
                    .any(|m| m.user.eq_ignore_ascii_case(email.trim()) && m.needs_account())
                {
                    println!(
                        "no agent account on this box is theirs yet — until there is one, \
                         their runs spend whichever subscription the route names, or the \
                         box's own. Sign one in under Agent accounts (docs/teammate.md)"
                    );
                }
            }
            Ok(())
        }
        MemberCommand::List { json } => {
            let cfg = ops::read_config(board).await?;
            let accounts = ops::agent_accounts(board).await;
            let parsed = cfg.routing.config.clone().unwrap_or_default();
            if cfg.routing.config.is_none() {
                // The map is unreadable, not empty. Say which before printing
                // a roster of nothing.
                eprintln!(
                    "{} does not parse, so the map below is empty rather than read:\n  {}",
                    cfg.routing.path,
                    cfg.routing.problems.join("\n  ")
                );
            }
            let roster = comet_board::members::roster(&parsed, accounts.as_deref());
            ops::print_roster(&roster, board.host(), json)
        }
        MemberCommand::Remove { email } => {
            let before = ops::read_config(board).await?;
            let Some(was) = mapped(&before, &email) else {
                // Idempotent in the direction that matters: removing somebody
                // who is not there changed nothing, and saying so is not an
                // error.
                println!("{email} is not in the map — nothing to remove");
                return Ok(());
            };
            let after = ops::write_config(
                board,
                serde_json::json!({ "op": "user", "user": email, "value": null }),
            )
            .await?;
            println!("{email} removed (was {was}) — their dispatches commit as the box again");
            ops::print_write_result(&after);
            Ok(())
        }
    }
}

/// The `[users]` value for one person, as the config now stands.
///
/// Case-insensitive, because that is how the map is read at dispatch time
/// (`RoutingConfig::git_author_for`) — a lookup that disagreed with the reader
/// would report a change that did not happen.
fn mapped(cfg: &ops::BoardConfig, email: &str) -> Option<String> {
    let email = email.trim();
    cfg.routing
        .config
        .as_ref()?
        .users
        .iter()
        .find_map(|(k, v)| {
            k.trim()
                .eq_ignore_ascii_case(email)
                .then(|| v.trim().to_string())
        })
}

/// `comet-board routes …` — the routing.toml surface (gh#75).
///
/// Every branch is one RPC round trip against the board's host, and every write
/// answers with a fresh read: what gets printed after a change is the file as it
/// now stands, not what we hoped it would say.
async fn routes(board: &ops::Board, command: RoutesCommand) -> Result<()> {
    match command {
        RoutesCommand::List { json } => {
            let cfg = ops::read_config(board).await?;
            ops::print_config(&cfg, board.host(), json)
        }
        RoutesCommand::Show => {
            let cfg = ops::read_config(board).await?;
            if !cfg.routing.exists {
                // Not on stdout: `routes show > routing.toml` must not capture
                // an explanation as if it were config.
                eprintln!(
                    "{} does not exist yet — `comet-board init` writes the first one",
                    cfg.routing.path
                );
                return Ok(());
            }
            print!("{}", cfg.routing.text);
            Ok(())
        }
        RoutesCommand::Add {
            slug,
            labels,
            all_issues,
            force,
        } => {
            let labels: Option<Vec<String>> = if all_issues { Some(Vec::new()) } else { labels };
            let cfg = ops::write_config(
                board,
                serde_json::json!({
                    "op": "adopt", "slug": slug, "labels": labels, "force": force,
                }),
            )
            .await?;
            println!("adopted {slug}");
            ops::print_write_result(&cfg);
            Ok(())
        }
        RoutesCommand::Ignore { slug } => {
            let cfg = ops::write_config(board, serde_json::json!({ "op": "ignore", "slug": slug }))
                .await?;
            println!("{slug} will not be offered again (see [adopt] ignore)");
            ops::print_write_result(&cfg);
            Ok(())
        }
        RoutesCommand::Set {
            route,
            key,
            value,
            unset,
        } => {
            let value = settable(value, unset, &key)?;
            // 1-based for a person counting routes in a file, 0-based on the
            // wire. `routes list` prints the former.
            let index = route
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("routes are numbered from 1"))?;
            let cfg = ops::write_config(
                board,
                serde_json::json!({"op": "route", "route": index, "key": key, "value": value}),
            )
            .await?;
            ops::print_write_result(&cfg);
            Ok(())
        }
        RoutesCommand::Defaults { key, value, unset } => {
            let value = settable(value, unset, &key)?;
            let cfg = ops::write_config(
                board,
                serde_json::json!({"op": "default", "key": key, "value": value}),
            )
            .await?;
            ops::print_write_result(&cfg);
            Ok(())
        }
        RoutesCommand::Edit => edit_routing(board).await,
    }
}

/// A `set`'s value: given, or explicitly cleared. Refusing the empty case is
/// the point — `routes set 1 account` with neither a value nor `--unset` is
/// somebody who meant one of them, and guessing which would either write an
/// empty string or delete their configuration.
fn settable(value: Option<String>, unset: bool, key: &str) -> Result<Option<String>> {
    match (value, unset) {
        (Some(v), _) => Ok(Some(v)),
        (None, true) => Ok(None),
        (None, false) => bail!("give a value for `{key}`, or `--unset` to remove it"),
    }
}

/// `routes edit` — the file into `$EDITOR` and back, with the read it started
/// from carried along so a hand-edit on the box in the meantime is refused
/// rather than overwritten.
async fn edit_routing(board: &ops::Board) -> Result<()> {
    let before = ops::read_config(board).await?;
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|e| !e.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("$EDITOR is not set — set it, or use `comet-board routes set`")
        })?;

    // A scratch copy: the real file is on the board's device, which may not be
    // this one, and even when it is the write has to go through the validating
    // writer rather than round the side of it.
    let scratch =
        std::env::temp_dir().join(format!("comet-board-routing-{}.toml", std::process::id()));
    std::fs::write(&scratch, &before.routing.text)
        .with_context(|| format!("writing {}", scratch.display()))?;

    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&scratch)
        .status()
        .with_context(|| format!("running $EDITOR ({editor})"))?;
    let edited = std::fs::read_to_string(&scratch)
        .with_context(|| format!("reading {} back", scratch.display()));
    let _ = std::fs::remove_file(&scratch);
    if !status.success() {
        bail!("$EDITOR exited {status} — routing.toml is untouched");
    }
    let edited = edited?;

    if edited == before.routing.text {
        println!("no changes");
        return Ok(());
    }
    let cfg = ops::write_config(
        board,
        serde_json::json!({"op": "text", "text": edited, "base": before.routing.text}),
    )
    .await?;
    ops::print_write_result(&cfg);
    Ok(())
}

/// Say who a release is about to charge, before it charges them (gh#101).
///
/// On stderr, and never fatal: the engine applies the guard itself — it is the
/// only side that can refuse under `require-own` — so this is the line that
/// makes the spend visible in the surface the operator is actually looking at.
/// A release the engine then refuses has said it twice, which is the right
/// number of times for the sentence "somebody else is paying for this".
async fn warn_if_cross_billed(board: &ops::Board, task: &str, opts: ops::DispatchOpts<'_>) {
    if let Some(warning) = ops::cross_billing_warning(board, task, opts).await {
        eprintln!("{warning}");
    }
}

/// Echo the overrides a release was given, so the confirmation says what the
/// attempt is actually running under and not just that it started.
fn print_overrides(opts: &ops::DispatchOpts<'_>) {
    let parts: Vec<String> = [
        opts.runtime.map(|r| format!("runtime={r}")),
        opts.model.map(|m| format!("model={m}")),
        opts.account.map(|a| format!("account={a}")),
        opts.bill.map(|b| format!("bill={b}")),
        opts.onto.map(|o| format!("onto={o}")),
        opts.base.map(|b| format!("base={b}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !parts.is_empty() {
        println!("overrides: {}", parts.join(", "));
    }
}

/// How long the whole sweep of the other devices gets (gh#195).
///
/// Every call in it leaves this machine, and `doctor` is a report somebody is
/// waiting on: a box that has gone away must cost the report a few seconds, not
/// a minute. Devices the budget runs out on are reported as unasked rather than
/// as hosting nothing, which is the same distinction gh#155 drew for the picker.
const SWEEP_BUDGET: Duration = Duration::from_secs(8);

/// Is a failed [`methods::READ_BOARD_CONFIG`] the device's own answer — "I host
/// no board" — rather than a call that never landed (gh#155)?
///
/// `Refused` is `EngineRpc::board()` on a device with no board service, and
/// `UnknownMethod` is the same answer from an engine too old to know the
/// question: both replied, and their reply rules them out. Everything else
/// (transport, a relay 500, a timeout) means nobody was asked, and a board over
/// there would be invisible from here — which is a fact about the connection,
/// and belongs in the report as one.
fn hosts_no_board(err: &comet_rpc::RpcError) -> bool {
    matches!(
        err,
        comet_rpc::RpcError::Refused(_) | comet_rpc::RpcError::UnknownMethod(_)
    )
}

/// Which other devices on this account host a board of their own, and what it
/// polls (gh#195).
///
/// One relayed [`methods::READ_BOARD_CONFIG`] per device, over a connection of
/// its own so only `doctor` pays for it. The reply is the peer's `routing.toml`
/// as the peer reads it — the same call the settings page uses — so what comes
/// back is what that board is actually polling rather than what this box
/// believes about it.
///
/// A device that never answers is only reported when it was *present* at the
/// time (`host_presence`): this fleet is a Mac, a box and a phone, and a phone
/// is asleep essentially always. Warning about the phone on every run is how a
/// line stops being read before the day it has something to say.
async fn sweep_boards(
    port: u16,
    local_device: &str,
    edge: Option<&comet_proto::EdgeHealth>,
) -> Result<comet_board::doctor::Peers> {
    let client: RpcClient = connect_ws(&format!("ws://127.0.0.1:{port}")).await?;
    let devices: Vec<comet_proto::Device> = {
        let mut stream = tokio::time::timeout(
            FETCH_TIMEOUT,
            client.subscribe(methods::WATCH_DEVICES, serde_json::json!({})),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timed out listing devices"))??;
        let snapshot = stream
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("WatchDevices stream ended before a snapshot"))?;
        serde_json::from_value(snapshot)?
    };

    let now = chrono::Utc::now();
    let deadline = tokio::time::Instant::now() + SWEEP_BUDGET;
    let mut peers = comet_board::doctor::Peers::default();
    for device in devices.iter().filter(|d| d.id != local_device) {
        peers.asked += 1;
        // Gathered before the call, like gh#155's candidates: a device that
        // says nothing cannot tell you whether it was expected to answer.
        let present = matches!(
            comet_proto::view::host_presence(false, device.last_seen_at, edge, now),
            comet_proto::view::HostPresence::Online
        );
        let name = if device.name.is_empty() {
            device.id.clone()
        } else {
            device.name.clone()
        };
        let answer = tokio::time::timeout_at(
            deadline,
            client.call(
                methods::READ_BOARD_CONFIG,
                serde_json::json!({ "targetDeviceId": device.id }),
            ),
        )
        .await;
        match answer {
            Ok(Ok(reply)) => peers
                .boards
                .push(comet_board::doctor::peer_board(name, &reply)),
            // Answered, and the answer is "no board here".
            Ok(Err(e)) if hosts_no_board(&e) => {}
            // Never landed, or the budget ran out before it could.
            Ok(Err(_)) | Err(_) => {
                if present {
                    peers.unreachable.push(name);
                }
            }
        }
    }
    Ok(peers)
}

/// What one loopback conversation with the local engine yields.
///
/// A struct rather than a tuple because doctor wants nearly all of it and the
/// other two callers want one field: named holes read better than `(_, spaces,
/// _, _, _)` at three call sites.
struct EngineInfo {
    device: String,
    /// The engine binary's own version (gh#156). `None` from an engine older
    /// than the field — doctor then falls back to what is on disk.
    version: Option<String>,
    spaces: Vec<Space>,
    accounts: Vec<comet_proto::AgentAccount>,
    edge: Option<comet_proto::EdgeHealth>,
    /// How many people are in this workspace (gh#161). `None` when the engine
    /// could not be asked, or has no WorkOS session to ask with — doctor then
    /// says the fallback account was not checked rather than inventing a
    /// one-person box.
    members: Option<usize>,
    /// Which harnesses this device can actually start (gh#187). Empty from an
    /// engine too old to answer, which doctor reports as "not checked" rather
    /// than as a box that can run nothing.
    runtimes: Vec<comet_proto::view::board::RuntimeOption>,
    /// What the engine's release checker last saw at the edge (gh#197). `None`
    /// from an engine that has no updater attached or predates the verb.
    update: Option<comet_update::UpdateStatus>,
}

/// This device's spaces, from the engine: `LocalDevice` for the device id (and
/// the engine's version), the first `WatchSpaces` snapshot for the rows,
/// filtered to spaces this device owns — a route's `repo =` is a local path, and
/// another device's folders are not on this disk. Plus the engine's live
/// edge-connection census (gh#116), which is the one fact this loopback
/// conversation cannot otherwise reveal.
async fn fetch_spaces(port: u16) -> Result<EngineInfo> {
    let fetch = async {
        let client: RpcClient = connect_ws(&format!("ws://127.0.0.1:{port}")).await?;
        let local = client
            .call(methods::LOCAL_DEVICE, serde_json::json!({}))
            .await?;
        let device = local
            .get("deviceId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("LocalDevice reply missing deviceId"))?;
        // Optional by design: an engine predating gh#156 answers the identity
        // and says nothing about its version, which must not fail the dial.
        let version = local
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let mut stream = client
            .subscribe(methods::WATCH_SPACES, serde_json::json!({}))
            .await?;
        let snapshot = stream
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("WatchSpaces stream ended before a snapshot"))?;
        let spaces: Vec<Space> = serde_json::from_value(snapshot)?;
        let mine: Vec<Space> = spaces
            .into_iter()
            .filter(|s| s.device_id == device)
            .collect();
        // The saved agent logins, for the route `account` check. Offline list
        // (no `forceUsage`): doctor wants the ids and who they belong to, not
        // a round of rate-limit probes.
        let accounts: Vec<comet_proto::AgentAccount> = client
            .call(methods::LIST_AGENT_ACCOUNTS, serde_json::json!({}))
            .await
            .ok()
            .and_then(|v| serde_json::from_value::<comet_proto::AgentAccountsSnapshot>(v).ok())
            .map(|s| s.accounts)
            .unwrap_or_default();
        // Best-effort, like the accounts above: an engine too old to know the
        // method leaves doctor saying "not checked" rather than failing.
        let edge: Option<comet_proto::EdgeHealth> = client
            .call(methods::EDGE_HEALTH, serde_json::json!({}))
            .await
            .ok()
            .and_then(|v| serde_json::from_value(v).ok());
        // Best-effort again, and for one line of doctor's report: an engine in
        // dev mode or signed out answers an empty roster, and a workspace of
        // nobody is not a workspace of one — so an empty answer stays `None`.
        let members: Option<usize> = client
            .call(methods::LIST_MEMBERS, serde_json::json!({}))
            .await
            .ok()
            .and_then(|v| {
                v.get("members")
                    .and_then(|m| m.as_array())
                    .map(|m| m.len())
                    .filter(|n| *n > 0)
            });
        // What this box can actually run (gh#187) — best-effort like the rest:
        // an engine that predates the verb leaves doctor saying "not checked".
        let runtimes: Vec<comet_proto::view::board::RuntimeOption> = client
            .call(methods::LIST_BOARD_RUNTIMES, serde_json::json!({}))
            .await
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        // What the edge is handing out (gh#197), taken from the engine's own
        // release checker rather than fetched here: it is the process that
        // performs updates, and a second opinion could only disagree with it.
        // A stream because that is how the engine publishes it — the first
        // frame is the current status. An engine with no updater attached (or
        // one too old for the verb) ends the stream instead, which is `None`.
        let update: Option<comet_update::UpdateStatus> = match client
            .subscribe(methods::UPDATE_STATUS, serde_json::json!({}))
            .await
        {
            Ok(mut stream) => stream
                .recv()
                .await
                .and_then(|v| serde_json::from_value(v).ok()),
            Err(_) => None,
        };
        Ok::<_, anyhow::Error>(EngineInfo {
            device,
            version,
            spaces: mine,
            accounts,
            edge,
            members,
            runtimes,
            update,
        })
    };
    tokio::time::timeout(FETCH_TIMEOUT, fetch)
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}s", FETCH_TIMEOUT.as_secs()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two halves of gh#155's rule, on the verb this sweep uses: a refusal
    /// is the device's own answer, and a dead connection is not an answer at
    /// all.
    #[test]
    fn a_refusal_rules_a_device_out_and_a_dead_link_does_not() {
        assert!(hosts_no_board(&comet_rpc::RpcError::Refused(
            "board unavailable".into()
        )));
        assert!(hosts_no_board(&comet_rpc::RpcError::UnknownMethod(
            "ReadBoardConfig".into()
        )));
        assert!(!hosts_no_board(&comet_rpc::RpcError::Transport(
            "relay 500".into()
        )));
        assert!(!hosts_no_board(&comet_rpc::RpcError::Closed));
    }
}
