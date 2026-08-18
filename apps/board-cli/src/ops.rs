//! The agent-facing half of `comet-board`: list / dispatch / retry / cancel /
//! wait / new — §board-cli.
//!
//! Everything that reads or moves the live board goes through the engine's
//! typed RPC on the localhost IPC port, exactly as any viewport attaches: the
//! engine owns `board.db` and the board loop, and `WatchBoard` streams the rows
//! it derives. The JSON these commands print is herdr-board's `list --json`
//! contract verbatim (modulo the pane→chat renames documented on
//! [`TaskRow`]) — the agent conventions text depends on that shape, so it is
//! not ours to bend here.
//!
//! The board those RPCs reach need not be this device's: they are all
//! relay-forwardable (gh#55), so [`Board`] carries a `targetDeviceId` and
//! `--device` points the whole CLI at the box that hosts the board (gh#73).
//!
//! `new` is the exception to "ask the engine": it writes to the *trackers*
//! (Linear / GitHub), which sit upstream of the engine, so it speaks to them
//! directly with the same clients the sync loop uses.

use anyhow::{Context, Result, anyhow, bail};
use comet_board::adopt::{Unadopted, git_remote, github_slug};
use comet_board::claims::AttemptReview;
use comet_board::config::{self, Paths, RoutingConfig};
use comet_board::evidence::RunEvidence;
use comet_board::members::{Roster, Slot};
use comet_board::model::{BoardState, Source};
use comet_board::onboard::{Candidate, Onboarded};
use comet_board::routes::{RoutingView, cap_summary, match_summary};
use comet_board::rows::TaskRow;
use comet_board::runtime::{RuntimeOption, harness_for_runtime, runtime_name};
use comet_board::verdict::{self, Projection, VerdictReceipt};
use comet_proto::Device;
use comet_proto::view::board as view;
use comet_proto::view::stats::AggregateBoardStats;
use comet_rpc::{RpcClient, connect_ws, methods};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The engine answers a `WatchBoard` subscription with the current rows
/// immediately; anything slower than this is a listener that is not the engine.
pub const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// Set in a board-dispatched chat's environment (§dispatch-pipeline), so
/// `dispatch` from inside one inherits identity the way `HERDR_PANE_ID`
/// provided it under herdr.
pub const CHAT_ID_ENV: &str = "COMET_BOARD_CHAT_ID";

/// The engine connection plus which device's board it drives (gh#73).
///
/// The board store lives on exactly one device — usually the always-on box —
/// and every board RPC is relay-forwardable, so this dials the *local* engine
/// exactly as it always did and names the host in the params. That is the same
/// shape the desktop panel and the TUI pane send, and it is why `--device`
/// needs no new transport: the local engine forwards.
///
/// `host` is `None` for this device's own board, and then nothing extra is
/// sent — a single-device install speaks byte-for-byte what it spoke before.
pub struct Board {
    client: RpcClient,
    host: Option<String>,
}

/// Attach to the local engine, pointed at `device`'s board (`None` = this
/// device's own). A named device is resolved against the registered devices
/// first, so a typo costs an error naming the fleet rather than a call
/// forwarded into nothing.
pub async fn attach(port: u16, device: Option<&str>) -> Result<Board> {
    let client = connect_ws(&format!("ws://127.0.0.1:{port}"))
        .await
        .with_context(|| {
            format!(
                "connecting to the engine on 127.0.0.1:{port} — start `comet` or `comet headless`"
            )
        })?;
    let host = match device {
        Some(want) => Some(resolve_device(&client, want).await?),
        None => None,
    };
    Ok(Board { client, host })
}

impl Board {
    /// Merge the host's `targetDeviceId` passthrough into a call's params. The
    /// local board leaves them untouched.
    fn params(&self, value: serde_json::Value) -> serde_json::Value {
        with_target(value, self.host())
    }

    /// The device hosting the board, when it is not this one — for the error
    /// and confirmation text that has to say where the work landed.
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }
}

/// The `targetDeviceId` passthrough merged into a call's params (gh#55). `None`
/// — the board is this device's — leaves the params untouched, so a
/// single-device install sends exactly the shape it always did.
fn with_target(value: serde_json::Value, host: Option<&str>) -> serde_json::Value {
    let mut value = value;
    if let (Some(host), Some(object)) = (host, value.as_object_mut()) {
        object.insert("targetDeviceId".into(), serde_json::json!(host));
    }
    value
}

/// A device id for `--device`: an exact id, or a name as the device switcher
/// shows it. Names are the ones people have, ids are the ones that are unique;
/// accepting both and refusing an ambiguous name is the only honest reading.
async fn resolve_device(client: &RpcClient, want: &str) -> Result<String> {
    let mut stream = client
        .subscribe(methods::WATCH_DEVICES, serde_json::json!({}))
        .await
        .context("listing devices")?;
    let first = tokio::time::timeout(SNAPSHOT_TIMEOUT, stream.recv())
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {}s listing devices",
                SNAPSHOT_TIMEOUT.as_secs()
            )
        })?
        .ok_or_else(|| anyhow!("the device stream ended before a snapshot"))?;
    let devices: Vec<Device> = serde_json::from_value(first).context("parsing devices")?;
    pick_device(&devices, want)
}

/// `--device`'s value against the registered devices: an id wins outright, then
/// a case-insensitive name. Two devices sharing a name is a real thing (two
/// laptops called `laptop`), and picking one would send the work somewhere the
/// operator did not choose — so that asks for the id instead.
fn pick_device(devices: &[Device], want: &str) -> Result<String> {
    if let Some(d) = devices.iter().find(|d| d.id == want) {
        return Ok(d.id.clone());
    }
    let named: Vec<&Device> = devices
        .iter()
        .filter(|d| d.name.eq_ignore_ascii_case(want))
        .collect();
    match named.as_slice() {
        [d] => Ok(d.id.clone()),
        [] => bail!("no device `{want}`; this org has: {}", device_list(devices)),
        several => bail!(
            "`{want}` names {} devices; use the id: {}",
            several.len(),
            several
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn device_list(devices: &[Device]) -> String {
    if devices.is_empty() {
        return "none registered".to_string();
    }
    devices
        .iter()
        .map(|d| format!("{} ({})", d.name, d.id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The first `WatchBoard` snapshot. A stream that ends before one arrives is a
/// reachable engine whose board is not running: the RPC layer folds the
/// server's error into end-of-stream, so name the likely cause here.
async fn snapshot(
    stream: &mut tokio::sync::mpsc::Receiver<serde_json::Value>,
    host: Option<&str>,
) -> Result<Vec<TaskRow>> {
    let first = tokio::time::timeout(SNAPSHOT_TIMEOUT, stream.recv())
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {}s waiting for board rows",
                SNAPSHOT_TIMEOUT.as_secs()
            )
        })?
        .ok_or_else(|| no_board(host))?;
    serde_json::from_value(first).context("parsing board rows")
}

/// "Said nothing at all" IS the answer: the engine refuses `WatchBoard` outright
/// when it hosts no board, and the RPC layer folds that refusal into
/// end-of-stream. Which device was asked decides what to suggest.
fn no_board(host: Option<&str>) -> anyhow::Error {
    match host {
        Some(device) => anyhow!(
            "device {device} hosts no board — its stream ended before a snapshot; \
             the board is disabled there (COMET_BOARD=0) or its service failed to start"
        ),
        None => anyhow!(
            "the board stream ended before a snapshot — this device's board is disabled \
             (COMET_BOARD=0) or its service failed to start (see the engine log); if the \
             board lives on another device, name it with --device"
        ),
    }
}

/// Current board rows, in board order — one snapshot of what `WatchBoard`
/// streams.
pub async fn board_rows(board: &Board) -> Result<Vec<TaskRow>> {
    let mut stream = board
        .client
        .subscribe(methods::WATCH_BOARD, board.params(serde_json::json!({})))
        .await?;
    snapshot(&mut stream, board.host()).await
}

/// The on-demand union of every board host reachable from this engine.
pub async fn aggregate_stats(
    board: &Board,
    since_days: Option<i64>,
) -> Result<AggregateBoardStats> {
    let mut params = serde_json::json!({});
    if let (Some(days), Some(object)) = (since_days, params.as_object_mut()) {
        object.insert("sinceDays".into(), serde_json::json!(days));
    }
    let reply = board
        .client
        .call(methods::AGGREGATE_BOARD_STATS, board.params(params))
        .await
        .context("reading aggregate board stats")?;
    serde_json::from_value(reply).context("parsing AggregateBoardStats reply")
}

/// The row named by a canonical id or an unambiguous display identifier.
///
/// This is the wire-row twin of `comet_board::dispatch::task_by_reference`:
/// exact ids win, while a short identifier shared by repositories is refused
/// with every canonical candidate named. RPC consumers only have [`TaskRow`]
/// values, so they cannot call the board-core helper over stored `Task`s.
pub fn task_row_by_reference<'a>(rows: &'a [TaskRow], reference: &str) -> Result<&'a TaskRow> {
    let reference = reference.trim();
    if let Some(row) = rows.iter().find(|row| row.id == reference) {
        return Ok(row);
    }
    let named: Vec<&TaskRow> = rows
        .iter()
        .filter(|row| row.identifier.eq_ignore_ascii_case(reference))
        .collect();
    match named.as_slice() {
        [] => bail!("`{reference}` is not a task on the board"),
        [only] => Ok(only),
        several => bail!(
            "`{reference}` names {} tasks on the board — pass the task id to say which: {}",
            several.len(),
            several
                .iter()
                .map(|row| format!("`{}`", row.id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

// ---- list ---------------------------------------------------------------

/// Reject an unknown filter rather than returning an empty list: to a caller,
/// `[]` from a typo is indistinguishable from `[]` meaning "nothing is ready",
/// and the second is a normal answer worth acting on.
pub fn validate_filters(state: Option<&str>, source: Option<&str>) -> Result<()> {
    if let Some(want) = state
        && BoardState::parse(want).is_none()
    {
        bail!(
            "unknown state `{want}`; expected one of: {}",
            state_names().join(", ")
        );
    }
    if let Some(want) = source
        && Source::parse(want).is_none()
    {
        bail!("unknown source `{want}`; expected linear or github");
    }
    Ok(())
}

fn state_names() -> Vec<&'static str> {
    BoardState::SECTION_ORDER
        .iter()
        .map(|s| s.as_str())
        .collect()
}

pub fn filter_rows(rows: Vec<TaskRow>, state: Option<&str>, source: Option<&str>) -> Vec<TaskRow> {
    rows.into_iter()
        .filter(|r| state.is_none_or(|want| r.state == want))
        .filter(|r| source.is_none_or(|want| r.source == want))
        .collect()
}

pub fn print_tasks(rows: &[TaskRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("nothing on the board");
        return Ok(());
    }
    for r in rows {
        let extra = match (&r.pr_url, r.dispatchable) {
            // Two different reasons a row cannot be dispatched, and calling the
            // second one "no route" would send you to routing.toml for nothing.
            _ if r.gone => "  (gone upstream)".to_string(),
            (Some(pr), _) => format!("  {pr}"),
            (None, false) => "  (no route)".to_string(),
            _ => String::new(),
        };
        // How full the live agent's window is, in the same words the two
        // viewports use (gh#271) — and only once there is something to say.
        let extra = match comet_proto::view::board::context_note(r.context) {
            Some(note) => format!("{extra}  ({note})"),
            None => extra,
        };
        // Which layer of a stack this is, and what merging it would really do
        // (gh#283). Only for stacked rows: for a standalone pull request
        // `mergeable_state` says what it appears to say, and the printed list
        // has never carried it. For a layer it does not — `clean` there is
        // clean against the layer below — so the row that would mislead is
        // exactly the row that speaks up. `--json` carries `landing` for all.
        let extra = match (
            comet_proto::view::board::stack_note(r),
            comet_proto::view::board::landing_note(r),
        ) {
            (Some(stack), Some(landing)) => format!("{extra}  ({stack}, {landing})"),
            (Some(stack), None) => format!("{extra}  ({stack})"),
            (None, _) => extra,
        };
        println!(
            "{:<8} {:<24} {:<10} {}{}",
            r.state,
            r.id,
            r.workspace.as_deref().unwrap_or("-"),
            truncate(&r.title, 48),
            extra
        );
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

// ---- the review contract (§gh#183) --------------------------------------

/// Record what this attempt did, and read back what the claims did not account
/// for.
///
/// The raw block goes over the wire unparsed on purpose: the format is enforced
/// on the board's host, so a refusal is the same refusal whichever client sent
/// it, and the CLI cannot become a second, laxer definition of the contract.
pub async fn submit_claims(board: &Board, task_id: &str, text: &str) -> Result<AttemptReview> {
    let params = board.params(serde_json::json!({ "taskId": task_id, "text": text }));
    let reply = board.client.call(methods::SUBMIT_CLAIMS, params).await?;
    serde_json::from_value(reply).context("parsing SubmitClaims reply")
}

/// One attempt's review. `None` is the task's latest attempt.
pub async fn attempt_review(
    board: &Board,
    task_id: &str,
    attempt: Option<i64>,
) -> Result<AttemptReview> {
    let mut params = serde_json::json!({ "taskId": task_id });
    if let (Some(attempt), Some(object)) = (attempt, params.as_object_mut()) {
        object.insert("attempt".into(), serde_json::json!(attempt));
    }
    let reply = board
        .client
        .call(methods::READ_ATTEMPT_REVIEW, board.params(params))
        .await?;
    serde_json::from_value(reply).context("parsing ReadAttemptReview reply")
}

/// The whole review, printed in the order the question is asked in: what was
/// asked, what the agent says it did, what the board saw for itself, and what
/// nobody accounted for.
///
/// The remainder goes last because it is what a reader should be left holding,
/// and it is the only section that is loud when it is non-empty.
pub fn print_review(review: &AttemptReview, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(review)?);
    } else {
        print!("{}", render_review(review));
    }
    Ok(())
}

/// What `claim` prints back: the receipt, and then the only part that matters —
/// the changes the agent has just failed to account for.
pub fn print_claim_result(review: &AttemptReview, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(review)?);
    } else {
        print!("{}", render_claim_result(review));
    }
    Ok(())
}

/// Submit a verdict on an attempt's pull request (§gh#239).
///
/// The unclaimed changes are not a parameter here either: they are derived on
/// the board's host and attached to both copies, so a caller cannot send a
/// verdict whose remainder disagrees with the one `review` just printed it.
pub async fn submit_verdict(
    board: &Board,
    task_id: &str,
    attempt: Option<i64>,
    kind: &str,
    comment: &str,
) -> Result<VerdictReceipt> {
    let mut params = serde_json::json!({
        "taskId": task_id, "kind": kind, "comment": comment
    });
    if let (Some(attempt), Some(object)) = (attempt, params.as_object_mut()) {
        object.insert("attempt".into(), serde_json::json!(attempt));
    }
    let reply = board
        .client
        .call(methods::SUBMIT_VERDICT, board.params(params))
        .await?;
    serde_json::from_value(reply).context("parsing SubmitVerdict reply")
}

/// What `verdict` prints back: where it went, and — the part worth reading —
/// where it did not.
pub fn print_verdict(receipt: &VerdictReceipt, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(receipt)?);
    } else {
        print!("{}", render_verdict(receipt));
    }
    Ok(())
}

pub fn render_verdict(receipt: &VerdictReceipt) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "· {} recorded{}",
        receipt.kind.label(),
        if receipt.recorded {
            ""
        } else {
            " — already submitted, nothing sent twice"
        }
    );
    match (receipt.delivered, receipt.not_delivered.as_deref()) {
        (true, _) => {
            let _ = writeln!(
                out,
                "· delivered into chat {}",
                receipt.chat_id.as_deref().unwrap_or("?")
            );
        }
        (false, Some(why)) => {
            let _ = writeln!(out, "! not delivered into the chat: {why}");
        }
        (false, None) => {}
    }
    // A verdict GitHub would not take is the loud line, not a missing one: it
    // stands and the agent has it, and nobody on the pull request knows (gh#365).
    let _ = writeln!(
        out,
        "{} {}",
        if receipt.projection == Projection::Posted {
            "·"
        } else {
            "!"
        },
        verdict::projection_line(
            receipt.kind,
            receipt.projection,
            receipt.refused.as_deref(),
            receipt.posted_as.as_deref(),
        ),
    );
    if receipt.unclaimed > 0 {
        let _ = writeln!(
            out,
            "· {} unclaimed change{} attached to both",
            receipt.unclaimed,
            if receipt.unclaimed == 1 { "" } else { "s" }
        );
    }
    out
}

/// Rendered rather than printed, so what a reviewer reads is testable. The
/// sections are the question in order, and the remainder is last because it is
/// what a reader should be left holding.
pub fn render_review(review: &AttemptReview) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{} · {}", review.brief.identifier, review.brief.title);
    let _ = writeln!(out, "  {}", review.brief.url);
    // "attempt 0 · still running" would be two lies about a pull request
    // nobody dispatched (§gh#344), so that line says what happened instead.
    if review.undispatched() {
        let _ = write!(out, "  no attempt");
        if let Some(branch) = &review.branch {
            let _ = write!(out, " · {branch}");
        }
        let _ = writeln!(out, " · opened outside the board");
    } else {
        let _ = write!(out, "  attempt {}", review.attempt);
        if let Some(branch) = &review.branch {
            let _ = write!(out, " · {branch}");
        }
        match &review.outcome {
            Some(outcome) => {
                let _ = writeln!(out, " · {outcome}");
            }
            None => {
                let _ = writeln!(out, " · still running");
            }
        }
    }
    if let Some(pr) = &review.pr_url {
        let _ = writeln!(out, "  {pr}");
    }
    // Whether it can land, and — for a layer of a stack — that it *is* one
    // (§gh#389). Never the flat `mergeable_state`, which mid-stack says `clean`
    // about a pull request nothing will let through until the ones below it go
    // first. Silent on a pull request nobody has asked GitHub about, which is
    // the ordinary state of a fresh row.
    if let Some(note) = comet_proto::view::board::landing_note(review.stacked()) {
        let _ = writeln!(out, "  {note}");
    }
    if let Some(line) = comet_proto::view::board::stack_line(review.stacked()) {
        let _ = writeln!(out, "  {line}");
        let map: Vec<String> = comet_proto::view::board::stack_map(review.stacked())
            .iter()
            .map(comet_proto::view::board::layer_label)
            .collect();
        let _ = writeln!(out, "    {}", map.join(" ↑ "));
        if let Some(order) = comet_proto::view::board::merge_order(review.stacked()) {
            let _ = writeln!(out, "    {order}");
        }
    }
    // The verdict, before any of the sections it was derived from. Read from
    // `claims::verdict` rather than phrased here, so this terminal and the
    // desktop review screen cannot come to different conclusions about the
    // same attempt.
    let verdict = review.verdict();
    let _ = writeln!(
        out,
        "  {} {}",
        if verdict.tone.loud() { "!" } else { "·" },
        verdict.text
    );
    // Under the verdict and above the evidence it qualifies (§gh#349): what
    // this run was actually permitted to do. A reviewer weighing "the tests
    // passed" is entitled to know whether the agent that ran them was confined
    // to its checkout. Marked `?` rather than `!` — it is a term of the run,
    // not a finding against it, and [`AttemptReview::sandbox_note`] says why it
    // stays out of the verdict.
    if let Some(note) = review.sandbox_note() {
        let _ = writeln!(out, "  ? {note}");
    }
    out.push('\n');
    effects_section(review, &mut out);
    out.push('\n');
    claims_section(review, &mut out);
    out.push('\n');
    evidence_section(&review.evidence, &mut out);
    out.push('\n');
    remainder_section(review, &mut out);
    out
}

pub fn render_claim_result(review: &AttemptReview) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "recorded {} claim(s) on {} attempt {}",
        review.remainder.claims.len(),
        review.brief.identifier,
        review.attempt
    );
    // A claim the diff does not support is the other half of the answer: work
    // described that did not happen, said as loudly as work nobody described.
    for claim in review.remainder.claims.iter().filter(|c| !c.anchored()) {
        let _ = writeln!(out, "  ! nothing in the diff matches: {}", claim.text);
    }
    out.push('\n');
    remainder_section(review, &mut out);
    out
}

/// The effects row (§gh#236): what the board read off the branch itself, above
/// anything the agent said about it.
///
/// Above the claims for the same reason it sits above them on the desktop
/// screen — a reader who has already been told a fluent story about the work
/// reads the numbers as confirmation of it. These come first so they are read
/// as what they are.
fn effects_section(review: &AttemptReview, out: &mut String) {
    use std::fmt::Write;
    let _ = writeln!(out, "EFFECTS");
    for chip in review.effect_chips() {
        let _ = writeln!(out, "  {} {}", ground_mark(chip.ground), chip.text);
    }
}

/// One glyph per [`Ground`], so a terminal reader gets the distinction the
/// colours carry on the desktop. `?` is the one that matters: a fact the board
/// could not establish must not read like a fact it established.
fn ground_mark(ground: comet_board::effects::Ground) -> &'static str {
    use comet_board::effects::Ground;
    match ground {
        Ground::Neutral => "·",
        Ground::Settled => "✓",
        Ground::Working => "*",
        Ground::Unknown => "?",
    }
}

fn claims_section(review: &AttemptReview, out: &mut String) {
    use std::fmt::Write;
    // A block that was written and could not be read (§gh#235). Printed whole,
    // unlike the verdict's one line: this is the section a reviewer scrolls to
    // when the verdict tells them the claims went missing, and the refusal
    // names the line that did it.
    if let Some(err) = &review.claims_error {
        let _ = writeln!(
            out,
            "CLAIMS — a block was written and would not parse; nothing was recorded"
        );
        for line in err.lines() {
            let _ = writeln!(out, "  ! {}", line.trim());
        }
        return;
    }
    if !review.claimed() {
        let _ = writeln!(
            out,
            "{}",
            if review.undispatched() {
                // Nobody was told the contract, so nobody ignored it (§gh#344).
                "CLAIMS — none; nothing dispatched this pull request, so no agent was \
                 ever told the contract"
            } else {
                "CLAIMS — none submitted; this attempt never answered the contract"
            }
        );
        return;
    }
    let _ = writeln!(
        out,
        "CLAIMS ({}, submitted {})",
        review.remainder.claims.len(),
        review.claimed_at.as_deref().unwrap_or("—")
    );
    for claim in &review.remainder.claims {
        // `!` is a claim nothing in the diff supports — a claim about work that
        // did not happen, which is at least as interesting as an unclaimed
        // file. `✓` is one something the agent did not author stands behind,
        // and `·` is the middle: anchored, and corroborated by nothing
        // (§gh#236).
        let _ = writeln!(out, "  {} {}", review.claim_mark(claim).glyph(), claim.text);
        if !claim.matched.is_empty() {
            let _ = writeln!(out, "      {}", claim.matched.join(", "));
        }
        // The evidence that attaches to this one claim, under it.
        for chip in review.claim_chips(claim) {
            let _ = writeln!(out, "      {} {}", ground_mark(chip.ground), chip.text);
        }
        // A path nothing happened to and a symbol no changed line names are
        // the same finding said about different anchors, and a reviewer needs
        // to know which they are looking at before going to check it (§gh#235).
        for anchor in &claim.unmatched {
            let _ = match comet_board::claims::anchor_kind(anchor) {
                comet_board::claims::AnchorKind::Path => {
                    writeln!(out, "      (unchanged: {anchor})")
                }
                comet_board::claims::AnchorKind::Symbol => {
                    writeln!(out, "      (no changed line names: {anchor})")
                }
            };
        }
    }
}

fn evidence_section(evidence: &RunEvidence, out: &mut String) {
    use std::fmt::Write;
    if evidence.commands == 0 {
        let _ = writeln!(
            out,
            "EVIDENCE — the board recorded no commands for this run"
        );
        return;
    }
    let _ = writeln!(
        out,
        "EVIDENCE ({} command(s) ran, {} exited non-zero)",
        evidence.commands, evidence.failed
    );
    if !evidence.checked() {
        let _ = writeln!(out, "  ! nothing that checks anything ran");
        return;
    }
    for check in &evidence.checks {
        let tail = match (check.runs, check.failed) {
            (1, 0) => String::new(),
            (runs, 0) => format!("  ×{runs}"),
            (runs, failed) => format!("  ×{runs}, {failed} failed"),
        };
        let _ = writeln!(
            out,
            "  {} {}{tail}",
            if check.ever_passed() { "·" } else { "!" },
            check.command
        );
    }
    if evidence.truncated {
        let _ = writeln!(out, "  (…and more; the list is capped)");
    }
}

fn remainder_section(review: &AttemptReview, out: &mut String) {
    use std::fmt::Write;
    if let comet_board::claims::DiffSource::Unavailable { reason } = &review.diff {
        let _ = writeln!(out, "UNCLAIMED — unknown: {reason}");
        return;
    }
    let total = review.changed.len();
    if review.remainder.complete() {
        let _ = writeln!(
            out,
            "UNCLAIMED — none; all {total} changed file(s) are accounted for"
        );
    } else {
        let _ = writeln!(
            out,
            "UNCLAIMED ({} of {total} changed file(s))",
            review.remainder.unclaimed.len()
        );
        for file in &review.remainder.unclaimed {
            let _ = writeln!(
                out,
                "  {:<2} {:<52} {}",
                file.status,
                file.path,
                file.counts()
            );
        }
    }
    if matches!(review.diff, comet_board::claims::DiffSource::Recorded) {
        let _ = writeln!(
            out,
            "  (from the diff the board recorded; the checkout is gone)"
        );
    }
    if matches!(review.diff, comet_board::claims::DiffSource::PullRequest) {
        let _ = writeln!(
            out,
            "  (from GitHub's file list for the pull request; nothing ran here)"
        );
    }
    // The one thing that would make an empty remainder a lie: work that is in
    // the checkout and not on the branch has been shown to nobody.
    if let Some(n) = review.uncommitted.filter(|n| *n > 0) {
        let _ = writeln!(
            out,
            "  ! {n} file(s) changed in the checkout and not committed — \
             not on the branch, so not in the diff above"
        );
    }
}

// ---- dispatch / retry / cancel ------------------------------------------

/// What `DispatchTask` answers: the attempt's address.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dispatched {
    pub chat_id: String,
    pub cwd: String,
    pub attempt: usize,
}

/// Everything a release carries besides the task itself.
#[derive(Debug, Default, Clone, Copy)]
pub struct DispatchOpts<'a> {
    /// The dispatching chat's id — provenance, never authority.
    pub via: Option<&'a str>,
    /// Runtime override for this one dispatch; `None` = the route's.
    pub runtime: Option<&'a str>,
    /// Model override for the chosen harness.
    pub model: Option<&'a str>,
    /// Agent-account slot whose subscription this run spends.
    pub account: Option<&'a str>,
    /// `--bill`: "spend that account, I know whose it is" (gh#101). Names the
    /// payer — a slot id, which also selects it, or the email on the login
    /// being spent. What `billing_guard = "require-own"` accepts instead of
    /// refusing; inert under `warn` and `off` beyond picking the slot.
    pub bill: Option<&'a str>,
    /// Who this shell says is releasing the work — the WorkOS user signed in on
    /// this device (gh#74). Resolved by [`signed_in_email`] rather than passed
    /// by hand: the CLI is a frontend like the other two, and without it the
    /// billing guard has nothing to compare an account against.
    pub via_user: Option<&'a str>,
    /// `--onto`: stack this release on another task — cut it from the branch
    /// that task's attempt holds, and open its pull request against that branch
    /// rather than trunk (gh#285). A task id or the identifier on the board.
    pub onto: Option<&'a str>,
    /// `--base`: cut this release from that branch instead of the route's
    /// `base`. The escape hatch for a branch no task on the board holds;
    /// passing it together with `--onto` is refused by the engine.
    pub base: Option<&'a str>,
    /// End the task's live attempt and release a fresh one — `retry` on a
    /// blocked row (gh#49), and the one deliberate breach of the
    /// one-live-attempt rule. Ordinary dispatches send `false` and are refused
    /// on a live attempt.
    pub replace: bool,
    /// `--stack`: ask the agent to decompose the task into a stack of layered
    /// pull requests (gh#287). Adds a block to the brief and nothing else — the
    /// layers are the agent's to design and `gh stack`'s to create.
    pub stack: bool,
    /// `--decompose`: ask the agent to split the task into tickets and release
    /// each to an agent of its own (gh#340). Adds a block to the brief and
    /// nothing else; naming it together with `--stack` is refused by the
    /// engine (and by clap before that).
    pub decompose: bool,
}

/// Whether `retry` on a row in this state has to replace a live attempt.
///
/// Only `blocked`: its agent is alive and waiting on input, so the
/// one-live-attempt rule would refuse a plain dispatch, and ending it is the
/// whole point of retrying. A `failed` attempt is already closed — replacing
/// nothing would be a lie in the log — so that one is an ordinary release, and
/// so is a `ready` row that never got off the ground. Anything else keeps its
/// live attempt and is left to the engine's own refusal, which names the chat.
///
/// Exactly the desktop panel's rule (`crates/ui/src/board.rs`), so the same row
/// retried from a shell and from the panel takes the same path.
pub fn retry_replaces(state: &str) -> bool {
    state == BoardState::Blocked.as_str()
}

/// The dispatching chat's id: `--via` when given, else the identity a
/// board-dispatched chat inherits from its environment. Provenance, never
/// authority — the engine records it on the attempt and nothing else.
pub fn provenance(flag: Option<String>) -> Option<String> {
    provenance_from(flag, std::env::var(CHAT_ID_ENV).ok())
}

fn provenance_from(flag: Option<String>, env: Option<String>) -> Option<String> {
    flag.or(env).filter(|s| !s.is_empty())
}

// ---- whose subscription (gh#101) ----------------------------------------

/// The email of the WorkOS user signed in on THIS device, when there is one.
///
/// Local on purpose, and not forwarded to the board's host: the question is
/// "who is running this shell", and the box's own signed-in user is the wrong
/// answer to it. `None` for a signed-out engine, an engine that answers
/// something else, and any failure at all — this decorates a dispatch, it must
/// never be what stops one.
pub async fn signed_in_email(board: &Board) -> Option<String> {
    let mut stream = board
        .client
        .subscribe(methods::AUTH_STATUS, serde_json::json!({}))
        .await
        .ok()?;
    let first = tokio::time::timeout(SNAPSHOT_TIMEOUT, stream.recv())
        .await
        .ok()??;
    match serde_json::from_value::<comet_proto::AuthState>(first).ok()? {
        comet_proto::AuthState::SignedIn { user, .. }
        | comet_proto::AuthState::NeedsOrganization { user } => {
            Some(user.email).filter(|e| !e.is_empty())
        }
        comet_proto::AuthState::SignedOut => None,
    }
}

/// The one line `dispatch` and `retry` print before releasing a run that
/// charges somebody else — `None` when this one does not, which is most of
/// them.
///
/// Resolved here rather than read back off the reply because it has to be said
/// *before* the release: by the time `DispatchTask` answers, the worktree is
/// cut and the agent is running on the account in question. The engine applies
/// the guard itself either way (and is the only one that can refuse under
/// `require-own`) — this is the CLI keeping the promise the pickers keep, that
/// nobody spends a subscription without being told whose.
///
/// Every lookup degrades to silence. A board that cannot say whose login a slot
/// is has not found a problem, and grounding a legitimate dispatch on a failed
/// lookup would be the wrong half of this feature.
pub async fn cross_billing_warning(
    board: &Board,
    task_id: &str,
    opts: DispatchOpts<'_>,
) -> Option<String> {
    // Nobody to compare against: an unattributed dispatch names no wronged
    // party, and asking the box two more questions could not change that.
    let me = opts.via_user?;
    let rows = board_rows(board).await.ok()?;
    let row = task_row_by_reference(&rows, task_id).ok()?;
    match cross_billing_preflight_for_row(board, row, opts, Some(me)).await {
        CrossBillingPreflight::DifferentPayer(warning) => Some(warning),
        CrossBillingPreflight::SamePayer | CrossBillingPreflight::Unknown(_) => None,
    }
}

/// What the available account evidence says about a pending dispatch's payer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrossBillingPreflight {
    SamePayer,
    DifferentPayer(String),
    Unknown(String),
}

/// Whether dispatching `row` under `opts` keeps or changes an explicitly
/// supplied payer, without collapsing unavailable evidence into permission.
///
/// Human frontends pass their signed-in identity through
/// [`cross_billing_warning`]. The board MCP consumer instead passes the parent
/// attempt's recorded `billed_to`: that is budget provenance, not a claim that
/// the parent agent is the human who released its child.
pub async fn cross_billing_preflight_for_row(
    board: &Board,
    row: &TaskRow,
    opts: DispatchOpts<'_>,
    payer: Option<&str>,
) -> CrossBillingPreflight {
    let Some(payer) = payer.filter(|payer| !payer.trim().is_empty()) else {
        return CrossBillingPreflight::Unknown(
            "the parent attempt has no recorded payer".to_string(),
        );
    };
    // Exactly the chain `build_spec` walks: `--bill <slot>`, then `--account`,
    // then the route's own — which is what the row reports for a task nothing
    // has run on yet.
    let slot = opts
        .bill
        .filter(|b| comet_board::billing::bill_names_a_slot(b))
        .or(opts.account)
        .or(row.account.as_deref());
    let Some(runtime) = opts.runtime.or(row.runtime.as_deref()) else {
        return CrossBillingPreflight::Unknown(
            "the target task has no resolved runtime".to_string(),
        );
    };
    let Some(harness) = harness_for_runtime(runtime) else {
        return CrossBillingPreflight::Unknown(format!(
            "the target runtime `{runtime}` does not identify a known agent harness"
        ));
    };
    let accounts = match board
        .client
        .call(
            methods::LIST_AGENT_ACCOUNTS,
            board.params(serde_json::json!({})),
        )
        .await
    {
        Ok(value) => match serde_json::from_value::<comet_proto::AgentAccountsSnapshot>(value) {
            Ok(accounts) => accounts,
            Err(err) => {
                return CrossBillingPreflight::Unknown(format!(
                    "the agent accounts reply was malformed: {err}"
                ));
            }
        },
        Err(err) => {
            return CrossBillingPreflight::Unknown(format!(
                "the agent accounts could not be read: {err}"
            ));
        }
    };
    let Some(billed) = view::billed_email(&accounts.accounts, harness, slot) else {
        let account = slot
            .map(|slot| format!("account `{slot}`"))
            .unwrap_or_else(|| "the active account".to_string());
        return CrossBillingPreflight::Unknown(format!(
            "the agent accounts do not identify a payer for {account} under runtime `{runtime}`"
        ));
    };
    if view::cross_billed(Some(billed), Some(payer)) {
        CrossBillingPreflight::DifferentPayer(view::bills_warning(billed, harness))
    } else {
        CrossBillingPreflight::SamePayer
    }
}

/// One model a dispatch can be pointed at, as `ListModels` reports it for a
/// harness — `id` is exactly what the `DispatchTask` override sends, so it is
/// the only field a check can be made against.
#[derive(Debug, Deserialize)]
struct ModelChoice {
    id: String,
}

/// `dispatch`, with `--runtime` / `--model` checked against the engine's own
/// catalogs first — the same two calls the desktop picker fills its rows from
/// (`ListBoardRuntimes`, `ListModels { harness }`), so the CLI refuses exactly
/// what the picker would not have offered.
///
/// Worth a round-trip before the verb: the engine rejects an unknown *runtime*
/// on its own, but an unknown *model* is the harness's business, and by the
/// time the harness sees it the dispatch has already cut a worktree, made a
/// chat and started an agent. A typo should cost an error, not an attempt.
pub async fn dispatch_checked(
    board: &Board,
    task_id: &str,
    opts: DispatchOpts<'_>,
) -> Result<Dispatched> {
    // A model with no runtime beside it runs under the row's — which is what
    // `list --json` shows as that row's `runtime`, and what `--runtime`'s help
    // points at. Only fetched when it is the missing half of the answer.
    let row_runtime = match (opts.model, opts.runtime) {
        (Some(_), None) => board_rows(board)
            .await
            .ok()
            .and_then(|rows| rows.into_iter().find(|r| r.id == task_id))
            .and_then(|r| r.runtime),
        _ => None,
    };
    check_overrides(board, opts.runtime, opts.model, row_runtime.as_deref()).await?;
    dispatch(board, task_id, opts).await
}

/// `retry`, deciding from the row itself whether the release has to replace a
/// live attempt (gh#73). Reading the row is not optional: `replace` is a real
/// cancellation, and sending it unconditionally would let `retry` end a
/// *working* agent that nobody asked to interrupt.
pub async fn retry(board: &Board, task_id: &str, opts: DispatchOpts<'_>) -> Result<Retried> {
    let row = board_rows(board)
        .await?
        .into_iter()
        .find(|r| r.id == task_id)
        .ok_or_else(|| anyhow!("{task_id} is not on the board"))?;
    let replace = retry_replaces(&row.state);
    let dispatched = dispatch_checked(board, task_id, DispatchOpts { replace, ..opts }).await?;
    // An ordinary blocked retry mints the next attempt after cancelling the
    // live chat.
    let replaced = replace && dispatched.attempt > row.attempts;
    Ok(Retried {
        dispatched,
        replaced,
        was: row.state,
    })
}

/// What `retry` did, so the confirmation can say whether an attempt was ended
/// to make room — the difference between a retry and a first dispatch is the
/// agent that was killed, and that is worth one line.
pub struct Retried {
    pub dispatched: Dispatched,
    pub replaced: bool,
    /// The state the row was in when the retry was decided.
    pub was: String,
}

/// Refuse an override the dispatch would only choke on later.
///
/// A catalog that cannot be read proves nothing either way, so it degrades to
/// a note on stderr and lets the dispatch through — the same choice the desktop
/// picker makes when `ListBoardRuntimes` fails (it dispatches with the route's
/// rather than trapping the operator). Refusing there would ground a legitimate
/// dispatch on the strength of a failed lookup.
async fn check_overrides(
    board: &Board,
    runtime: Option<&str>,
    model: Option<&str>,
    row_runtime: Option<&str>,
) -> Result<()> {
    if runtime.is_none() && model.is_none() {
        return Ok(());
    }

    let mut chosen = runtime.or(row_runtime).map(str::to_string);
    if let Some(want) = runtime {
        match board
            .client
            .call(
                methods::LIST_BOARD_RUNTIMES,
                board.params(serde_json::json!({})),
            )
            .await
            .context("listing runtimes")
            .and_then(|v| {
                serde_json::from_value::<Vec<RuntimeOption>>(v).context("parsing runtimes")
            }) {
            Ok(options) => chosen = Some(resolve_runtime(&options, want)?),
            Err(e) => eprintln!(
                "note: could not list runtimes ({e:#}) — dispatching with `{want}` unchecked"
            ),
        }
    }

    let Some(want) = model else { return Ok(()) };
    // Which harness's catalog to ask about. `ListModels` is keyed by harness
    // id, and the canonical runtime name *is* that id — an alias spelling
    // (`claude`, `openai-codex`) has to be resolved through the same table the
    // engine's own `build_spec` uses, or the lookup asks about nothing. It
    // carries the host too: the run executes there, so the catalog a dispatch
    // is checked against has to be that device's.
    let Some(harness) = chosen
        .as_deref()
        .and_then(harness_for_runtime)
        .map(runtime_name)
    else {
        // No runtime to attribute the model to: an unrouted row, or one whose
        // runtime is not a comet harness. The dispatch itself will say so.
        return Ok(());
    };

    match board
        .client
        .call(
            methods::LIST_MODELS,
            board.params(serde_json::json!({ "harness": harness })),
        )
        .await
        .context("listing models")
        .and_then(|v| serde_json::from_value::<Vec<ModelChoice>>(v).context("parsing models"))
    {
        Ok(models) => check_model(&models, want, harness),
        Err(e) => {
            eprintln!(
                "note: could not list {harness} models ({e:#}) — dispatching with `{want}` unchecked"
            );
            Ok(())
        }
    }
}

/// The canonical runtime name for an override, or an error naming what the
/// engine does offer.
///
/// Refuses a runtime the host cannot start as firmly as one it has never heard
/// of (gh#187), and for a sharper reason: an unknown name fails at `build_spec`
/// having cost nothing, while a known-but-absent one used to fail at the
/// harness spawn — after a worktree, a chat and a queued brief. The engine
/// refuses it too; this is the same refusal one round trip earlier, in the
/// same words, listing what the box *can* run.
fn resolve_runtime(options: &[RuntimeOption], want: &str) -> Result<String> {
    // The catalog offers one canonical name per harness; `routing.toml`'s alias
    // spellings are valid input the picker has no reason to list twice, so an
    // alias resolves to the canonical entry rather than being refused.
    let canonical = harness_for_runtime(want).map(runtime_name);
    let option = options
        .iter()
        .find(|o| o.name.eq_ignore_ascii_case(want) || Some(o.name.as_str()) == canonical)
        .ok_or_else(|| {
            anyhow!(
                "unknown runtime `{want}`; the engine offers: {}",
                options
                    .iter()
                    .map(|o| o.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    if let Some(reason) = option.unavailable {
        let ready: Vec<&str> = options
            .iter()
            .filter(|o| o.available())
            .map(|o| o.name.as_str())
            .collect();
        bail!(
            "{}. This box can run: {}",
            reason.refusal(&option.name),
            if ready.is_empty() {
                "nothing".to_string()
            } else {
                ready.join(", ")
            }
        );
    }
    Ok(option.name.clone())
}

fn check_model(models: &[ModelChoice], want: &str, runtime: &str) -> Result<()> {
    // An empty catalog is a harness that could not enumerate (opencode with no
    // provider reachable, say), not a harness with no models — nothing to
    // check the override against.
    if models.is_empty() || models.iter().any(|m| m.id.eq_ignore_ascii_case(want)) {
        return Ok(());
    }
    bail!(
        "`{runtime}` has no model `{want}`; its catalog: {}",
        models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub async fn dispatch(board: &Board, task_id: &str, opts: DispatchOpts<'_>) -> Result<Dispatched> {
    let params = board.params(dispatch_params(task_id, opts));
    let reply = board.client.call(methods::DISPATCH_TASK, params).await?;
    serde_json::from_value(reply).context("parsing DispatchTask reply")
}

/// The `DispatchTask` params for a release. Only the overrides that were given
/// are sent: an absent key is "the route's", which is not the same statement as
/// a null, and `replace` is absent unless it is true so an ordinary dispatch
/// sends what it always sent.
fn dispatch_params(task_id: &str, opts: DispatchOpts<'_>) -> serde_json::Value {
    let mut params = serde_json::json!({ "taskId": task_id, "via": opts.via });
    let Some(object) = params.as_object_mut() else {
        return params;
    };
    for (key, value) in [
        ("runtime", opts.runtime),
        ("model", opts.model),
        ("account", opts.account),
        ("bill", opts.bill),
        ("viaUser", opts.via_user),
        ("onto", opts.onto),
        ("base", opts.base),
    ] {
        if let Some(value) = value {
            object.insert(key.into(), serde_json::Value::String(value.to_string()));
        }
    }
    if opts.replace {
        object.insert("replace".into(), serde_json::Value::Bool(true));
    }
    if opts.stack {
        object.insert("stack".into(), serde_json::Value::Bool(true));
    }
    if opts.decompose {
        object.insert("decompose".into(), serde_json::Value::Bool(true));
    }
    params
}

pub async fn cancel(board: &Board, task_id: &str) -> Result<()> {
    board
        .client
        .call(
            methods::CANCEL_TASK,
            board.params(serde_json::json!({ "taskId": task_id })),
        )
        .await?;
    Ok(())
}

/// Merge the task's pull request (gh#408) — the confirmed keypress, executed
/// where the board's store and GitHub credential are. The answer is the
/// engine's one sentence about what actually happened: `o/r#87 merged`,
/// `o/r#87 is in the merge queue`, `o/r#87 is still merging`.
pub async fn merge(board: &Board, task_id: &str) -> Result<String> {
    let reply = board
        .client
        .call(
            methods::MERGE_TASK,
            board.params(serde_json::json!({ "taskId": task_id })),
        )
        .await?;
    reply
        .get("line")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("the engine's merge reply carried no line: {reply}"))
}

// ---- wait ---------------------------------------------------------------

/// The states that count as settled for one `wait` call.
///
/// The default trio is "finished, one way or another": work to look at, work
/// that broke, or work whose ticket closed under it. `blocked` is deliberately
/// not among them — an agent pausing for an approval mid-run is not a result,
/// and an orchestrator that waited on it would be woken by every permission
/// prompt.
///
/// But a child that asks a question and is never answered never settles either,
/// and `wait` would then hold until its timeout or forever (gh#73). So
/// `--blocked-is-settled` *adds* blocked to whichever set is in play rather
/// than replacing it: the caller wants to be called back when its child needs
/// an answer AND when it finishes, and spelling the whole set out by hand to
/// say that is the kind of thing nobody does twice.
pub fn settle_states(explicit: &[String], blocked_is_settled: bool) -> Vec<String> {
    let mut states = if explicit.is_empty() {
        vec![
            BoardState::Review.as_str().to_string(),
            BoardState::Failed.as_str().to_string(),
            BoardState::Done.as_str().to_string(),
        ]
    } else {
        explicit.to_vec()
    };
    let blocked = BoardState::Blocked.as_str();
    if blocked_is_settled && !states.iter().any(|s| s == blocked) {
        states.push(blocked.to_string());
    }
    states
}

/// Block until watched work settles — the counterpart to `dispatch`, so an
/// orchestrator can release work and be told, instead of polling or falling
/// silent until a human prods it.
///
/// herdr-board's `wait` was a poll loop that reconciled as it went; here it is
/// a `WatchBoard` subscription, which is the same promise kept better — the
/// engine pushes rows after every sync cycle, status refresh and command, so
/// this answers as soon as the answer is true without doing any work itself.
pub async fn wait_for(
    board: &Board,
    tasks: &[String],
    states: &[String],
    timeout: Option<Duration>,
) -> Result<Vec<TaskRow>> {
    for state in states {
        if BoardState::parse(state).is_none() {
            bail!(
                "unknown state `{state}`; expected one of: {}",
                state_names().join(", ")
            );
        }
    }
    let started = tokio::time::Instant::now();
    let deadline = timeout.map(|t| started + t);

    let mut stream = board
        .client
        .subscribe(methods::WATCH_BOARD, board.params(serde_json::json!({})))
        .await?;
    let mut rows = snapshot(&mut stream, board.host()).await?;

    // With no explicit tasks, watch whatever is in flight right now. Resolved
    // once, at the start: a task dispatched later is not what this call is
    // waiting for.
    let watching: Vec<String> = if tasks.is_empty() {
        rows.iter()
            .filter(|r| in_flight(r))
            .map(|r| r.id.clone())
            .collect()
    } else {
        tasks.to_vec()
    };
    if watching.is_empty() {
        // Distinct from "nothing matched": there was never anything to wait
        // for, which usually means the caller dispatched nothing, or the work
        // had already settled before it asked.
        bail!("nothing is in flight to wait for");
    }

    loop {
        let matched = settled(&rows, &watching, states);
        if !matched.is_empty() {
            return Ok(matched);
        }
        let next = match deadline {
            Some(d) => tokio::time::timeout_at(d, stream.recv())
                .await
                .map_err(|_| {
                    anyhow!(
                        "timed out after {:?} waiting for {} task(s) to reach {states:?}",
                        started.elapsed(),
                        watching.len()
                    )
                })?,
            None => stream.recv().await,
        };
        rows = match next {
            Some(v) => serde_json::from_value(v).context("parsing board rows")?,
            None => bail!("the board stream ended while waiting — did the engine stop?"),
        };
    }
}

/// A row with a live attempt. `chat_id` is set exactly when a live attempt has
/// its chat; `working`/`blocked` cover the window where the attempt exists but
/// its status has not folded into the row yet.
fn in_flight(r: &TaskRow) -> bool {
    r.chat_id.is_some() || matches!(r.state.as_str(), "working" | "blocked")
}

fn settled(rows: &[TaskRow], watching: &[String], states: &[String]) -> Vec<TaskRow> {
    rows.iter()
        .filter(|r| watching.contains(&r.id) && states.contains(&r.state))
        .cloned()
        .collect()
}

/// Wait for a task to appear on the board — `new --dispatch` needs the row to
/// exist before it can be released, and the engine's sync loop is what puts it
/// there on its next poll.
pub async fn await_row(board: &Board, task_id: &str, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut stream = board
        .client
        .subscribe(methods::WATCH_BOARD, board.params(serde_json::json!({})))
        .await?;
    let mut rows = snapshot(&mut stream, board.host()).await?;
    loop {
        if rows.iter().any(|r| r.id == task_id) {
            return Ok(());
        }
        let next = tokio::time::timeout_at(deadline, stream.recv())
            .await
            .map_err(|_| {
                anyhow!(
                    "{task_id} did not reach the board within {}s — it exists upstream; \
                     dispatch it once it appears: comet-board dispatch --task {task_id}",
                    timeout.as_secs()
                )
            })?;
        rows = match next {
            Some(v) => serde_json::from_value(v).context("parsing board rows")?,
            None => bail!("the board stream ended while waiting — did the engine stop?"),
        };
    }
}

// ---- routes (the routing.toml surface, gh#75) ---------------------------

/// What both config RPCs answer with: the host's `routing.toml`, and the repos
/// with a space on that device that nothing on the board watches.
#[derive(Debug, Serialize, Deserialize)]
pub struct BoardConfig {
    pub routing: RoutingView,
    #[serde(default)]
    pub unadopted: Vec<Unadopted>,
}

pub async fn read_config(board: &Board) -> Result<BoardConfig> {
    let reply = board
        .client
        .call(
            methods::READ_BOARD_CONFIG,
            board.params(serde_json::json!({})),
        )
        .await?;
    serde_json::from_value(reply).context("parsing ReadBoardConfig reply")
}

/// Send one edit. `op` is the tagged params documented on
/// [`methods::WRITE_BOARD_CONFIG`]; the reply is a fresh read, so what is
/// printed afterwards is the file as it now stands rather than what we hoped.
pub async fn write_config(board: &Board, op: serde_json::Value) -> Result<BoardConfig> {
    let reply = board
        .client
        .call(methods::WRITE_BOARD_CONFIG, board.params(op))
        .await?;
    serde_json::from_value(reply).context("parsing WriteBoardConfig reply")
}

/// Print the config: the routes with their numbers, what is wrong with it, and
/// what is sitting there unadopted.
///
/// The numbers are what `routes set` takes, and they are 1-based here and
/// 0-based on the wire — a person counting routes in a file starts at one, and
/// the conversion happens once, at the edge.
pub fn print_config(cfg: &BoardConfig, host: Option<&str>, json: bool) -> Result<()> {
    if json {
        // The whole reply, not just the routing half: an agent asking what is
        // configured wants the same answer about what is *not* — that is the
        // list it would act on.
        println!("{}", serde_json::to_string_pretty(cfg)?);
        return Ok(());
    }
    let where_ = match host {
        Some(h) => format!("{} on {h}", cfg.routing.path),
        None => cfg.routing.path.clone(),
    };
    println!("{where_}");
    if !cfg.routing.exists {
        println!("no routing.toml yet — `comet-board init` writes the first one");
    }

    match &cfg.routing.config {
        None => {}
        Some(c) => {
            if c.routes.is_empty() {
                println!("\nno routes — every row on the board reads `no route`");
            }
            for (i, r) in c.routes.iter().enumerate() {
                println!(
                    "\n{:>2}  {}  ·  {}",
                    i + 1,
                    r.display_name(),
                    match_summary(&r.match_)
                );
                let cap = cap_summary(r, &c.defaults.max_duration);
                let mut meta = vec![
                    format!("space {}", r.workspace),
                    format!("repo {}", r.repo),
                    format!("runtime {}", r.runtime),
                    format!("cap {cap}"),
                ];
                if let Some(a) = &r.account {
                    meta.push(format!("account {a}"));
                }
                // Only where the route disagrees with the board (gh#101) — the
                // mode itself is `doctor`'s line, and repeating it on every
                // route would bury the one route that answers differently.
                if r.billing_guard.is_some() {
                    meta.push(format!(
                        "billing_guard {}",
                        c.billing_guard(Some(r)).as_str()
                    ));
                }
                if let Some(n) = r.max_concurrent {
                    meta.push(format!("max_concurrent {n}"));
                }
                // Same rule for the instruction file (gh#272): named on the
                // route that answers differently from the board, and nowhere
                // else. `doctor` says what the board itself does.
                if r.agent_instructions.is_some() {
                    meta.push(format!(
                        "agent_instructions {}",
                        c.agent_instructions(Some(r))
                    ));
                }
                println!("    {}", meta.join(" · "));
            }
            if !c.github.repos.is_empty() {
                println!("\npolled repos: {}", c.github.repos.join(", "));
            }
        }
    }

    if !cfg.routing.problems.is_empty() {
        // Loudly, and last-but-one: this is the config the board is NOT
        // running on, and a reader who scrolled past the routes has to see why
        // they are not in force.
        println!(
            "\n{} problem(s) — the board is running on the last config that loaded:",
            cfg.routing.problems.len()
        );
        for p in &cfg.routing.problems {
            println!("  ✕ {p}");
        }
    }
    if !cfg.unadopted.is_empty() {
        println!("\nnot on the board yet:");
        for u in &cfg.unadopted {
            println!("  {:<40} {}{}", u.slug, u.label, u.missing.note());
        }
        println!("  add one:  comet-board routes add <owner/repo>");
    }
    Ok(())
}

// ---- onboard (gh#97) ----------------------------------------------------

/// Clone, space, adopt — one round trip, all of it on the board's device.
///
/// Nothing here is done locally, deliberately: the checkout has to be where the
/// agents run, the space has to be owned by that device, and the GitHub
/// credential that decides whether the repo is reachable lives with the board.
/// A laptop driving this needs no credential and no shell on the box.
pub async fn onboard(
    board: &Board,
    slug: &str,
    dir: Option<&str>,
    labels: Option<&[String]>,
    force: bool,
) -> Result<Onboarded> {
    // `force` only ever sent when it was asked for: the default shape is the
    // one every board that predates gh#343 already understands.
    let mut params = serde_json::json!({ "slug": slug });
    if let Some(object) = params.as_object_mut() {
        if let Some(dir) = dir {
            object.insert("dir".into(), serde_json::json!(dir));
        }
        if force {
            object.insert("force".into(), serde_json::json!(true));
        }
        // Absent and `[]` are different instructions — see `label_filter`.
        if let Some(labels) = labels {
            object.insert("labels".into(), serde_json::json!(labels));
        }
    }
    let reply = board
        .client
        .call(methods::ONBOARD_REPO, board.params(params))
        .await?;
    serde_json::from_value(reply).context("parsing OnboardRepo reply")
}

/// What the board's GitHub App can see, and which of it is already on the board.
pub async fn app_repos(board: &Board) -> Result<Vec<Candidate>> {
    let reply = board
        .client
        .call(methods::LIST_APP_REPOS, board.params(serde_json::json!({})))
        .await?;
    serde_json::from_value(reply).context("parsing ListAppRepos reply")
}

/// The onboarding offer, as `comet-board onboard` with no repo prints it.
pub fn print_candidates(repos: &[Candidate], host: Option<&str>, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(repos)?);
        return Ok(());
    }
    let where_ = match host {
        Some(h) => format!(" on {h}"),
        None => String::new(),
    };
    if repos.is_empty() {
        println!("the board's GitHub App is installed nowhere{where_}");
        return Ok(());
    }
    println!("repos the board's App can see{where_}:");
    for r in repos {
        // The ones already on the board are shown, not hidden: "is this one set
        // up?" is half of why anybody runs this.
        println!("  {:<44} {}", r.slug, r.note());
    }
    println!(
        "\nput one on the board:  comet-board onboard <owner/repo> [--dir <path>] [--labels a,b | --all-issues]"
    );
    Ok(())
}

/// What one onboard did, step by step.
///
/// Per step rather than as one line, because re-running after a half-finished
/// first go is the intended repair and the operator has to see which half was
/// missing to believe it is now whole.
pub fn print_onboarded(done: &Onboarded, host: Option<&str>, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(done)?);
        return Ok(());
    }
    println!(
        "onboarded {} on {}",
        done.slug,
        host.unwrap_or("this device")
    );
    println!("  clone    {:<8} {}", done.clone.as_str(), done.path);
    println!("  space    {:<8} {}", done.space.as_str(), done.space_name);
    match &done.adopted {
        None => println!("  routing  unchanged  already polled and routed"),
        Some(a) => {
            let mut wrote = Vec::new();
            if a.wrote_route {
                wrote.push("a [[route]]".to_string());
            }
            if a.wrote_repo {
                wrote.push("[github] repos".to_string());
            }
            if let Some(l) = &a.labels {
                wrote.push(if l.is_empty() {
                    "a [[github.repo]] polling every open issue".to_string()
                } else {
                    format!("a [[github.repo]] filter: {}", l.join(", "))
                });
            }
            println!("  routing  wrote    {}", wrote.join(" + "));
            println!(
                "           a commented `label = \"{}\"` route was left for Linear issues",
                a.suggested_label
            );
        }
    }
    if let Some(p) = &done.preview {
        println!("  issues   {}", p.count_phrase());
        for (label, n) in p.labels.iter().take(8) {
            println!("             {n:>4}  {label}");
        }
        // The failure this warns about is real and quiet: pointing the board at
        // one repo put 83 rows on it in a single poll.
        if done.adopted.as_ref().is_some_and(|a| a.labels.is_none()) && p.open_issues > 20 {
            println!(
                "  note     the global [github] labels filter applies — {} would arrive \
                 unfiltered; narrow it with `comet-board routes edit`",
                p.count_phrase()
            );
        }
    }
    for note in done.notes() {
        println!("  note     {note}");
    }
    println!("\nthe board polls on its next cycle: comet-board list --state ready");
    Ok(())
}

/// Report what a write landed: the problems if any, else a one-liner.
pub fn print_write_result(cfg: &BoardConfig) {
    if cfg.routing.problems.is_empty() {
        println!("routing.toml updated (previous contents in routing.toml.bak)");
    } else {
        // A write only lands if it validates, so problems here are ones the
        // file already had — worth saying, and not the same as "your edit
        // broke it", which comes back as an error instead.
        println!(
            "routing.toml updated, but {} pre-existing problem(s) remain:",
            cfg.routing.problems.len()
        );
        for p in &cfg.routing.problems {
            println!("  ✕ {p}");
        }
    }
}

// ---- members (the `[users]` map beside the slots, gh#162) ---------------

/// The board host's saved agent-account logins, or `None` when the engine
/// could not be asked.
///
/// Forwardable like the board calls, so `--device` asks the *box* which logins
/// it has — which is the only device whose answer means anything here, since
/// those are the subscriptions the dispatches spend.
///
/// `None` rather than an empty list on failure, deliberately: "this box has no
/// slots" and "nobody could be asked" are different answers, and rendering the
/// second as the first would tell every mapped teammate they need an account
/// they may already have (gh#155).
pub async fn agent_accounts(board: &Board) -> Option<Vec<comet_proto::AgentAccount>> {
    // Offline list, as doctor's is: the ids and who they belong to, not a
    // round of rate-limit probes against everybody's plan.
    let reply = board
        .client
        .call(
            methods::LIST_AGENT_ACCOUNTS,
            board.params(serde_json::json!({})),
        )
        .await
        .ok()?;
    serde_json::from_value::<comet_proto::AgentAccountsSnapshot>(reply)
        .ok()
        .map(|s| s.accounts)
}

/// One slot, as every line that names one spells it.
fn slot_line(s: &Slot) -> String {
    let who = s.email.as_deref().unwrap_or("login unreadable");
    format!(
        "{} · {} · {who}{}",
        s.id,
        runtime_name(s.harness),
        if s.active { " · active" } else { "" }
    )
}

/// The map, the slots, and the pairing between them.
pub fn print_roster(roster: &Roster, host: Option<&str>, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(roster)?);
        return Ok(());
    }
    if let Some(h) = host {
        println!("the board on {h}");
    }
    if roster.members.is_empty() {
        println!(
            "no `[users]` map — every dispatch commits under this box's own git identity, \
             whoever released it"
        );
    }
    for m in &roster.members {
        match &m.author {
            Some(a) => println!("{}  →  {} <{}>", m.user, a.name, a.email),
            // The config error `problems()` already reports, said again where
            // somebody is looking at the map: this entry authors nothing.
            None => println!(
                "{}  →  \"{}\" is not an address — those dispatches commit as the box",
                m.user, m.value
            ),
        }
        if m.author.is_some() && !m.noreply {
            println!(
                "    not a GitHub noreply address — attribution depends on it being on \
                 that account, which nothing here can check"
            );
        }
        // The pairing gh#162 is about. Only when the slots are known: a box
        // that could not be asked must not accuse everybody of missing one.
        match (roster.accounts_known, m.accounts.as_slice()) {
            (_, [first, rest @ ..]) => {
                println!("    account {}", slot_line(first));
                for s in rest {
                    println!("            {}", slot_line(s));
                }
            }
            (true, []) => println!(
                "    no agent account — their dispatches spend whichever subscription the \
                 route names, or the box's own"
            ),
            (false, []) => println!("    agent account not checked — the engine did not answer"),
        }
    }
    if !roster.unmapped.is_empty() {
        // Not a fault: the box owner's own login lives here. It is the other
        // half of the same pairing, and the answer to "why did their commit
        // land as the box".
        println!("\nslots belonging to nobody in the map:");
        for s in &roster.unmapped {
            println!("  {}", slot_line(s));
        }
    }
    if !roster.accounts_known {
        println!(
            "\nthe engine could not be asked for this box's agent accounts, so no pairing \
             above is known — start `comet` or `comet headless`"
        );
    }
    println!(
        "\nadd one:  comet-board member add <email> --github <login>\
         \nthe rest: docs/teammate.md"
    );
    Ok(())
}

// ---- new ----------------------------------------------------------------

/// Where a new ticket should be written.
///
/// There is no inferring this. A label maps to a *route* — which repo the work
/// happens in — and says nothing about which tracker the project's tickets live
/// in. This board's own backlog is in Linear while its code is on GitHub, and a
/// repo whose issues you already keep on GitHub is the reverse, so the same
/// label would justify either answer. `[defaults] new_source` sets the habit;
/// `--source` overrides it.
#[derive(Debug, Default)]
pub struct NewTask<'a> {
    pub title: &'a str,
    pub body: Option<&'a str>,
    /// Linear team key. Only needed with more than one team.
    pub team: Option<&'a str>,
    pub labels: &'a [String],
    /// `linear` or `github`; falls back to `[defaults] new_source`.
    pub source: Option<&'a str>,
    /// `owner/repo`, for GitHub with several repos configured.
    pub repo: Option<&'a str>,
}

/// Write a ticket, so recording the work is cheaper than not recording it.
///
/// Work that goes through a ticket is traceable — reasoning, branch, PR,
/// review, closure. Work that does not is a wall of commits somebody has to
/// reconstruct later. The difference in practice is almost entirely friction,
/// so this exists to remove it.
pub fn new_task(
    paths: &Paths,
    cfg: &RoutingConfig,
    spec: &NewTask<'_>,
) -> Result<(String, String)> {
    let source = spec
        .source
        .map(str::to_string)
        .unwrap_or_else(|| cfg.defaults.new_source.clone());

    if source == "github" {
        let here = git_remote(".").as_deref().and_then(github_slug);
        let repo = github_repo(&cfg.github.repos, spec.repo, here)?;
        if matches!(config::github_auth(paths), config::GithubAuth::None) {
            bail!(
                "no GitHub credential — set GITHUB_TOKEN, or GITHUB_APP_ID and \
                 GITHUB_APP_PRIVATE_KEY_PATH; see `comet-board doctor`"
            );
        }
        let gh = comet_board::sources::github::Github::new(
            comet_board::sources::github::HttpRest::from_paths(paths)?,
        );
        let (number, url) = gh.create_issue(&repo, spec.title, spec.body, spec.labels)?;
        return Ok((format!("{repo}#{number}"), url));
    }
    if source != "linear" {
        bail!("unknown source `{source}`; expected linear or github");
    }

    let key = config::linear_api_key(paths)
        .ok_or_else(|| anyhow!("no LINEAR_API_KEY; see `comet-board doctor`"))?;
    let linear = comet_board::sources::linear::Linear::new(
        comet_board::sources::linear::HttpTransport::new(key)?,
    );

    let teams = linear.team_ids()?;
    let team_id = match spec.team {
        Some(k) => teams
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(k))
            .map(|(_, id)| id.clone())
            .ok_or_else(|| {
                anyhow!(
                    "no Linear team `{k}`; known: {}",
                    teams
                        .iter()
                        .map(|(key, _)| key.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?,
        // With one team there is nothing to choose; with several, say so rather
        // than filing into whichever came back first.
        None if teams.len() == 1 => teams[0].1.clone(),
        None => bail!(
            "several Linear teams exist; name one with --team: {}",
            teams
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    let known = linear.label_ids().unwrap_or_default();
    let mut ids = Vec::new();
    for want in spec.labels {
        match known
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(want))
        {
            Some((_, id)) => ids.push(id.clone()),
            None => bail!(
                "no Linear label `{want}`; known: {}",
                known
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    linear.create_issue(&team_id, spec.title, spec.body, &ids)
}

/// Which repo a GitHub ticket goes to, in order of explicitness: `--repo`, the
/// only configured repo, the checkout you are standing in.
///
/// Standing in a repo is a statement about which repo you mean — an agent
/// filing a ticket is almost always inside the checkout the ticket is about,
/// and making it pass `--repo` is asking it to repeat what the working
/// directory already says. But only a repo the board actually polls: filing
/// into an unpolled one writes a real issue that never reaches the board, which
/// is the failure this would otherwise cause silently and often.
fn github_repo(repos: &[String], flag: Option<&str>, here: Option<String>) -> Result<String> {
    flag.map(str::to_string)
        // One configured repo is not a choice; several are.
        .or_else(|| (repos.len() == 1).then(|| repos[0].clone()))
        .or_else(|| {
            let slug = here.clone()?;
            repos
                .iter()
                .find(|r| r.eq_ignore_ascii_case(&slug))
                .cloned()
        })
        .ok_or_else(|| match here {
            // In a GitHub repo, but not one the board watches. Naming the repo
            // would not help: the issue still would not show up. Adopting it is
            // the fix.
            Some(slug) => anyhow!(
                "{slug} is not polled by the board, so a ticket filed there would not \
                 appear — adopt it first (`comet-board adopt {slug}`), or name another \
                 with --repo; configured: {}",
                repos.join(", ")
            ),
            None => anyhow!(
                "name the repo with --repo; configured: {}",
                repos.join(", ")
            ),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, state: &str) -> TaskRow {
        TaskRow {
            automation: None,
            automation_owner: None,
            id: id.into(),
            identifier: id.into(),
            title: format!("task {id}"),
            state: state.into(),
            source: "github".into(),
            url: String::new(),
            labels: vec![],
            dispatchable: true,
            gone: false,
            route: None,
            workspace: None,
            runtime: None,
            chat_id: None,
            review_chat_id: None,
            pr_url: None,
            pr_number: None,
            pr_base_ref: None,
            pr_mergeable: None,
            changes_below: None,
            landing: None,
            stack: None,
            branch: None,
            dispatched_by: None,
            dispatched_by_chat: None,
            last_outcome: None,
            last_outcome_at: None,
            attempts: 0,
            reopened: 0,
            updated_at: "2026-08-01T11:00:00Z".into(),
            started_at: None,
            account: None,
            dispatched_by_user: None,
            dispatched_by_verified: false,
            billed_to: None,
            max_duration_secs: None,
            context: None,
        }
    }

    #[test]
    fn unknown_filters_are_rejected_not_emptied() {
        // `[]` from a typo would be indistinguishable from "nothing is ready".
        assert!(validate_filters(Some("redy"), None).is_err());
        assert!(validate_filters(None, Some("jira")).is_err());
        assert!(validate_filters(Some("ready"), Some("linear")).is_ok());
        assert!(validate_filters(None, None).is_ok());
    }

    #[test]
    fn filters_apply_by_state_and_source() {
        let mut linear_row = row("linear:AGE-1", "working");
        linear_row.source = "linear".into();
        let rows = vec![
            row("gh:o/r#1", "ready"),
            row("gh:o/r#2", "working"),
            linear_row,
        ];

        let ready = filter_rows(rows.clone(), Some("ready"), None);
        assert_eq!(
            ready.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["gh:o/r#1"]
        );

        let linear = filter_rows(rows, None, Some("linear"));
        assert_eq!(
            linear.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["linear:AGE-1"]
        );
    }

    #[test]
    fn in_flight_means_a_live_attempt_not_a_state_name() {
        let mut with_chat = row("a", "ready");
        with_chat.chat_id = Some("chat-1".into());
        assert!(
            in_flight(&with_chat),
            "a chat holds a live attempt whatever the derived state"
        );
        assert!(in_flight(&row("b", "working")));
        assert!(in_flight(&row("c", "blocked")));
        assert!(!in_flight(&row("d", "ready")));
        assert!(!in_flight(&row("e", "review")));
    }

    #[test]
    fn settled_matches_only_watched_tasks_in_wanted_states() {
        let rows = vec![row("a", "review"), row("b", "review"), row("c", "working")];
        let watching = vec!["a".to_string(), "c".into()];
        let states = vec!["review".to_string(), "failed".into(), "done".into()];
        let matched = settled(&rows, &watching, &states);
        assert_eq!(
            matched.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["a"]
        );
    }

    #[test]
    fn provenance_prefers_the_flag_and_ignores_empties() {
        assert_eq!(
            provenance_from(Some("chat-flag".into()), Some("chat-env".into())).as_deref(),
            Some("chat-flag")
        );
        assert_eq!(
            provenance_from(None, Some("chat-env".into())).as_deref(),
            Some("chat-env")
        );
        assert_eq!(provenance_from(None, Some(String::new())), None);
        assert_eq!(provenance_from(None, None), None);
    }

    fn runtimes() -> Vec<RuntimeOption> {
        comet_board::runtime::runtime_options()
    }

    fn models(ids: &[&str]) -> Vec<ModelChoice> {
        ids.iter()
            .map(|id| ModelChoice { id: (*id).into() })
            .collect()
    }

    #[test]
    fn runtime_override_resolves_aliases_and_refuses_the_rest() {
        // Canonical, as the picker offers it.
        assert_eq!(
            resolve_runtime(&runtimes(), "opencode").unwrap(),
            "opencode"
        );
        // `routing.toml` spellings the engine accepts but the catalog does not
        // list twice — refusing these would refuse what a route already says.
        assert_eq!(
            resolve_runtime(&runtimes(), "claude").unwrap(),
            "claude-code"
        );
        assert_eq!(
            resolve_runtime(&runtimes(), "openai-codex").unwrap(),
            "codex"
        );

        let err = resolve_runtime(&runtimes(), "claude-cod")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown runtime `claude-cod`"), "{err}");
        // The error has to carry the answer; there is no picker to fall back on.
        assert!(err.contains("claude-code"), "{err}");
    }

    /// A runtime the host lists but cannot start is refused here, before the
    /// dispatch that would have cut a worktree to find out (gh#187) — and the
    /// error says which of the two is wrong, plus what the box *can* run.
    #[test]
    fn a_runtime_the_host_cannot_start_is_refused_with_the_reason() {
        use comet_proto::view::board::RuntimeUnavailable;
        let mut options = runtimes();
        for option in &mut options {
            option.unavailable = match option.name.as_str() {
                "opencode" => Some(RuntimeUnavailable::NotInstalled),
                "codex" => Some(RuntimeUnavailable::SignedOut),
                _ => None,
            };
        }

        let err = resolve_runtime(&options, "opencode")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not installed"), "{err}");
        assert!(err.contains("claude-code"), "names what does work: {err}");

        // The other axis, named apart: this one is a login, not an install.
        let err = resolve_runtime(&options, "openai-codex")
            .unwrap_err()
            .to_string();
        assert!(err.contains("signed out"), "{err}");

        // And an available one still resolves, alias and all.
        assert_eq!(resolve_runtime(&options, "claude").unwrap(), "claude-code");
    }

    #[test]
    fn model_override_is_checked_against_the_catalog() {
        let catalog = models(&["claude-opus-5", "claude-sonnet-5"]);
        assert!(check_model(&catalog, "claude-opus-5", "claude-code").is_ok());

        let err = check_model(&catalog, "opus", "claude-code")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no model `opus`"), "{err}");
        assert!(err.contains("claude-opus-5"), "{err}");

        // A harness that could not enumerate proves nothing — the override
        // stands and the harness stays the authority.
        assert!(check_model(&[], "anything", "opencode").is_ok());
    }

    #[test]
    fn github_repo_resolution_in_order_of_explicitness() {
        let repos = vec!["owner/one".to_string(), "owner/two".into()];
        // The flag wins.
        assert_eq!(
            github_repo(&repos, Some("owner/two"), Some("owner/one".into())).unwrap(),
            "owner/two"
        );
        // One configured repo is not a choice.
        assert_eq!(
            github_repo(&["owner/only".to_string()], None, None).unwrap(),
            "owner/only"
        );
        // The checkout you are standing in, but only if the board polls it.
        assert_eq!(
            github_repo(&repos, None, Some("Owner/One".into())).unwrap(),
            "owner/one"
        );
        let err = github_repo(&repos, None, Some("owner/unpolled".into())).unwrap_err();
        assert!(err.to_string().contains("adopt"), "{err}");
        assert!(github_repo(&repos, None, None).is_err());
    }

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "linux".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        }
    }

    #[test]
    fn device_resolves_by_id_then_name() {
        let devices = vec![device("dev-1", "the-box"), device("dev-2", "laptop")];
        assert_eq!(pick_device(&devices, "dev-2").unwrap(), "dev-2");
        // The name people actually have, spelled how they feel like spelling it.
        assert_eq!(pick_device(&devices, "The-Box").unwrap(), "dev-1");

        let err = pick_device(&devices, "boxx").unwrap_err().to_string();
        assert!(err.contains("no device `boxx`"), "{err}");
        // The error carries the fleet; there is no picker to fall back on.
        assert!(err.contains("the-box (dev-1)"), "{err}");
    }

    #[test]
    fn an_ambiguous_device_name_asks_for_the_id() {
        // Two laptops called `laptop` is a real fleet. Picking one would send
        // the work to a device the operator did not choose.
        let devices = vec![device("dev-1", "laptop"), device("dev-2", "laptop")];
        let err = pick_device(&devices, "laptop").unwrap_err().to_string();
        assert!(err.contains("names 2 devices"), "{err}");
        assert!(err.contains("dev-1, dev-2"), "{err}");

        // An id is never ambiguous, even when the names collide.
        assert_eq!(pick_device(&devices, "dev-2").unwrap(), "dev-2");
    }

    #[test]
    fn the_host_rides_on_board_calls_and_a_local_board_sends_nothing_extra() {
        assert_eq!(
            with_target(serde_json::json!({ "taskId": "t" }), Some("dev-1")),
            serde_json::json!({ "taskId": "t", "targetDeviceId": "dev-1" })
        );
        assert_eq!(
            with_target(serde_json::json!({ "taskId": "t" }), None),
            serde_json::json!({ "taskId": "t" })
        );
    }

    #[test]
    fn dispatch_sends_only_the_overrides_it_was_given() {
        // An absent key is "the route's", which is not the same statement as a
        // null — and `replace` absent is what an ordinary dispatch has always
        // sent.
        assert_eq!(
            dispatch_params("gh:o/r#1", DispatchOpts::default()),
            serde_json::json!({ "taskId": "gh:o/r#1", "via": null })
        );
        assert_eq!(
            dispatch_params(
                "gh:o/r#1",
                DispatchOpts {
                    via: Some("chat-7"),
                    runtime: Some("opencode"),
                    model: Some("claude-opus-5"),
                    account: Some("slot-a"),
                    bill: None,
                    via_user: None,
                    onto: None,
                    base: None,
                    replace: true,
                    stack: false,
                    decompose: false,
                }
            ),
            serde_json::json!({
                "taskId": "gh:o/r#1",
                "via": "chat-7",
                "runtime": "opencode",
                "model": "claude-opus-5",
                "account": "slot-a",
                "replace": true,
            })
        );
        // gh#287: asked for, and only then — an engine that does not know the
        // key is sent nothing to ignore.
        assert_eq!(
            dispatch_params(
                "gh:o/r#1",
                DispatchOpts {
                    stack: true,
                    ..DispatchOpts::default()
                }
            ),
            serde_json::json!({ "taskId": "gh:o/r#1", "via": null, "stack": true })
        );
        // gh#340: the same rule for the other decomposition ask.
        assert_eq!(
            dispatch_params(
                "gh:o/r#1",
                DispatchOpts {
                    decompose: true,
                    ..DispatchOpts::default()
                }
            ),
            serde_json::json!({ "taskId": "gh:o/r#1", "via": null, "decompose": true })
        );
    }

    /// gh#101: the acknowledgement and this shell's own user ride along, and
    /// only when there is one. `viaUser` is the half of the billing comparison
    /// the CLI never sent before — without it the guard on the box has nothing
    /// to compare an account against, and `require-own` could never refuse a
    /// release from a shell.
    #[test]
    fn dispatch_carries_the_acknowledgement_and_who_is_signed_in() {
        assert_eq!(
            dispatch_params(
                "gh:o/r#1",
                DispatchOpts {
                    bill: Some("brede@tally.no"),
                    via_user: Some("ana@example.com"),
                    ..Default::default()
                }
            ),
            serde_json::json!({
                "taskId": "gh:o/r#1",
                "via": null,
                "bill": "brede@tally.no",
                "viaUser": "ana@example.com",
            })
        );
        // A shell nobody is signed into sends neither, exactly as before.
        let bare = dispatch_params("gh:o/r#1", DispatchOpts::default());
        assert!(bare.get("viaUser").is_none());
        assert!(bare.get("bill").is_none());
    }

    /// `--bill <slot>` is consent *and* a choice of account; `--bill <email>`
    /// only ever the first, because there is no slot id to select when the
    /// login being spent is the box's own.
    #[test]
    fn bill_selects_a_slot_but_an_email_only_acknowledges() {
        assert!(comet_board::billing::bill_names_a_slot("8f2c1d0a7b6e4539"));
        assert!(!comet_board::billing::bill_names_a_slot("brede@tally.no"));
    }

    #[test]
    fn retry_replaces_only_a_blocked_rows_live_attempt() {
        // Blocked: the agent is alive and waiting, so the one-live-attempt rule
        // would refuse a plain dispatch — ending it is the point.
        assert!(retry_replaces("blocked"));
        // Failed and ready hold nothing live; replacing nothing would be a lie
        // in the log, and the engine's own guard is free to refuse the rest.
        assert!(!retry_replaces("failed"));
        assert!(!retry_replaces("ready"));
        assert!(!retry_replaces("working"));
        assert!(!retry_replaces("review"));
        assert!(!retry_replaces("done"));
    }

    #[test]
    fn blocked_is_settled_adds_to_the_states_rather_than_replacing_them() {
        assert_eq!(settle_states(&[], false), ["review", "failed", "done"]);
        // The orchestrator wants to hear about a question AND about the finish.
        assert_eq!(
            settle_states(&[], true),
            ["review", "failed", "done", "blocked"]
        );
        // An explicit set is still the caller's; the flag only tops it up.
        assert_eq!(
            settle_states(&["done".to_string()], true),
            ["done", "blocked"]
        );
        // And it never doubles a state the caller already named.
        assert_eq!(settle_states(&["blocked".to_string()], true), ["blocked"]);
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("short", 48), "short");
        assert_eq!(truncate("ålesund", 4), "åle…");
    }

    // ---- the review, as a reader sees it (§gh#183) ----------------------

    use comet_board::claims::{Brief, ChangedFile, ClaimView, DiffSource, Remainder};
    use comet_board::evidence::Check;

    fn changed(path: &str, status: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            status: status.into(),
            added: 12,
            removed: 3,
            binary: false,
            symbols: Vec::new(),
        }
    }

    fn review_of(remainder: Remainder, changed_files: Vec<ChangedFile>) -> AttemptReview {
        AttemptReview {
            automation: None,
            automation_owner: None,
            task_id: "gh:o/r#183".into(),
            attempt: 7,
            attempt_number: 2,
            state: "review".into(),
            outcome: Some("done".into()),
            branch: Some("board/gh-183".into()),
            worktree: Some("/wt/gh-183".into()),
            pr_url: Some("https://github.com/o/r/pull/210".into()),
            pr_base_ref: None,
            pr_mergeable: None,
            changes_below: None,
            stack: None,
            brief: Brief {
                identifier: "gh#183".into(),
                title: "review backend".into(),
                url: "https://github.com/o/r/issues/183".into(),
                body: None,
            },
            claimed_at: Some("2026-08-09T10:00:00Z".into()),
            claims_error: None,
            sandbox: None,
            remainder,
            changed: changed_files,
            diff: DiffSource::Checkout,
            uncommitted: Some(0),
            evidence: RunEvidence::default(),
            effects: comet_board::effects::Effects::default(),
        }
    }

    /// The remainder is the thing a reader is left holding, so it is last and
    /// it names every file.
    #[test]
    fn the_unclaimed_set_is_the_last_thing_the_review_says() {
        let text = render_review(&review_of(
            Remainder {
                claims: vec![ClaimView {
                    text: "Storage".into(),
                    files: vec!["src/db.rs".into()],
                    symbols: vec![],
                    matched: vec!["src/db.rs".into()],
                    unmatched: vec![],
                    call_sites: vec![],
                }],
                unclaimed: vec![changed("Cargo.lock", "A")],
                unmatched_anchors: vec![],
                claimed: 1,
            },
            vec![changed("src/db.rs", "M"), changed("Cargo.lock", "A")],
        ));
        assert!(text.contains("gh#183 · review backend"), "{text}");
        assert!(text.contains("attempt 7 · board/gh-183 · done"), "{text}");
        assert!(
            text.contains("CLAIMS (1, submitted 2026-08-09T10:00:00Z)"),
            "{text}"
        );
        assert!(
            text.contains("UNCLAIMED (1 of 2 changed file(s))"),
            "{text}"
        );
        assert!(text.contains("Cargo.lock"), "{text}");
        let (before, after) = text.split_once("UNCLAIMED").expect("a remainder section");
        assert!(before.contains("CLAIMS"), "the claims come first");
        assert!(!after.contains("CLAIMS"), "and the remainder is last");
    }

    /// Two silences a review must never render as calm: an attempt that never
    /// claimed anything, and a run that never checked anything.
    #[test]
    fn an_unanswered_contract_and_an_unchecked_run_both_say_so() {
        let mut review = review_of(
            Remainder {
                unclaimed: vec![changed("src/db.rs", "M")],
                ..Default::default()
            },
            vec![changed("src/db.rs", "M")],
        );
        review.claimed_at = None;
        review.evidence = RunEvidence {
            commands: 214,
            failed: 11,
            checks: vec![],
            truncated: false,
        };
        let text = render_review(&review);
        assert!(text.contains("never answered the contract"), "{text}");
        assert!(
            text.contains("214 command(s) ran, 11 exited non-zero"),
            "{text}"
        );
        assert!(text.contains("nothing that checks anything ran"), "{text}");
    }

    /// A check that never passed is marked, and a claim that anchors to nothing
    /// is marked the same way — both are things the reader has to look at.
    #[test]
    fn failures_and_unanchored_claims_are_both_flagged() {
        let mut review = review_of(
            Remainder {
                claims: vec![ClaimView {
                    text: "Fixed the retry path".into(),
                    files: vec!["src/retry.rs".into()],
                    symbols: vec![],
                    matched: vec![],
                    unmatched: vec!["src/retry.rs".into()],
                    call_sites: vec![],
                }],
                unmatched_anchors: vec!["src/retry.rs".into()],
                ..Default::default()
            },
            vec![],
        );
        review.evidence = RunEvidence {
            commands: 9,
            failed: 2,
            checks: vec![Check {
                command: "cargo test".into(),
                runs: 2,
                failed: 2,
            }],
            truncated: false,
        };
        let text = render_review(&review);
        assert!(text.contains("! Fixed the retry path"), "{text}");
        assert!(text.contains("(unchanged: src/retry.rs)"), "{text}");
        assert!(text.contains("! cargo test  ×2, 2 failed"), "{text}");

        // …and the claim receipt says the same thing, first.
        let receipt = render_claim_result(&review);
        assert!(
            receipt.starts_with("recorded 1 claim(s) on gh#183 attempt 7"),
            "{receipt}"
        );
        assert!(
            receipt.contains("! nothing in the diff matches: Fixed the retry path"),
            "{receipt}"
        );
    }

    /// The effects row (§gh#236): above the claims, because a reader who has
    /// been told a fluent story reads numbers underneath it as confirmation.
    #[test]
    fn the_effects_the_board_derived_come_before_anything_the_agent_said() {
        use comet_board::effects::{Effects, FileScan};
        let mut review = review_of(
            Remainder {
                claims: vec![ClaimView {
                    text: "The scan and its test".into(),
                    files: vec!["src/effects.rs".into()],
                    symbols: vec![],
                    matched: vec!["src/effects.rs".into()],
                    unmatched: vec![],
                    call_sites: vec![],
                }],
                claimed: 1,
                ..Default::default()
            },
            vec![changed("src/effects.rs", "A")],
        );
        review.effects = Effects {
            read: true,
            files: vec![FileScan {
                path: "src/effects.rs".into(),
                kind: Some(comet_board::effects::FileKind::Rust),
                tests_added: 4,
                ..Default::default()
            }],
            tests_before: Some(41),
            tests_after: Some(47),
            deps_added: vec!["toml".into()],
            deps_known: true,
        };
        review.evidence = RunEvidence {
            commands: 12,
            failed: 1,
            checks: vec![Check {
                command: "cargo test".into(),
                runs: 2,
                failed: 1,
            }],
            truncated: false,
        };
        let text = render_review(&review);
        assert!(text.contains("Tests 41 → 47, all passing"), "{text}");
        assert!(text.contains("* 1 dependency added"), "{text}");
        assert!(text.contains("· Public API unchanged"), "{text}");
        // The claim carries its own evidence, and a tick it had to earn.
        assert!(text.contains("✓ The scan and its test"), "{text}");
        assert!(text.contains("✓ 4 new tests pass"), "{text}");
        let (before, after) = text.split_once("CLAIMS").expect("a claims section");
        assert!(before.contains("EFFECTS"), "the effects come first");
        assert!(!after.contains("EFFECTS"));
    }

    /// The exit condition, in the terminal: a review the board never read says
    /// so, instead of printing five clean results.
    #[test]
    fn a_review_with_no_effects_read_says_that_rather_than_reassuring_anybody() {
        let text = render_review(&review_of(Remainder::default(), vec![]));
        assert!(
            text.contains("? no effects read from this branch"),
            "{text}"
        );
        assert!(!text.contains("Public API unchanged"), "{text}");
    }

    /// A layer of a stack says so, in the terminal too (§gh#389).
    ///
    /// The header block is where a reader learns what they are looking at, and
    /// "an ordinary pull request" is the wrong answer for a layer: the map says
    /// which siblings exist, the order says which way the chain merges, and the
    /// landing note refuses to repeat GitHub's `clean` unqualified.
    #[test]
    fn a_stacked_review_says_which_layer_it_is_and_which_way_the_chain_merges() {
        use comet_proto::view::board::{RowStack, StackLayer};
        let layer = |n: i64, position: i64| StackLayer {
            id: format!("gh:o/r!{n}"),
            identifier: format!("gh!{n}"),
            pr_number: Some(n),
            position: Some(position),
            open: true,
            mergeable: Some("clean".into()),
            changes_requested: false,
        };
        let mut review = review_of(Remainder::default(), vec![]);
        review.task_id = "gh:o/r!48".into();
        review.pr_url = Some("https://github.com/o/r/pull/48".into());
        review.pr_base_ref = Some("board/gh-44-packages".into());
        review.pr_mergeable = Some("clean".into());
        review.stack = Some(RowStack {
            number: 49,
            position: Some(2),
            size: Some(3),
            base_ref: Some("main".into()),
            layers: vec![layer(47, 1), layer(48, 2), layer(50, 3)],
        });

        let text = render_review(&review);
        assert!(
            text.contains("stack 2 of 3 · onto board/gh-44-packages · lands on main"),
            "{text}"
        );
        assert!(text.contains("#47 ↑ #48 ↑ #50"), "{text}");
        assert!(
            text.contains("bottom-up: #47 lands before this one, #50 after"),
            "{text}"
        );
        assert!(
            text.contains("ready to land with 1 below"),
            "the AND across the layers below, never a bare `clean`: {text}"
        );

        // The same pull request with a stuck layer under it: GitHub still says
        // `clean` about this one, and the terminal still refuses to.
        review.stack.as_mut().unwrap().layers[0].mergeable = Some("dirty".into());
        let text = render_review(&review);
        assert!(
            text.contains("clean against board/gh-44-packages · waiting on PR #47"),
            "{text}"
        );
        assert!(!text.contains("ready to land"), "{text}");
    }

    /// A pull request that is not a layer of anything gains nothing: no map, no
    /// order, and the header block it always had.
    #[test]
    fn an_unstacked_review_prints_no_stack_at_all() {
        let text = render_review(&review_of(Remainder::default(), vec![]));
        assert!(!text.contains("stack "), "{text}");
        assert!(!text.contains("bottom-up"), "{text}");
        assert!(!text.contains("↑"), "{text}");
    }

    /// The two states that would otherwise read as "nothing changed".
    #[test]
    fn an_unreadable_diff_and_uncommitted_work_are_never_silent() {
        let mut review = review_of(Remainder::default(), vec![]);
        review.diff = DiffSource::Unavailable {
            reason: "the checkout was reclaimed (/wt/gh-183)".into(),
        };
        let text = render_review(&review);
        assert!(text.contains("UNCLAIMED — unknown"), "{text}");
        assert!(text.contains("reclaimed"), "{text}");

        let mut review = review_of(Remainder::default(), vec![]);
        review.uncommitted = Some(3);
        let text = render_review(&review);
        assert!(
            text.contains("all 0 changed file(s) are accounted for"),
            "{text}"
        );
        assert!(
            text.contains("3 file(s) changed in the checkout and not committed"),
            "an empty remainder over uncommitted work is the friendly lie: {text}"
        );
    }

    /// A pull request nobody dispatched (§gh#344), in the terminal: no attempt
    /// to name, no contract anybody was told, and a diff that came from GitHub.
    #[test]
    fn an_undispatched_pull_request_says_what_it_is_and_blames_nobody() {
        let mut review = review_of(
            Remainder {
                unclaimed: vec![changed("src/approval.rs", "M")],
                ..Remainder::default()
            },
            vec![changed("src/approval.rs", "M")],
        );
        review.attempt = comet_board::claims::NO_ATTEMPT;
        review.attempt_number = 0;
        review.outcome = None;
        review.claimed_at = None;
        review.uncommitted = None;
        review.diff = DiffSource::PullRequest;
        review.branch = Some("codex/restore-green-main".into());

        let text = render_review(&review);
        assert!(
            text.contains("no attempt · codex/restore-green-main · opened outside the board"),
            "{text}"
        );
        assert!(!text.contains("still running"), "nothing is running: {text}");
        assert!(
            text.contains("nothing dispatched this pull request"),
            "{text}"
        );
        assert!(
            !text.contains("never answered the contract"),
            "nobody was asked: {text}"
        );
        assert!(text.contains("UNCLAIMED (1 of 1"), "{text}");
        assert!(
            text.contains("from GitHub's file list for the pull request"),
            "{text}"
        );
    }

    /// What the agent was permitted to do, above the evidence it produced
    /// (§gh#349). A reviewer weighing "cargo test passed" is entitled to know
    /// whether the agent that ran it was confined to its own checkout.
    #[test]
    fn a_run_that_had_the_whole_box_says_so_above_its_own_evidence() {
        use comet_proto::{SandboxLevel, SandboxReport};

        // Nobody said: no line at all, rather than a reassuring one.
        let review = review_of(Remainder::default(), vec![]);
        let text = render_review(&review);
        assert!(!text.contains("full access"), "{text}");

        // Sandboxed as dispatched: still nothing to say.
        let mut review = review_of(Remainder::default(), vec![]);
        review.sandbox = Some(SandboxReport::as_requested(SandboxLevel::WorkspaceWrite));
        assert!(!render_review(&review).contains("full access"));

        // Widened out from under the dispatch: said, with the request named,
        // and above the EFFECTS block it qualifies.
        let mut review = review_of(Remainder::default(), vec![]);
        review.sandbox = Some(SandboxReport::widened(
            SandboxLevel::WorkspaceWrite,
            SandboxLevel::DangerFullAccess,
            "this codex predates the worktree-mount fix",
        ));
        let text = render_review(&review);
        assert!(
            text.contains("? this agent had full access to the box"),
            "{text}"
        );
        assert!(text.contains("workspace-write was requested"), "{text}");
        assert!(
            text.find("full access").unwrap() < text.find("EFFECTS").unwrap(),
            "the caveat comes before what it qualifies: {text}"
        );
    }
}
