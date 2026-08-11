//! The turn-level cap on an agent spinning in place (gh#270): how much a run
//! may fail, or churn, before the board says something and then stops it.
//!
//! [`crate::overrun`] catches the *slow* run — two hours of wall clock, whatever
//! it spent them on. Nothing caught the fast one. An agent retrying the same
//! failing command, or making tool calls without end, burns tokens at full
//! speed for as long as the wall clock lets it; on an unattended board that is
//! the biggest unguarded failure mode there is, and the clock that eventually
//! closes it charges for the whole two hours first.
//!
//! comet cannot enforce this inside the agent's own loop — the loop belongs to
//! Claude Code, Codex or opencode — and it does not need to. It already watches
//! the loop from outside: every harness normalizes its stream into
//! [`AgentEvent::ToolCall`] / [`AgentEvent::ToolResult`], the `is_error` flag on
//! the result is normalized too (claude's `is_error` block field, codex's
//! `failed`/non-zero exit, opencode's status), and the engine can both steer a
//! live run and interrupt it. So this counts what goes past and says which rung
//! of the ladder the run is on:
//!
//! - **Steer first.** Most loops are recoverable, and an interrupt throws away
//!   a worktree's worth of context. The soft rung is one message into the chat
//!   naming what is looping — the same shape as the duration cap's warning, and
//!   for the same reason: a run the board is about to end gets told first.
//! - **Then stop.** The hard rung ends the run with an error, which is the
//!   *existing* block path: the attempt keeps its chat and its context, the
//!   board's reconcile reads `Errored` off the journal, and the orchestrator
//!   (or the dispatching agent) is told exactly as it is told about any other
//!   block. Nothing new had to be invented for anybody to hear about this.
//!
//! Kept pure here — no chat, no config, no engine — so the arithmetic is
//! testable on a list of events, exactly as [`crate::overrun`]'s is on a clock.

use std::collections::HashMap;

use comet_proto::{AgentEvent, ToolCall, TurnLimits};

/// The shortest failure cap the config will accept. Two failures in a row is a
/// flaky test and a retry; three is the first number that could mean a loop,
/// and a cap below it would steer half the runs on the board.
pub const MIN_TOOL_FAILURES: u32 = 3;

/// The shortest call cap the config will accept. A single turn that reads a
/// file, greps for a symbol and runs a test suite is already a dozen calls;
/// anything under this would fire before an agent had finished orienting.
pub const MIN_TOOL_CALLS: u32 = 50;

/// How many failures of *assorted* tools mean what `N` failures of one tool
/// mean. Stuck and flailing are both loops, but they are not equally certain:
/// twenty different calls failing in a row is a run that has lost the thread,
/// and ten identical ones is a run that has stopped reading its own output.
const ANY_TOOL_MULTIPLE: u32 = 2;

/// The hard rung is twice the soft one, on every dimension. The warning buys
/// the agent exactly as much rope as it had already spent — enough to commit
/// what it has and say what it is stuck on, not enough to spend the afternoon.
///
/// Counted from zero, not from the warning, and the counters are not reset by
/// the steer that carries it: the ladder has to hold for a run whose warning
/// was never delivered at all (a harness with no steering, a mailbox torn down
/// mid-turn), and a rung that only armed on delivery would leave exactly those
/// runs unbounded. Nothing is lost by it — a *success* still clears the failure
/// counters, so an agent that takes the advice is out of the ladder entirely by
/// the first tool call that lands.
const HARD_MULTIPLE: u32 = 2;

/// Which rung of the ladder a run has reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    /// Say so, once, into the chat. The run keeps going.
    Steer,
    /// End the run. It gets an error the transcript keeps and the board reads.
    Stop,
}

/// What is looping, in the terms the message will use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The same call, failing over and over — the unambiguous case.
    SameTool { call: String, failures: u32 },
    /// Assorted calls, all failing. Nothing is landing.
    AnyTool { failures: u32 },
    /// No failures needed: this turn has simply not stopped calling tools.
    Calls { calls: u32 },
}

impl Reason {
    /// The clause every message about this breach is built on, so the chat, the
    /// log and the transcript's error part cannot describe it three ways.
    pub fn sentence(&self) -> String {
        match self {
            Reason::SameTool { call, failures } => {
                format!("`{call}` has failed {failures} times in a row")
            }
            Reason::AnyTool { failures } => {
                format!("{failures} tool calls in a row have failed")
            }
            Reason::Calls { calls } => {
                format!("this turn has made {calls} tool calls")
            }
        }
    }
}

/// One breach: which rung, and what tripped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub rung: Rung,
    pub reason: Reason,
}

impl Verdict {
    /// What the agent is told at the soft rung.
    ///
    /// Addressed to an agent that can still act: it names what the board can
    /// see, asks for a change of approach, and spells out the alternative —
    /// stopping and *saying* it is blocked is a good outcome here, and an agent
    /// that does not know that will keep retrying until it is interrupted.
    pub fn steer_text(&self) -> String {
        format!(
            "comet-board: {} — from out here that looks like a loop. Change approach, or \
             stop and say what you are blocked on: commit what you have and open a pull \
             request describing where it got stuck. If this keeps up the board will end \
             the run.",
            self.reason.sentence()
        )
    }

    /// What the transcript, the journal and the log say when the run is ended.
    ///
    /// Written for the two readers it actually gets: the operator scrolling the
    /// chat tomorrow, and whoever picks the attempt up — so it says what was
    /// counted and that the context survived, which is the difference between
    /// this and a lost attempt.
    pub fn stop_text(&self) -> String {
        format!(
            "comet-board ended this run: {}, and the warning it was already given did not \
             change that. The chat still holds the whole task — retry or cancel the attempt \
             from the board.",
            self.reason.sentence()
        )
    }
}

/// The per-turn counters, fed one [`AgentEvent`] at a time.
///
/// One of these per run, not per turn: [`AgentEvent::Done`] is the turn
/// boundary and resets it in place, so a persistent session's second turn
/// starts from zero without the caller tracking anything.
pub struct Watch {
    limits: TurnLimits,
    /// Tool call ids seen but not yet resolved → what they were, so a result
    /// can be attributed to the call it answers.
    inflight: HashMap<String, String>,
    /// The current run of identical failures: what, and how many.
    same: Option<(String, u32)>,
    /// Failures in a row, whatever they were.
    any: u32,
    /// Tool calls this turn.
    calls: u32,
    /// The one steer this turn is allowed, spent by the soft rung. Only the
    /// turn ending gives it back — a run warned at the cap that goes on to the
    /// hard rung must not be warned a second time on the way.
    steered: bool,
    /// The run has been ended; nothing more is counted or said.
    stopped: bool,
}

impl Watch {
    pub fn new(limits: TurnLimits) -> Self {
        Watch {
            limits,
            inflight: HashMap::new(),
            same: None,
            any: 0,
            calls: 0,
            steered: false,
            stopped: false,
        }
    }

    /// Nothing configured — every chat the board did not dispatch. The caller
    /// can skip the whole thing, and [`Watch::observe`] answers `None` anyway.
    pub fn is_off(&self) -> bool {
        self.limits.is_off()
    }

    /// Count one event, and say whether it just tripped a rung.
    ///
    /// At most one verdict per rung per turn: the soft one is spent by the
    /// first breach, and the hard one ends the run, so a spinning agent
    /// produces two messages and not two hundred.
    pub fn observe(&mut self, event: &AgentEvent) -> Option<Verdict> {
        if self.stopped || self.limits.is_off() {
            return None;
        }
        match event {
            AgentEvent::ToolCall { id, call } => {
                self.inflight.insert(id.clone(), signature(call));
                self.calls = self.calls.saturating_add(1);
            }
            AgentEvent::ToolResult { id, is_error } => {
                let signature = self.inflight.remove(id);
                if !is_error {
                    // Something landed. Whatever the agent was stuck on, it is
                    // not stuck on it now — consecutive means consecutive.
                    self.same = None;
                    self.any = 0;
                    return None;
                }
                self.any = self.any.saturating_add(1);
                self.same = match (self.same.take(), signature) {
                    (Some((prev, n)), Some(now)) if prev == now => {
                        Some((prev, n.saturating_add(1)))
                    }
                    (_, Some(now)) => Some((now, 1)),
                    // A result whose call we never saw (an id from before this
                    // run, a harness that resolves twice): it still counts as a
                    // failure, it just cannot extend a run of one call.
                    (_, None) => None,
                };
            }
            // The turn ended. Everything resets, including the steer: the next
            // turn is new work and gets its own warning before it is stopped.
            AgentEvent::Done { .. } => {
                self.inflight.clear();
                self.same = None;
                self.any = 0;
                self.calls = 0;
                self.steered = false;
                return None;
            }
            _ => return None,
        }
        self.verdict()
    }

    fn verdict(&mut self) -> Option<Verdict> {
        // The hard rung first. A run that crosses both on one event — a steer
        // that never reached the harness, a cap raised under a live run — is
        // stopped rather than warned about a line it is already well past.
        if let Some(reason) = self.breach(HARD_MULTIPLE) {
            self.stopped = true;
            return Some(Verdict {
                rung: Rung::Stop,
                reason,
            });
        }
        if self.steered {
            return None;
        }
        let reason = self.breach(1)?;
        self.steered = true;
        Some(Verdict {
            rung: Rung::Steer,
            reason,
        })
    }

    /// What has crossed `multiple` times its cap, if anything. The order is the
    /// order of certainty: the same call failing is the clearest thing the
    /// board can see, and sheer volume the least.
    fn breach(&self, multiple: u32) -> Option<Reason> {
        if let Some(cap) = self.limits.tool_failures {
            if let Some((call, failures)) = &self.same
                && *failures >= cap.saturating_mul(multiple)
            {
                return Some(Reason::SameTool {
                    call: call.clone(),
                    failures: *failures,
                });
            }
            if self.any
                >= cap
                    .saturating_mul(ANY_TOOL_MULTIPLE)
                    .saturating_mul(multiple)
            {
                return Some(Reason::AnyTool { failures: self.any });
            }
        }
        if let Some(cap) = self.limits.tool_calls
            && self.calls >= cap.saturating_mul(multiple)
        {
            return Some(Reason::Calls { calls: self.calls });
        }
        None
    }
}

/// How much of a call has to match for two of them to be "the same call".
///
/// The whole target, not the tool name: an agent alternating between three
/// broken files is not looping on any one of them, and an agent running
/// `cargo test -p comet-board` for the ninth time is. Clipped so one enormous
/// heredoc cannot make the chat message unreadable — the clip is applied to
/// both sides equally, so it never merges two distinct calls that differ only
/// past the cut.
fn signature(call: &ToolCall) -> String {
    match call {
        ToolCall::Exec { command } => format!("exec {}", clip(command)),
        ToolCall::ReadFile { path } => format!("read {}", clip(path)),
        ToolCall::WriteFile { path, .. } => format!("write {}", clip(path)),
        ToolCall::EditFile { path, .. } => format!("edit {}", clip(path)),
        ToolCall::ApplyPatch { path } => {
            format!("patch {}", clip(path.as_deref().unwrap_or("(inline)")))
        }
        ToolCall::Search { pattern, path } => match path {
            Some(path) => format!("search {} in {}", clip(pattern), clip(path)),
            None => format!("search {}", clip(pattern)),
        },
        ToolCall::Glob { pattern } => format!("glob {}", clip(pattern)),
        ToolCall::WebFetch { url, .. } => format!("fetch {}", clip(url)),
        ToolCall::WebSearch { query } => format!("web search {}", clip(query)),
        ToolCall::Todo { .. } => "todo".into(),
        ToolCall::Mcp { server, tool, .. } => format!("mcp {server}/{tool}"),
        ToolCall::Skill { name, .. } => format!("skill /{name}"),
        ToolCall::Task { description, .. } => format!("task {}", clip(description)),
        ToolCall::Unknown { name, .. } => format!("tool {name}"),
    }
}

/// One line's worth of a tool's target, whitespace flattened so a multi-line
/// command reads as one phrase in the chat.
fn clip(s: &str) -> String {
    const MAX: usize = 60;
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX {
        return flat;
    }
    format!("{}…", flat.chars().take(MAX).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(tool_failures: Option<u32>, tool_calls: Option<u32>) -> TurnLimits {
        TurnLimits {
            tool_failures,
            tool_calls,
        }
    }

    fn call(id: &str, command: &str) -> AgentEvent {
        AgentEvent::ToolCall {
            id: id.into(),
            call: ToolCall::Exec {
                command: command.into(),
            },
        }
    }

    fn result(id: &str, is_error: bool) -> AgentEvent {
        AgentEvent::ToolResult {
            id: id.into(),
            is_error,
        }
    }

    /// Run `n` attempts at `command` through the watch, all failing, and return
    /// every verdict it produced.
    fn fail(watch: &mut Watch, command: &str, n: u32) -> Vec<Verdict> {
        let mut out = Vec::new();
        for i in 0..n {
            let id = format!("{command}-{i}");
            out.extend(watch.observe(&call(&id, command)));
            out.extend(watch.observe(&result(&id, true)));
        }
        out
    }

    #[test]
    fn a_run_that_is_getting_somewhere_is_never_touched() {
        // The exit criterion's other half: a working agent must cost nothing.
        let mut watch = Watch::new(limits(Some(3), Some(50)));
        for i in 0..40 {
            let id = format!("t{i}");
            assert_eq!(watch.observe(&call(&id, "cargo test")), None);
            assert_eq!(watch.observe(&result(&id, false)), None);
        }
    }

    #[test]
    fn one_success_clears_the_run_of_failures() {
        let mut watch = Watch::new(limits(Some(3), None));
        assert!(fail(&mut watch, "cargo test", 2).is_empty());
        // A landing call resets it; two more failures are two, not four.
        assert_eq!(watch.observe(&call("ok", "cargo build")), None);
        assert_eq!(watch.observe(&result("ok", false)), None);
        assert!(fail(&mut watch, "cargo test", 2).is_empty());
    }

    #[test]
    fn the_same_call_failing_steers_at_the_cap_and_stops_at_twice_it() {
        let mut watch = Watch::new(limits(Some(3), None));
        let verdicts = fail(&mut watch, "cargo test -p comet-board", 3);
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].rung, Rung::Steer);
        assert_eq!(
            verdicts[0].reason,
            Reason::SameTool {
                call: "exec cargo test -p comet-board".into(),
                failures: 3
            }
        );
        // Warned and unchanged: three more of the same ends it.
        let verdicts = fail(&mut watch, "cargo test -p comet-board", 3);
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].rung, Rung::Stop);
        // …and nothing is said twice, however long the stream goes on.
        assert!(fail(&mut watch, "cargo test -p comet-board", 20).is_empty());
    }

    /// Assorted failing calls are the looser signal, so they get twice the rope
    /// — and a run alternating between two commands never accumulates a
    /// same-call streak at all.
    #[test]
    fn assorted_failures_take_twice_as_many_to_say_the_same_thing() {
        let mut watch = Watch::new(limits(Some(3), None));
        let mut verdicts = Vec::new();
        for i in 0..6 {
            let id = format!("t{i}");
            verdicts.extend(watch.observe(&call(&id, &format!("try-{i}"))));
            verdicts.extend(watch.observe(&result(&id, true)));
        }
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].rung, Rung::Steer);
        assert_eq!(verdicts[0].reason, Reason::AnyTool { failures: 6 });
    }

    /// The warning landing changes nothing about the ladder — which is the
    /// property that matters, because the run whose warning never *did* land
    /// (a harness that cannot be steered, a mailbox torn down mid-turn) is
    /// exactly the one that must still be stopped.
    #[test]
    fn a_delivered_warning_neither_re_arms_the_run_nor_repeats_itself() {
        let mut watch = Watch::new(limits(Some(3), None));
        assert_eq!(fail(&mut watch, "cargo test", 3)[0].rung, Rung::Steer);
        watch.observe(&AgentEvent::Steered {
            assistant_message_id: None,
            next_assistant_message_id: None,
        });
        let verdicts = fail(&mut watch, "cargo test", 3);
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].rung, Rung::Stop);
    }

    /// …and an agent that takes the advice is out of the ladder entirely: one
    /// call that lands clears the failures, so the rope it has left is the
    /// whole of it again.
    #[test]
    fn taking_the_advice_clears_the_ladder() {
        let mut watch = Watch::new(limits(Some(3), None));
        assert_eq!(fail(&mut watch, "cargo test", 3)[0].rung, Rung::Steer);
        watch.observe(&call("ok", "cargo build"));
        watch.observe(&result("ok", false));
        // Five more failures and the run is still going: eight in the turn is
        // well past the hard rung's six, but they are not consecutive, and
        // consecutive is the only thing this counts.
        assert!(fail(&mut watch, "cargo test", 5).is_empty());
    }

    /// A turn is the unit. The next one is new work, and gets its own warning
    /// before anything is ended.
    #[test]
    fn a_finished_turn_starts_the_next_one_from_zero() {
        let mut watch = Watch::new(limits(Some(3), Some(50)));
        assert_eq!(fail(&mut watch, "cargo test", 3)[0].rung, Rung::Steer);
        watch.observe(&AgentEvent::Done {
            status: comet_proto::DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
        });
        let verdicts = fail(&mut watch, "cargo test", 3);
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].rung, Rung::Steer);
    }

    #[test]
    fn sheer_volume_trips_without_a_single_failure() {
        let mut watch = Watch::new(limits(None, Some(50)));
        let mut verdicts = Vec::new();
        for i in 0..100 {
            let id = format!("t{i}");
            verdicts.extend(watch.observe(&call(&id, "read the same file again")));
            verdicts.extend(watch.observe(&result(&id, false)));
        }
        assert_eq!(verdicts.len(), 2);
        assert_eq!(verdicts[0].rung, Rung::Steer);
        assert_eq!(verdicts[0].reason, Reason::Calls { calls: 50 });
        assert_eq!(verdicts[1].rung, Rung::Stop);
        assert_eq!(verdicts[1].reason, Reason::Calls { calls: 100 });
    }

    /// Nothing inside a turn refills the call budget — a turn that has made a
    /// thousand calls has made them, whoever said what to it in between.
    #[test]
    fn a_steer_does_not_refill_the_call_budget() {
        let mut watch = Watch::new(limits(None, Some(50)));
        let mut verdicts = Vec::new();
        for i in 0..100 {
            if i == 60 {
                verdicts.extend(watch.observe(&AgentEvent::Steered {
                    assistant_message_id: None,
                    next_assistant_message_id: None,
                }));
            }
            let id = format!("t{i}");
            verdicts.extend(watch.observe(&call(&id, "spin")));
            verdicts.extend(watch.observe(&result(&id, false)));
        }
        assert_eq!(verdicts.last().map(|v| v.rung), Some(Rung::Stop));
    }

    /// Off is off: the chats a person opened are not policed, and the watch
    /// must cost them nothing.
    #[test]
    fn no_limits_means_no_verdicts_ever() {
        let mut watch = Watch::new(TurnLimits::default());
        assert!(watch.is_off());
        assert!(fail(&mut watch, "cargo test", 500).is_empty());
    }

    /// The signature is the whole target, so an agent working through three
    /// broken files is not reported as looping on one of them.
    #[test]
    fn different_targets_are_different_calls() {
        assert_eq!(
            signature(&ToolCall::ReadFile {
                path: "/a/b.rs".into()
            }),
            "read /a/b.rs"
        );
        assert_ne!(
            signature(&ToolCall::ReadFile {
                path: "/a/b.rs".into()
            }),
            signature(&ToolCall::ReadFile {
                path: "/a/c.rs".into()
            })
        );
        // …and a multi-line command reads as one phrase, clipped.
        let long = signature(&ToolCall::Exec {
            command: format!("run\n  {}", "x".repeat(200)),
        });
        assert!(long.starts_with("exec run x"));
        assert!(long.ends_with('…'));
        assert!(long.chars().count() < 80);
    }

    #[test]
    fn every_breach_says_what_it_counted() {
        let steer = Verdict {
            rung: Rung::Steer,
            reason: Reason::SameTool {
                call: "exec cargo test".into(),
                failures: 8,
            },
        };
        assert!(
            steer
                .steer_text()
                .contains("`exec cargo test` has failed 8 times in a row")
        );
        let stop = Verdict {
            rung: Rung::Stop,
            reason: Reason::Calls { calls: 4000 },
        };
        assert!(
            stop.stop_text()
                .contains("this turn has made 4000 tool calls")
        );
        // The chat is told the context survived — the difference between a
        // stopped run and a lost attempt.
        assert!(stop.stop_text().contains("retry or cancel"));
    }
}
