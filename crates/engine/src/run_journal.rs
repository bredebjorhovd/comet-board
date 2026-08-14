//! Per-session on-disk event journal (port of comet's `run-journal.ts`, JSONL-shaped).
//!
//! One append-only JSONL file per chat under `{data_dir}/journals/{chat_id}.jsonl`; each
//! line is `{"seq": n, "event": AgentEvent}` with a monotonically increasing `seq`. The
//! journal is the durable replay source for live streams (`Subscribe` = replay then tail
//! the broadcast hub) and the crash-recovery gauge: a journal whose LAST event is not
//! `Done` belongs to a run that died mid-stream — boot recovery stamps its doc entry
//! `aborted` and closes the journal with a synthetic `Done`.
//!
//! Bounded-window compaction is deferred (whole file kept for now, per M2 scope); a torn
//! trailing line from a crash mid-write is tolerated everywhere.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use comet_board::evidence::RanCommand;
use comet_proto::{AgentEvent, ContextUsage, SandboxReport, TokenUsage, ToolCall};

/// How much of a chat's assistant text [`RunJournal::final_text`] keeps.
///
/// Generous against what it is for — a claims block of a hundred lines is a few
/// kilobytes — and small against what a long run journals, which is the point:
/// this is read once per settle and never rendered.
pub const MESSAGE_TAIL: usize = 64 * 1024;

/// What [`RunJournal::tokens`] found in a chat's journal: everything it spent,
/// and the model the harness said was spending it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalTokens {
    pub usage: TokenUsage,
    /// `None` on a journal whose runs never named a model.
    pub model: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalLine {
    seq: u64,
    event: AgentEvent,
}

struct ChatJournal {
    file: File,
    next_seq: u64,
    /// True when the file ends without a newline (torn write) — the next append
    /// starts with one so the torn line stays isolated.
    needs_newline: bool,
}

/// Append-only JSONL journal store, one file per chat.
pub struct RunJournal {
    dir: PathBuf,
    open_files: Mutex<HashMap<String, ChatJournal>>,
}

impl RunJournal {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, JournalError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            open_files: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, ChatJournal>> {
        self.open_files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn path_for(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{}.jsonl", sanitize_id(chat_id)))
    }

    fn attempts_path(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{}.resume", sanitize_id(chat_id)))
    }

    /// Auto-resume revival budget (comet `resumeAttempt`/`MAX_AUTO_RESUME`):
    /// persisted beside the journal so a run that CRASHES THE ENGINE cannot
    /// revive itself in an infinite boot loop.
    pub fn resume_attempts(&self, chat_id: &str) -> u32 {
        std::fs::read_to_string(self.attempts_path(chat_id))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// The same count, for a reader that is not the recovery loop — the board,
    /// which spends one restart budget across both of them (gh#392).
    ///
    /// `None` when this device has no journal for the chat at all: a chat that
    /// runs on another device, or one whose journal has been swept. The ledger
    /// only ever exists beside a journal, so its absence *with* a journal is a
    /// real zero and its absence *without* one is no answer, and the board
    /// treats those two differently — it will not put a count in front of a
    /// person that was taken from a file it never found.
    pub fn revivals(&self, chat_id: &str) -> Option<u32> {
        self.path_for(chat_id)
            .exists()
            .then(|| self.resume_attempts(chat_id))
    }

    pub fn note_resume_attempt(&self, chat_id: &str) -> u32 {
        let next = self.resume_attempts(chat_id) + 1;
        if let Err(err) = std::fs::write(self.attempts_path(chat_id), next.to_string()) {
            tracing::warn!(chat = %chat_id, error = %err, "resume-attempt ledger write failed");
        }
        next
    }

    /// A cleanly completed turn resets the budget — only consecutive
    /// crash-revive-crash cycles exhaust it.
    pub fn clear_resume_attempts(&self, chat_id: &str) {
        let _ = std::fs::remove_file(self.attempts_path(chat_id));
    }

    /// Append one event; returns its journal seq.
    pub fn append(&self, chat_id: &str, event: &AgentEvent) -> Result<u64, JournalError> {
        let mut files = self.lock();
        if !files.contains_key(chat_id) {
            let path = self.path_for(chat_id);
            let (next_seq, needs_newline) = scan_tail(&path)?;
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            files.insert(
                chat_id.to_string(),
                ChatJournal {
                    file,
                    next_seq,
                    needs_newline,
                },
            );
        }
        // Entry guaranteed present; avoid unwrap in a library path regardless.
        let Some(journal) = files.get_mut(chat_id) else {
            return Err(JournalError::Io(std::io::Error::other(
                "journal entry vanished under lock",
            )));
        };
        let seq = journal.next_seq;
        let line = serde_json::to_string(&JournalLine {
            seq,
            event: event.clone(),
        })?;
        let mut buf = Vec::with_capacity(line.len() + 2);
        if journal.needs_newline {
            buf.push(b'\n');
        }
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        journal.file.write_all(&buf)?;
        journal.file.flush()?;
        journal.needs_newline = false;
        journal.next_seq = seq + 1;
        Ok(seq)
    }

    /// Events with `seq > after_seq`, in order. A cursor ahead of the last issued seq is
    /// from a previous era (file replaced) — falls back to a full replay, mirroring comet.
    pub fn replay(
        &self,
        chat_id: &str,
        after_seq: u64,
    ) -> Result<Vec<(u64, AgentEvent)>, JournalError> {
        let path = self.path_for(chat_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let all = read_lines(&path)?;
        let last_seq = all.last().map(|(seq, _)| *seq).unwrap_or(0);
        let from = if after_seq > last_seq { 0 } else { after_seq };
        Ok(all.into_iter().filter(|(seq, _)| *seq > from).collect())
    }

    /// Everything a chat has spent, summed off its journal (gh#151).
    ///
    /// The journal is the right source and not a live counter: it is already
    /// the durable record of the run (the settle authority, `last_event`), so
    /// an engine restart mid-attempt loses nothing, and a total read here is
    /// the same total the transcript would give.
    ///
    /// [`AgentEvent::Usage`] is emitted once per *turn* by both harnesses (see
    /// [`TokenUsage`]), so this sum is a sum over turns. `None` means the chat
    /// reported no usage at all — a journal that predates gh#151, a harness
    /// that meters nothing, a run that never started. Never `Some(zero)` for
    /// those: the board renders "no usage reported" as a blank, and a zero
    /// would read as work that was free.
    ///
    /// Lines are filtered by tag before they are parsed, because a long run's
    /// journal is mostly text deltas and this is called on every reconcile.
    pub fn tokens(&self, chat_id: &str) -> Result<Option<JournalTokens>, JournalError> {
        const USAGE_TAG: &str = r#""type":"usage""#;
        const STARTED_TAG: &str = r#""type":"sessionStarted""#;
        let path = self.path_for(chat_id);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut out = JournalTokens::default();
        let mut reported = false;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if !line.contains(USAGE_TAG) && !line.contains(STARTED_TAG) {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<JournalLine>(&line) else {
                continue;
            };
            match parsed.event {
                AgentEvent::Usage(usage) => {
                    out.usage.add(usage);
                    reported = true;
                }
                // The model the harness said it was actually running, which is
                // the only place it is stated: a route names one on maybe one
                // dispatch in ten, and the harness default is what the rest
                // ran on. Last wins — a resumed chat can change model, and
                // what it is on now is the more useful of the two.
                AgentEvent::SessionStarted { model, .. } if !model.is_empty() => {
                    out.model = Some(model);
                }
                _ => {}
            }
        }
        Ok(reported.then_some(out))
    }

    /// How full the chat's context window was the last time a harness said
    /// (gh#271).
    ///
    /// **The last reading wins — never a sum.** Fullness is a level, and the
    /// journal holds every reading a run reported; adding them would produce a
    /// number many times the window. That is the whole reason it is not a
    /// [`TokenUsage`] bucket, and the reason this scan looks so unlike
    /// [`tokens`](Self::tokens) two functions up.
    ///
    /// `None` is "nothing was ever reported": a journal that predates gh#271,
    /// a harness that meters no window (opencode), a Claude CLI too old to
    /// answer `get_context_usage`. The board leaves the attempt's columns NULL
    /// for all of them, and the page renders a blank rather than a 0% that
    /// would read as an empty context.
    ///
    /// Tag-filtered before parsing, like every other scan here: this is read
    /// on every reconcile of a live attempt, and a long run's journal is
    /// mostly text deltas.
    pub fn context(&self, chat_id: &str) -> Result<Option<ContextUsage>, JournalError> {
        const CONTEXT_TAG: &str = r#""type":"contextUsage""#;
        let path = self.path_for(chat_id);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut last = None;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if !line.contains(CONTEXT_TAG) {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<JournalLine>(&line) else {
                continue;
            };
            if let AgentEvent::ContextUsage(usage) = parsed.event {
                last = Some(usage);
            }
        }
        Ok(last)
    }

    /// What sandbox this chat's runs actually got (§gh#349) — one line per run,
    /// written by the harness before it says anything else.
    ///
    /// **The last run wins, and it is not a sum or a maximum.** A chat is
    /// persistent across turns and can be resumed after a CLI upgrade, so the
    /// terms it is on now are the terms of its most recent run — the same rule
    /// [`context`](Self::context) follows, for the same reason: this is a state,
    /// not a flow.
    ///
    /// That does lose a chat that ran unsandboxed yesterday and sandboxed
    /// today. The alternative — reporting the loosest level any run ever had —
    /// would make one old run brand a chat for the rest of its life, and a
    /// warning that can never be cleared is one nobody reads. The journal keeps
    /// every line either way, so the history is there for anyone who wants it.
    ///
    /// `None` is "no harness ever said": a journal written before gh#349, and
    /// every chat with no journal at all. The board keeps NULL for those rather
    /// than assuming a level, because assuming the requested one is precisely
    /// the mistake this exists to correct.
    pub fn sandbox(&self, chat_id: &str) -> Result<Option<SandboxReport>, JournalError> {
        const SANDBOX_TAG: &str = r#""type":"sandbox""#;
        let path = self.path_for(chat_id);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut last = None;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if !line.contains(SANDBOX_TAG) {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<JournalLine>(&line) else {
                continue;
            };
            if let AgentEvent::Sandbox(report) = parsed.event {
                last = Some(report);
            }
        }
        Ok(last)
    }

    /// Every shell command this chat's runs executed, paired with how it
    /// exited (§gh#183).
    ///
    /// The board's evidence half: a claim written by the agent is worth reading
    /// only beside something the agent did not author, and the journal already
    /// records every `ToolCall`/`ToolResult` pair. Nothing new is instrumented
    /// here — this is a read of a file that was being written anyway.
    ///
    /// A call whose result never landed (the run died mid-command) comes back
    /// `failed: false`: the journal does not say how it ended, and a guess
    /// would be invented evidence in exactly the direction this exists to
    /// avoid. Same reason the tokens scan filters by tag first: a long run's
    /// journal is mostly text deltas, and this is read on every reconcile of a
    /// live attempt.
    ///
    /// `None` is "no journal for this chat"; `Some(empty)` is a run that
    /// executed no commands at all, which is a much more interesting answer.
    pub fn commands(&self, chat_id: &str) -> Result<Option<Vec<RanCommand>>, JournalError> {
        const CALL_TAG: &str = r#""type":"toolCall""#;
        const RESULT_TAG: &str = r#""type":"toolResult""#;
        let path = self.path_for(chat_id);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // Tool-call ids are unique within a run, so the index is where a result
        // finds the command it belongs to.
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut out: Vec<RanCommand> = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if !line.contains(CALL_TAG) && !line.contains(RESULT_TAG) {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<JournalLine>(&line) else {
                continue;
            };
            match parsed.event {
                AgentEvent::ToolCall {
                    id,
                    call: ToolCall::Exec { command },
                } => {
                    by_id.insert(id, out.len());
                    out.push(RanCommand {
                        command,
                        failed: false,
                    });
                }
                AgentEvent::ToolResult { id, is_error } => {
                    if let Some(ix) = by_id.get(&id) {
                        out[*ix].failed = is_error;
                    }
                }
                _ => {}
            }
        }
        Ok(Some(out))
    }

    /// The tail of what the agent said in this chat, for the claims harvest
    /// (§gh#235).
    ///
    /// The journal already holds every `TextDelta` and every run's `Done`
    /// result; this concatenates them and keeps the last [`MESSAGE_TAIL`]
    /// bytes. A tail, not a transcript: the block a finished attempt writes is
    /// at the end by construction, and a run that talked for a megabyte is not
    /// a run whose opening paragraph is where its claims are.
    ///
    /// The bound is applied on a **character** boundary — the buffer is drained
    /// by whole deltas rather than sliced at a byte offset, so a multi-byte
    /// character never gets cut in half and the caller is never handed a
    /// mangled path.
    ///
    /// `None` is "no journal for this chat"; `Some("")` is a run that never
    /// said anything, which parses to no claims either way.
    pub fn final_text(&self, chat_id: &str) -> Result<Option<String>, JournalError> {
        const TEXT_TAG: &str = r#""type":"textDelta""#;
        const DONE_TAG: &str = r#""type":"done""#;
        let path = self.path_for(chat_id);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // Kept as the pieces that arrived rather than one string, so trimming
        // to the tail is dropping whole pieces off the front.
        let mut parts: std::collections::VecDeque<String> = Default::default();
        let mut held = 0usize;
        let mut push = |text: String, parts: &mut std::collections::VecDeque<String>| {
            held += text.len();
            parts.push_back(text);
            while held > MESSAGE_TAIL && parts.len() > 1 {
                if let Some(dropped) = parts.pop_front() {
                    held -= dropped.len();
                }
            }
        };
        for line in BufReader::new(file).lines() {
            let line = line?;
            if !line.contains(TEXT_TAG) && !line.contains(DONE_TAG) {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<JournalLine>(&line) else {
                continue;
            };
            match parsed.event {
                AgentEvent::TextDelta { text } => push(text, &mut parts),
                // A harness that reports its final message once, whole, rather
                // than as deltas. Both are appended: one of them is empty on
                // any given harness, and a harness that does both would only be
                // saying the same thing twice — which a fence search does not
                // care about.
                AgentEvent::Done {
                    result: Some(result),
                    ..
                } if !result.is_empty() => push(result, &mut parts),
                _ => {}
            }
        }
        Ok(Some(parts.into_iter().collect()))
    }

    /// The last event in a chat's journal, if any (ignores a torn tail line).
    pub fn last_event(&self, chat_id: &str) -> Result<Option<(u64, AgentEvent)>, JournalError> {
        let path = self.path_for(chat_id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(read_lines(&path)?.into_iter().next_back())
    }

    /// Crash-recovery scan: chat ids whose journal's last event is NOT a `Done` — their
    /// runs died mid-stream and need recovery (stamp `aborted`, close the journal).
    pub fn stale_sessions(&self) -> Result<Vec<String>, JournalError> {
        let mut stale = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(chat_id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let last = read_lines(&path)?.into_iter().next_back();
            match last {
                Some((_, AgentEvent::Done { .. })) | None => {}
                Some(_) => stale.push(chat_id.to_string()),
            }
        }
        stale.sort();
        Ok(stale)
    }

    /// Remove a chat's journal file entirely (tests / future compaction).
    pub fn discard(&self, chat_id: &str) -> Result<(), JournalError> {
        self.lock().remove(chat_id);
        let path = self.path_for(chat_id);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

/// Parse every valid line; malformed lines (torn tail writes) are skipped.
fn read_lines(path: &Path) -> Result<Vec<(u64, AgentEvent)>, JournalError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<JournalLine>(&line) {
            Ok(parsed) => out.push((parsed.seq, parsed.event)),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "journal: skipping malformed line");
            }
        }
    }
    Ok(out)
}

/// Next seq (last valid seq + 1, starting at 1) and whether the file ends mid-line.
fn scan_tail(path: &Path) -> Result<(u64, bool), JournalError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((1, false)),
        Err(e) => return Err(e.into()),
    };
    let needs_newline = bytes.last().is_some_and(|b| *b != b'\n');
    let next_seq = read_lines(path)?
        .last()
        .map(|(seq, _)| seq + 1)
        .unwrap_or(1);
    Ok((next_seq, needs_newline))
}

/// Chat ids become file names; anything outside a conservative set is replaced so a
/// hostile id cannot traverse paths. (Ids are uuids in practice.)
fn sanitize_id(chat_id: &str) -> String {
    chat_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::DoneStatus;

    fn text(s: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: s.into() }
    }

    fn done() -> AgentEvent {
        AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
        }
    }

    fn usage(input: u64, output: u64) -> AgentEvent {
        AgentEvent::Usage(TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: input * 10,
            cache_creation_tokens: 1,
        })
    }

    fn started(model: &str) -> AgentEvent {
        AgentEvent::SessionStarted {
            harness: comet_proto::HarnessId::ClaudeCode,
            model: model.into(),
            tools: Vec::new(),
            cwd: "/tmp".into(),
            session_id: "s1".into(),
            assistant_message_id: "m1".into(),
        }
    }

    /// One `Usage` event per turn, so a run's total is the sum over its turns
    /// — the property gh#151 had to settle before anything could be added up.
    #[test]
    fn a_runs_tokens_are_the_sum_of_its_turns() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("chat-1", &started("claude-opus-5")).unwrap();
        journal.append("chat-1", &text("thinking")).unwrap();
        journal.append("chat-1", &usage(100, 10)).unwrap();
        journal.append("chat-1", &done()).unwrap();
        // A second turn in the same chat.
        journal.append("chat-1", &usage(50, 5)).unwrap();
        journal.append("chat-1", &done()).unwrap();

        let tokens = journal.tokens("chat-1").unwrap().expect("reported");
        assert_eq!(tokens.usage.input_tokens, 150);
        assert_eq!(tokens.usage.output_tokens, 15);
        assert_eq!(tokens.usage.cache_read_tokens, 1_500);
        assert_eq!(tokens.usage.cache_creation_tokens, 2);
        assert_eq!(tokens.model.as_deref(), Some("claude-opus-5"));
    }

    /// The board spends one restart budget across the engine's revivals and
    /// its own (gh#392), so what it reads here has to separate "this device
    /// revived it twice" from "this device has never run this chat" — the
    /// second is not a zero, and a zero is what would let the board claim a
    /// count it never took.
    #[test]
    fn revivals_are_a_count_only_where_there_is_a_journal_to_count_from() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.revivals("elsewhere"), None, "no journal, no answer");

        journal.append("chat-1", &started("mock")).unwrap();
        assert_eq!(
            journal.revivals("chat-1"),
            Some(0),
            "and this one is a zero"
        );

        journal.note_resume_attempt("chat-1");
        journal.note_resume_attempt("chat-1");
        assert_eq!(journal.revivals("chat-1"), Some(2));

        // A turn that completed cleanly clears it — both budgets are counting
        // a run that cannot get going, not one that has had a long life.
        journal.clear_resume_attempts("chat-1");
        assert_eq!(journal.revivals("chat-1"), Some(0));
    }

    /// A chat that reported nothing is `None`, never `Some(zero)`: the board
    /// leaves such an attempt blank, and a zero would read as free work.
    #[test]
    fn a_chat_that_reported_nothing_says_nothing_rather_than_zero() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.tokens("never-ran").unwrap(), None);
        journal.append("chat-1", &started("mock")).unwrap();
        journal.append("chat-1", &text("hello")).unwrap();
        journal.append("chat-1", &done()).unwrap();
        assert_eq!(
            journal.tokens("chat-1").unwrap(),
            None,
            "a harness that meters nothing is not a harness that spent nothing"
        );
    }

    /// Fullness is a level: the newest reading is the answer, and the older
    /// ones are history — never addends. (Summing the three below would say a
    /// window twice its own size was full, which is the mistake this scan
    /// exists to not make.)
    #[test]
    fn the_context_reading_is_the_last_one_and_never_a_sum() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.context("never-ran").unwrap(), None);
        journal.append("chat-1", &started("claude-opus-5")).unwrap();
        for used in [40_000, 120_000, 170_000] {
            journal
                .append(
                    "chat-1",
                    &AgentEvent::ContextUsage(ContextUsage {
                        used_tokens: used,
                        max_tokens: 200_000,
                        compact_at_tokens: Some(167_000),
                    }),
                )
                .unwrap();
        }
        journal.append("chat-1", &usage(1, 1)).unwrap();
        journal.append("chat-1", &done()).unwrap();

        let ctx = journal.context("chat-1").unwrap().expect("reported");
        assert_eq!(ctx.used_tokens, 170_000);
        assert_eq!(ctx.percent(), Some(85));
        assert!(ctx.is_near_compaction(0.9), "past the CLI's own threshold");
        // The spend scan is untouched by any of it — two meters, one journal.
        assert_eq!(journal.tokens("chat-1").unwrap().unwrap().usage.total(), 13);
    }

    /// The sandbox a chat is on is the one its most recent run got — a chat
    /// resumed after a CLI upgrade is not still branded by the terms of the
    /// run before it (§gh#349).
    #[test]
    fn the_sandbox_reading_is_the_last_run_not_the_loosest_one_ever_seen() {
        use comet_proto::{SandboxLevel, SandboxReport};
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.sandbox("never-ran").unwrap(), None);

        // Run one, on a codex old enough to earn the escalation.
        let widened = SandboxReport::widened(
            SandboxLevel::WorkspaceWrite,
            SandboxLevel::DangerFullAccess,
            "this codex predates the worktree-mount fix",
        );
        journal
            .append("chat-1", &AgentEvent::Sandbox(widened.clone()))
            .unwrap();
        journal.append("chat-1", &started("gpt-5.6-codex")).unwrap();
        journal.append("chat-1", &done()).unwrap();
        assert_eq!(journal.sandbox("chat-1").unwrap(), Some(widened));

        // Run two, after the upgrade. The chat is now sandboxed, and the read
        // says so rather than carrying yesterday's escalation forever.
        let clean = SandboxReport::as_requested(SandboxLevel::WorkspaceWrite);
        journal
            .append("chat-1", &AgentEvent::Sandbox(clean.clone()))
            .unwrap();
        journal.append("chat-1", &started("gpt-5.6-codex")).unwrap();
        journal.append("chat-1", &done()).unwrap();
        assert_eq!(journal.sandbox("chat-1").unwrap(), Some(clean));

        // A journal from before any harness reported one says nothing, which
        // must not be read as "it was sandboxed".
        journal.append("chat-2", &started("mock")).unwrap();
        journal.append("chat-2", &done()).unwrap();
        assert_eq!(journal.sandbox("chat-2").unwrap(), None);
    }

    /// A chat whose harness never metered a window says nothing, rather than
    /// saying 0% — which would read as an empty context, not a silent one.
    #[test]
    fn a_chat_whose_harness_meters_no_window_reports_no_fullness() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("chat-1", &started("mock")).unwrap();
        journal.append("chat-1", &usage(5, 5)).unwrap();
        journal.append("chat-1", &done()).unwrap();
        assert_eq!(journal.context("chat-1").unwrap(), None);
    }

    /// The scan filters lines by tag before parsing them. A transcript that
    /// happens to *contain* the tag (an agent pasting its own journal, say)
    /// must not be mistaken for a usage event.
    #[test]
    fn text_that_merely_looks_like_a_usage_line_is_not_counted_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal
            .append("chat-1", &text(r#"{"type":"usage","inputTokens":9999}"#))
            .unwrap();
        journal.append("chat-1", &usage(1, 1)).unwrap();
        let tokens = journal.tokens("chat-1").unwrap().expect("reported");
        assert_eq!(tokens.usage.input_tokens, 1);
    }

    /// The model last announced wins — a chat resumed on another model is on
    /// that one now, and that is the more useful of the two answers.
    #[test]
    fn the_model_recorded_is_the_one_the_last_run_named() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal
            .append("chat-1", &started("claude-sonnet-5"))
            .unwrap();
        journal.append("chat-1", &usage(1, 1)).unwrap();
        journal.append("chat-1", &started("claude-opus-5")).unwrap();
        journal.append("chat-1", &usage(1, 1)).unwrap();
        let tokens = journal.tokens("chat-1").unwrap().expect("reported");
        assert_eq!(tokens.model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn appends_are_monotonic_and_replayable() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.append("chat-1", &text("a")).unwrap(), 1);
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
        assert_eq!(journal.append("chat-1", &done()).unwrap(), 3);

        let all = journal.replay("chat-1", 0).unwrap();
        assert_eq!(all.len(), 3);
        let after = journal.replay("chat-1", 2).unwrap();
        assert_eq!(after.len(), 1);
        assert!(matches!(after[0].1, AgentEvent::Done { .. }));
        // Era fallback: cursor ahead of last seq replays everything.
        assert_eq!(journal.replay("chat-1", 99).unwrap().len(), 3);
    }

    #[test]
    fn seq_continues_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = RunJournal::open(dir.path()).unwrap();
            journal.append("chat-1", &text("a")).unwrap();
        }
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
    }

    #[test]
    fn stale_scan_flags_journals_without_terminal_done() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("dead", &text("partial")).unwrap();
        journal.append("clean", &text("full")).unwrap();
        journal.append("clean", &done()).unwrap();
        assert_eq!(journal.stale_sessions().unwrap(), vec!["dead".to_string()]);
        // Closing the stale journal with a Done clears the flag.
        journal.append("dead", &done()).unwrap();
        assert!(journal.stale_sessions().unwrap().is_empty());
    }

    #[test]
    fn torn_tail_line_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = RunJournal::open(dir.path()).unwrap();
            journal.append("chat-1", &text("a")).unwrap();
        }
        // Simulate a crash mid-write: garbage with no trailing newline.
        let path = dir.path().join("chat-1.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"seq\":2,\"event\":{\"type\":\"textD")
            .unwrap();
        drop(f);

        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.replay("chat-1", 0).unwrap().len(), 1);
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
        let all = journal.replay("chat-1", 0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].0, 2);
    }

    // ---- the closing message, for the claims harvest (§gh#235) ------------

    /// Deltas and `Done` results alike: one harness streams the final message,
    /// another reports it whole, and the fence search does not care which.
    #[test]
    fn the_closing_message_is_the_chats_own_words_back() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("chat-1", &started("mock")).unwrap();
        journal.append("chat-1", &text("```claims\n")).unwrap();
        journal
            .append("chat-1", &text("Did the storage :: src/db.rs\n"))
            .unwrap();
        journal.append("chat-1", &text("```")).unwrap();
        journal
            .append(
                "chat-1",
                &AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: Some("\nand that is that.".into()),
                    error: None,
                    session_id: None,
                },
            )
            .unwrap();

        let said = journal.final_text("chat-1").unwrap().expect("a journal");
        assert!(said.contains("```claims"), "{said}");
        assert!(said.contains("Did the storage :: src/db.rs"), "{said}");
        assert!(said.ends_with("and that is that."), "{said}");
    }

    /// A chat that never ran is `None`; one that ran and said nothing is an
    /// empty string. Both harvest to no claims, and neither is an error.
    #[test]
    fn a_chat_with_no_journal_and_a_chat_that_said_nothing_are_different() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.final_text("never-ran").unwrap(), None);
        journal.append("chat-1", &started("mock")).unwrap();
        journal.append("chat-1", &done()).unwrap();
        assert_eq!(journal.final_text("chat-1").unwrap().as_deref(), Some(""));
    }

    /// The tail, and the tail is where the block is: an agent writes its
    /// claims when the work is done.
    #[test]
    fn a_long_run_keeps_the_end_of_what_it_said() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        let filler = "x".repeat(4096);
        journal
            .append("chat-1", &text("the opening paragraph"))
            .unwrap();
        for _ in 0..40 {
            journal.append("chat-1", &text(&filler)).unwrap();
        }
        journal
            .append("chat-1", &text("```claims\nDid it :: src/a.rs\n```"))
            .unwrap();

        let said = journal.final_text("chat-1").unwrap().unwrap();
        assert!(said.len() <= MESSAGE_TAIL + filler.len());
        assert!(said.ends_with("```"), "the end survives");
        assert!(
            !said.contains("the opening paragraph"),
            "and the front is what falls off"
        );
    }
}
