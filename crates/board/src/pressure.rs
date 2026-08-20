//! What the box has left, and what an OOM kill looked like (gh#533).
//!
//! gh#529 stopped the *engine* dying when the kernel reached into its cgroup.
//! This is the other half of that night: the board kept releasing work onto a
//! box that had nothing left to release it onto, and when the kernel finally
//! chose a victim the chat said "claude exited unexpectedly (killed by signal
//! 9)" — which reads as a flaky agent, not as a full box.
//!
//! Two questions, one module, because they are asked of the same three files:
//!
//! - **Is there room to start another agent?** [`headroom`] — a slot count is
//!   not a memory budget. Three slots of Next.js builds is not three slots of
//!   doc edits, and the Mylder rule held a swapless 16G box at 3-of-3 heavy
//!   builds until the kernel took the engine (gh#526 — an incident, so no `§`:
//!   it names no file in `docs/board/`).
//! - **Did this run die because the box ran out?** [`OomWatch`] — armed when a
//!   run starts, asked when its child dies on a signal. The answer is a delta
//!   in the kernel's own OOM-kill counters, not a guess from the signal number.
//!
//! Everything is read from `/proc` and `/sys/fs/cgroup`, so everything is
//! Linux. On macOS every reading is `None` and every decision that depends on
//! one abstains: a laptop that cannot be measured must not be *described*, and
//! a dispatch gate that guessed would refuse work on the box where the operator
//! is sitting. The parsers are pure and take the file text, so the decisions are
//! testable on the machine this is written on.

/// What `/proc/meminfo` says, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    pub total: u64,
    /// `MemAvailable` — the kernel's own estimate of what a new allocation can
    /// have without swapping, which is the number this is about. `MemFree` is
    /// not it: a healthy box keeps almost nothing free and almost everything
    /// reclaimable, so free-memory alarms fire on every box that is working.
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl Memory {
    /// The share of the box's memory a new allocation could still have, 0.0–1.0.
    pub fn available_share(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.available as f64 / self.total as f64
    }

    /// A box with no swap has no slack at all: the kernel's only answer to a
    /// spike is to kill something. Worth saying in every sentence about it.
    pub fn swapless(&self) -> bool {
        self.swap_total == 0
    }
}

/// What `/proc/pressure/memory` says — the PSI averages, in percent of the
/// window that tasks spent stalled on memory.
///
/// `some` is any task stalled; `full` is *every* runnable task stalled, which
/// on a box doing work at all is the number that means trouble. The 10-second
/// average is what a dispatch decision wants: a 60-second one still reads high
/// for a minute after the pressure lifted, which would hold the queue closed
/// long after the box recovered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Psi {
    pub some_avg10: f64,
    pub some_avg60: f64,
    pub full_avg10: f64,
}

/// What `/proc/loadavg` says, beside the core count it has to be read against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Load {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
    pub cores: usize,
}

impl Load {
    /// Runnable-or-waiting tasks per core, on the fifteen-minute average — the
    /// only one of the three that says *sustained*. A one-minute spike is a
    /// build starting.
    pub fn per_core(&self) -> f64 {
        if self.cores == 0 {
            return 0.0;
        }
        self.fifteen / self.cores as f64
    }
}

/// The kernel's OOM-kill counters, as two totals that only ever go up.
///
/// Both, because they answer different questions and the difference is the
/// interesting part. `cgroup` is `memory.events`' `oom_kill` for the cgroup the
/// engine is in — every process of the unit the kernel or systemd killed, from
/// a `MemoryMax` breach or from a boxwide shortage. `boxwide` is
/// `/proc/vmstat`'s `oom_kill`, which counts every OOM kill on the machine. A
/// boxwide count that moved while the cgroup's did not is the box killing
/// somebody *else* — still a fact about the night, and not this run's death.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OomCounters {
    pub cgroup: u64,
    pub boxwide: u64,
}

/// Everything one look at the box says.
///
/// Every field is an `Option` and every `None` is "this box does not answer
/// that", never a zero. A missing `/proc/pressure/memory` (PSI compiled out) and
/// a box under no pressure at all are different states, and reporting the first
/// as the second is how a gate stops gating.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Snapshot {
    pub memory: Option<Memory>,
    pub psi: Option<Psi>,
    pub load: Option<Load>,
    pub oom: Option<OomCounters>,
}

impl Snapshot {
    /// Read the box. Cheap — four small pseudo-files, no allocation worth
    /// counting — so callers on the dispatch path may take one per decision
    /// rather than caching a reading that would be stale by the time it is used.
    pub fn read() -> Snapshot {
        Snapshot {
            memory: read_file("/proc/meminfo")
                .as_deref()
                .and_then(parse_meminfo),
            psi: read_file("/proc/pressure/memory")
                .as_deref()
                .and_then(parse_psi),
            load: read_file("/proc/loadavg")
                .as_deref()
                .and_then(|t| parse_loadavg(t, cores())),
            oom: read_oom_counters(),
        }
    }
}

/// How much of the box must still be available before a dispatch may start.
///
/// A fraction of `MemTotal` rather than a byte count so one default fits a 16G
/// VPS and a 64G workstation. See [`crate::config::parse_memory_headroom`] for
/// the spelling an operator writes.
pub const DEFAULT_HEADROOM: f64 = 0.15;

/// The PSI `some avg10` past which the box is already stalling on memory, in
/// percent.
///
/// Twenty: a box spending a fifth of every ten seconds waiting on reclaim is
/// past throttling and into thrashing, and the next allocation is the one that
/// picks a victim. Below that, reclaim is doing its job — a busy build box sits
/// in the single digits all afternoon and must not be treated as full.
///
/// Not configurable, unlike the headroom floor. The floor is about *this* box's
/// size, which only the operator knows; PSI is already normalized to the box.
pub const PSI_STALLING: f64 = 20.0;

/// What a look at the box says about starting one more agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Headroom {
    /// Nothing could be measured — not Linux, or a kernel that does not answer.
    /// Never a refusal: a gate that cannot see must not be the thing that stops
    /// work, and every box comet ran on before this shipped is this state.
    Unknown,
    /// There is room.
    Clear,
    /// There is not, and this is the sentence saying so — the board's own
    /// words, ready to be a `Defer` reason or a dispatch refusal.
    Tight(String),
}

impl Headroom {
    /// The reason, when there is one.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Headroom::Tight(reason) => Some(reason),
            _ => None,
        }
    }
}

/// Is there room on this box for one more agent?
///
/// `floor` is the share of `MemTotal` that must still be available. Two ways to
/// be tight, and both are named in the sentence because they fail differently:
/// **available memory under the floor** is the state a build is about to walk
/// into, and **PSI already over [`PSI_STALLING`]** is the state the box is
/// *in* — reclaim thrashing, every task waiting, and no new allocation that
/// will not make it worse.
///
/// Deliberately not a function of what the queued work *is*: the board cannot
/// know that a task is a Next.js build until the agent runs `pnpm build`. What
/// it can know is what the last three agents left, which is what this measures.
pub fn headroom(snap: &Snapshot, floor: f64) -> Headroom {
    if let Some(psi) = snap.psi
        && psi.some_avg10 > PSI_STALLING
    {
        return Headroom::Tight(format!(
            "the box is stalling on memory — {:.0}% of the last 10s spent waiting on reclaim \
             (PSI some avg10, over {PSI_STALLING:.0}%){}",
            psi.some_avg10,
            match snap.memory {
                Some(mem) => format!(
                    ", {} of {} available",
                    bytes(mem.available),
                    bytes(mem.total)
                ),
                None => String::new(),
            }
        ));
    }
    let Some(mem) = snap.memory else {
        // No memory reading and no PSI reading is a box that said nothing.
        // PSI alone having been read and being calm is a genuine `Clear`.
        return if snap.psi.is_some() {
            Headroom::Clear
        } else {
            Headroom::Unknown
        };
    };
    if mem.available_share() < floor {
        return Headroom::Tight(format!(
            "the box has {} of {} available ({:.0}%, floor {:.0}%){}",
            bytes(mem.available),
            bytes(mem.total),
            mem.available_share() * 100.0,
            floor * 100.0,
            if mem.swapless() {
                " and no swap — the kernel's only answer to a spike here is to kill something"
            } else {
                ""
            }
        ));
    }
    Headroom::Clear
}

// ---- did this run's child die because the box ran out? ---------------------

/// The kernel's OOM-kill counters as they stood when a run started, so its
/// child's death can be asked about afterwards (gh#533).
///
/// Armed rather than sampled-at-death because the counters are cumulative: the
/// unit has been up for weeks and the box has OOM-killed things before, so the
/// only reading that means anything is the *delta* across one run. Arming is
/// two file reads and costs nothing; a run that starts on a box with no
/// counters to read is armed with `None` and will never claim anything.
#[derive(Debug, Clone, Copy, Default)]
pub struct OomWatch {
    at_start: Option<OomCounters>,
}

impl OomWatch {
    /// Take the reading a run starts with.
    pub fn arm() -> OomWatch {
        OomWatch {
            at_start: read_oom_counters(),
        }
    }

    /// Arm with a reading somebody else took — the seam the tests come in
    /// through, and the way a caller that already holds a [`Snapshot`] avoids
    /// reading the same two files twice.
    pub fn armed_with(at_start: Option<OomCounters>) -> OomWatch {
        OomWatch { at_start }
    }

    /// What the counters did while this run was alive, given where they stand
    /// now. `None` when there was nothing to compare — no reading at either
    /// end — or when nothing was killed.
    pub fn since(&self, now: Option<OomCounters>) -> Option<OomEvent> {
        let (before, after) = (self.at_start?, now?);
        let event = OomEvent {
            in_cgroup: after.cgroup.saturating_sub(before.cgroup),
            boxwide: after.boxwide.saturating_sub(before.boxwide),
        };
        (event.in_cgroup > 0 || event.boxwide > 0).then_some(event)
    }

    /// The same question, reading the counters now.
    pub fn kills(&self) -> Option<OomEvent> {
        self.since(read_oom_counters())
    }
}

/// How many OOM kills happened while one run was alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OomEvent {
    /// Killed inside the engine's own cgroup — one of the unit's processes,
    /// which is to say one of its agents.
    pub in_cgroup: u64,
    /// Killed anywhere on the box, this cgroup included.
    pub boxwide: u64,
}

/// The signals a death by OOM can arrive as.
///
/// 9 is the kernel's own OOM killer and systemd-oomd, which is nearly always
/// what this is. 15 is here because the night of 2026-08-19 produced both: with
/// the pre-gh#529 `OOMPolicy=stop`, systemd answered one kernel kill by
/// stopping the unit, and every *other* child of that unit died on a SIGTERM
/// that had nothing to do with its own memory use. Attributing that to the box
/// running out of memory is right — it is why they died.
pub const OOM_SIGNALS: &[i32] = &[9, 15];

/// The sentence a killed child's chat should carry instead of "exited
/// unexpectedly (killed by signal 9)".
///
/// Returns `None` unless both halves hold: the child died on one of
/// [`OOM_SIGNALS`], **and** the kernel's counters moved while it was running.
/// Signal 9 alone proves nothing — a `kill -9` from a shell looks identical —
/// and a board that guessed would tell an operator their box is out of memory
/// every time somebody killed a stuck CLI by hand.
///
/// It is a claim about the box with the evidence in the same sentence, because
/// the alternative — the one that cost gh#526 a night — is a message a person
/// reads as their phone being flaky.
pub fn oom_attribution(
    child: &str,
    signal: i32,
    event: &OomEvent,
    mem: Option<&Memory>,
) -> Option<String> {
    if !OOM_SIGNALS.contains(&signal) {
        return None;
    }
    let mut said = String::from("the box ran out of memory: ");
    if event.in_cgroup > 0 {
        said.push_str(&format!(
            "the kernel OOM-killed {} process{} inside the engine's own cgroup while this run \
             was going, and this run's `{child}` died on signal {signal}",
            event.in_cgroup,
            plural(event.in_cgroup),
        ));
    } else {
        said.push_str(&format!(
            "the kernel OOM-killed {} process{} elsewhere on the box while this run was going, \
             and this run's `{child}` died on signal {signal}",
            event.boxwide,
            plural(event.boxwide),
        ));
    }
    if let Some(mem) = mem {
        said.push_str(&format!(
            ". {} of {} is available now{}",
            bytes(mem.available),
            bytes(mem.total),
            if mem.swapless() {
                ", and the box has no swap"
            } else {
                ""
            }
        ));
    }
    said.push('.');
    Some(said)
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "es" }
}

// ---- parsing (pure — this is the half the tests exercise) ------------------

/// `/proc/meminfo` — `MemTotal:  16316000 kB` and its three siblings. `None`
/// unless `MemTotal` and `MemAvailable` are both there: a kernel old enough to
/// lack `MemAvailable` (pre-3.14) cannot answer the question this asks, and
/// `MemFree` is not a stand-in for it.
pub fn parse_meminfo(text: &str) -> Option<Memory> {
    let mut total = None;
    let mut available = None;
    let mut swap_total = None;
    let mut swap_free = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let slot = match key.trim() {
            "MemTotal" => &mut total,
            "MemAvailable" => &mut available,
            "SwapTotal" => &mut swap_total,
            "SwapFree" => &mut swap_free,
            _ => continue,
        };
        *slot = parse_kb(value);
    }
    Some(Memory {
        total: total?,
        available: available?,
        // A kernel built without swap support omits these entirely, which is a
        // box with no swap — the one case where absence really is zero.
        swap_total: swap_total.unwrap_or(0),
        swap_free: swap_free.unwrap_or(0),
    })
}

/// `12345 kB` → bytes. Anything without the kB unit is ignored rather than
/// guessed at; every field this reads carries it.
fn parse_kb(value: &str) -> Option<u64> {
    let value = value.trim();
    let number = value.strip_suffix("kB").unwrap_or(value).trim();
    number.parse::<u64>().ok().map(|kb| kb * 1024)
}

/// `/proc/pressure/memory`:
///
/// ```text
/// some avg10=0.00 avg60=0.00 avg300=0.00 total=0
/// full avg10=0.00 avg60=0.00 avg300=0.00 total=0
/// ```
///
/// `None` when the `some` line is absent — a kernel without `CONFIG_PSI`
/// serves the file as an error or not at all, and an absent `full` line
/// (cgroup-root on some kernels) reads as 0.0 for the one field, which is the
/// only field this is allowed to assume about.
pub fn parse_psi(text: &str) -> Option<Psi> {
    let field = |line: &str, name: &str| -> Option<f64> {
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(name)?.strip_prefix('='))
            .and_then(|v| v.parse::<f64>().ok())
    };
    let some = text.lines().find(|l| l.starts_with("some "))?;
    let full = text.lines().find(|l| l.starts_with("full "));
    Some(Psi {
        some_avg10: field(some, "avg10")?,
        some_avg60: field(some, "avg60").unwrap_or(0.0),
        full_avg10: full.and_then(|l| field(l, "avg10")).unwrap_or(0.0),
    })
}

/// `/proc/loadavg` — `0.52 0.61 0.58 2/431 12345`.
pub fn parse_loadavg(text: &str, cores: usize) -> Option<Load> {
    let mut parts = text.split_whitespace();
    let mut next = || parts.next().and_then(|v| v.parse::<f64>().ok());
    Some(Load {
        one: next()?,
        five: next()?,
        fifteen: next()?,
        cores,
    })
}

/// A cgroup v2 `memory.events` file's `oom_kill` line.
///
/// `oom_kill` and not `oom`: `oom` counts the times the cgroup went OOM, which
/// includes breaches the kernel resolved by reclaiming rather than killing.
/// What this is about is a process that died.
pub fn parse_memory_events(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("oom_kill ")?.trim().parse::<u64>().ok())
}

/// `/proc/vmstat`'s boxwide `oom_kill` counter.
pub fn parse_vmstat_oom_kill(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("oom_kill ")?.trim().parse::<u64>().ok())
}

/// The cgroup v2 path this process is in, from `/proc/self/cgroup`
/// (`0::/user.slice/user-1000.slice/user@1000.service/app.slice/comet-native.service`).
///
/// The v2 line is the one whose hierarchy id is `0`; a v1-only box has no such
/// line and gets `None`, which is the right answer — `memory.events` is a v2
/// file and there is nothing here to read on a v1 box.
pub fn parse_self_cgroup(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim().to_string())
}

// ---- reading (the half that touches the box) ------------------------------

fn read_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// How many cores the box has, for [`Load::per_core`]. `available_parallelism`
/// respects cgroup CPU quota where the kernel exposes one, which is the number
/// a load average should be read against on a container anyway.
fn cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Both OOM counters, or `None` when neither could be read.
///
/// One half missing is not fatal: a box outside a cgroup v2 unit (a source
/// build in a terminal) still has `/proc/vmstat`, and reporting its boxwide
/// kills with a cgroup count of zero is honest — nothing was killed in a cgroup
/// this process is not in.
pub fn read_oom_counters() -> Option<OomCounters> {
    let cgroup = read_file("/proc/self/cgroup")
        .as_deref()
        .and_then(parse_self_cgroup)
        .and_then(|path| read_file(&format!("/sys/fs/cgroup{path}/memory.events")))
        .as_deref()
        .and_then(parse_memory_events);
    let boxwide = read_file("/proc/vmstat")
        .as_deref()
        .and_then(parse_vmstat_oom_kill);
    if cgroup.is_none() && boxwide.is_none() {
        return None;
    }
    Some(OomCounters {
        cgroup: cgroup.unwrap_or(0),
        boxwide: boxwide.unwrap_or(0),
    })
}

// ---- what the unit's journal remembers ------------------------------------

/// One oom-kill the engine's unit journal recorded.
///
/// The counters above are cumulative and reset when the unit restarts, which is
/// exactly the wrong memory for a box that OOM-killed four agents overnight and
/// was restarted at breakfast. The journal is the record that survives that, and
/// it carries the one thing a counter cannot: *when*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OomJournalEntry {
    /// The timestamp as journalctl printed it (`-o short-iso`), verbatim. Not
    /// parsed into a `DateTime` on purpose: it is evidence to be shown, and a
    /// format this cannot parse would otherwise become an entry silently
    /// dropped from a report about how often something happened.
    pub at: String,
    /// The message, without the timestamp and host prefix.
    pub line: String,
}

/// Pick the oom-kill lines out of `journalctl -o short-iso` output.
///
/// Matched on the word rather than on one systemd version's exact sentence:
/// "A process of this unit has been killed by the OOM killer" (the service
/// manager), "Killed … due to memory pressure" (systemd-oomd) and the kernel's
/// own "Out of memory: Killed process" all say `oom` or `out of memory`
/// somewhere, and a report that missed one because a distribution reworded it
/// would be quiet on the night it is needed.
pub fn parse_oom_journal(text: &str) -> Vec<OomJournalEntry> {
    text.lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("oom") || lower.contains("out of memory")
        })
        .map(|line| {
            // `2026-08-19T23:41:02+0200 host systemd[1]: unit: message`
            let (at, rest) = line.split_once(' ').unwrap_or((line, ""));
            OomJournalEntry {
                at: at.to_string(),
                line: rest
                    .split_once(": ")
                    .map(|(_, m)| m)
                    .unwrap_or(rest)
                    .trim()
                    .to_string(),
            }
        })
        .collect()
}

/// Ask the engine's own user unit what it has been killed for lately.
///
/// `None` when the journal could not be read at all — not Linux, no
/// `journalctl`, no such unit — which is "not checked" and never "nothing
/// happened". Bounded by `--since` so a box with a year of journal answers in
/// the time a report is allowed to take.
pub fn read_oom_journal(unit: &str, days: u32) -> Option<Vec<OomJournalEntry>> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let output = std::process::Command::new("journalctl")
        .args([
            "--user",
            "-u",
            unit,
            "--since",
            &format!("-{days}d"),
            "--no-pager",
            "-o",
            "short-iso",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_oom_journal(&String::from_utf8_lossy(&output.stdout)))
}

/// Bytes as a person reads them. Its own copy rather than [`crate::gc`]'s
/// because that one is about disks and rounds to whole gibibytes past a point;
/// a memory floor is decided in the hundreds of mebibytes.
pub fn bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    let n = n as f64;
    if n >= KIB * KIB * KIB {
        format!("{:.1} GiB", n / (KIB * KIB * KIB))
    } else if n >= KIB * KIB {
        format!("{:.0} MiB", n / (KIB * KIB))
    } else {
        format!("{n:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "\
MemTotal:       16316000 kB
MemFree:          204236 kB
MemAvailable:    1258496 kB
Buffers:            2048 kB
SwapTotal:             0 kB
SwapFree:              0 kB
";

    fn mem(total_gib: f64, available_gib: f64, swap_gib: f64) -> Memory {
        let gib = |g: f64| (g * 1024.0 * 1024.0 * 1024.0) as u64;
        Memory {
            total: gib(total_gib),
            available: gib(available_gib),
            swap_total: gib(swap_gib),
            swap_free: gib(swap_gib),
        }
    }

    fn snap(memory: Option<Memory>, psi: Option<Psi>) -> Snapshot {
        Snapshot {
            memory,
            psi,
            ..Snapshot::default()
        }
    }

    fn psi(some_avg10: f64) -> Psi {
        Psi {
            some_avg10,
            some_avg60: some_avg10,
            full_avg10: 0.0,
        }
    }

    #[test]
    fn meminfo_is_read_in_bytes_and_swaplessness_is_a_fact() {
        let m = parse_meminfo(MEMINFO).unwrap();
        assert_eq!(m.total, 16_316_000 * 1024);
        assert_eq!(m.available, 1_258_496 * 1024);
        assert!(m.swapless());
        // The production box on 2026-08-19: 7.7% left of 15.6 GiB.
        assert!(m.available_share() < 0.08, "{}", m.available_share());
    }

    /// `MemFree` is not `MemAvailable` and must never stand in for it: a kernel
    /// that cannot answer says nothing.
    #[test]
    fn a_kernel_without_memavailable_answers_nothing() {
        assert!(parse_meminfo("MemTotal: 100 kB\nMemFree: 50 kB\n").is_none());
    }

    #[test]
    fn a_kernel_built_without_swap_reads_as_a_box_with_none() {
        let m = parse_meminfo("MemTotal: 1000 kB\nMemAvailable: 500 kB\n").unwrap();
        assert!(m.swapless());
    }

    #[test]
    fn psi_reads_the_ten_second_average_off_both_lines() {
        let p = parse_psi(
            "some avg10=34.21 avg60=12.00 avg300=3.00 total=99\n\
             full avg10=8.10 avg60=2.00 avg300=1.00 total=42\n",
        )
        .unwrap();
        assert_eq!(p.some_avg10, 34.21);
        assert_eq!(p.some_avg60, 12.00);
        assert_eq!(p.full_avg10, 8.10);
        // A kernel with no PSI is a box that said nothing, not a calm one.
        assert!(parse_psi("").is_none());
    }

    #[test]
    fn loadavg_is_read_against_the_core_count() {
        let l = parse_loadavg("18.52 16.10 15.00 7/931 4242\n", 4).unwrap();
        assert_eq!(l.one, 18.52);
        assert_eq!(l.fifteen, 15.00);
        assert_eq!(l.per_core(), 3.75);
    }

    #[test]
    fn the_counter_read_is_oom_kill_and_not_oom() {
        let events = "low 0\nhigh 4211\nmax 12\noom 3\noom_kill 2\n";
        assert_eq!(parse_memory_events(events), Some(2));
        assert_eq!(parse_vmstat_oom_kill("pgfault 12\noom_kill 7\n"), Some(7));
        assert_eq!(parse_memory_events("low 0\nhigh 1\n"), None);
    }

    #[test]
    fn the_v2_cgroup_path_is_the_zero_hierarchy_line() {
        assert_eq!(
            parse_self_cgroup("0::/user.slice/user-1000.slice/comet-native.service\n").as_deref(),
            Some("/user.slice/user-1000.slice/comet-native.service")
        );
        // v1-only: no `0::` line, nothing to read.
        assert!(parse_self_cgroup("11:memory:/user.slice\n1:name=systemd:/\n").is_none());
    }

    // ---- the gate --------------------------------------------------------

    /// A box that cannot be measured must never be the reason work stops.
    #[test]
    fn an_unmeasurable_box_never_refuses_a_dispatch() {
        assert_eq!(
            headroom(&Snapshot::default(), DEFAULT_HEADROOM),
            Headroom::Unknown
        );
    }

    #[test]
    fn a_box_with_room_is_clear() {
        let s = snap(Some(mem(16.0, 9.0, 0.0)), Some(psi(0.4)));
        assert_eq!(headroom(&s, DEFAULT_HEADROOM), Headroom::Clear);
    }

    /// The state the Mylder box was in when the fourth build started.
    #[test]
    fn a_box_under_the_floor_defers_and_says_the_numbers() {
        let s = snap(Some(mem(15.6, 1.2, 0.0)), Some(psi(1.0)));
        let reason = match headroom(&s, DEFAULT_HEADROOM) {
            Headroom::Tight(reason) => reason,
            other => panic!("{other:?}"),
        };
        assert!(reason.contains("1.2 GiB of 15.6 GiB available"), "{reason}");
        assert!(reason.contains("floor 15%"), "{reason}");
        assert!(reason.contains("no swap"), "{reason}");
    }

    /// Swap is slack: the same shortfall on a box with a swapfile does not get
    /// the sentence that says the kernel's only answer is to kill something.
    #[test]
    fn a_box_with_swap_is_told_off_without_the_no_swap_clause() {
        let s = snap(Some(mem(15.6, 1.2, 4.0)), None);
        let reason = headroom(&s, DEFAULT_HEADROOM).reason().unwrap().to_string();
        assert!(!reason.contains("no swap"), "{reason}");
    }

    /// A box thrashing on reclaim is full whatever `MemAvailable` claims —
    /// available memory looks fine right up to the moment reclaim stops keeping
    /// up, which is what PSI is for.
    #[test]
    fn a_stalling_box_is_tight_even_with_memory_apparently_free() {
        let s = snap(Some(mem(16.0, 8.0, 0.0)), Some(psi(41.0)));
        let reason = headroom(&s, DEFAULT_HEADROOM).reason().unwrap().to_string();
        assert!(reason.contains("stalling on memory"), "{reason}");
        assert!(reason.contains("41%"), "{reason}");
    }

    #[test]
    fn a_calm_psi_reading_alone_is_enough_to_clear() {
        assert_eq!(
            headroom(&snap(None, Some(psi(0.0))), DEFAULT_HEADROOM),
            Headroom::Clear
        );
    }

    // ---- the attribution -------------------------------------------------

    fn counters(cgroup: u64, boxwide: u64) -> Option<OomCounters> {
        Some(OomCounters { cgroup, boxwide })
    }

    #[test]
    fn a_run_that_saw_no_kills_claims_nothing() {
        let watch = OomWatch::armed_with(counters(4, 11));
        assert!(watch.since(counters(4, 11)).is_none());
    }

    #[test]
    fn a_kill_inside_the_unit_is_counted_across_the_run() {
        let watch = OomWatch::armed_with(counters(4, 11));
        let event = watch.since(counters(5, 12)).unwrap();
        assert_eq!(event.in_cgroup, 1);
        assert_eq!(event.boxwide, 1);
    }

    /// Counters that could not be read at either end prove nothing — the
    /// board's rule everywhere else, applied here.
    #[test]
    fn an_unreadable_counter_claims_nothing() {
        assert!(OomWatch::armed_with(None).since(counters(9, 9)).is_none());
        assert!(OomWatch::armed_with(counters(0, 0)).since(None).is_none());
    }

    /// The message gh#526 needed: "the box ran out of memory", with the
    /// evidence in the same sentence.
    #[test]
    fn the_chat_is_told_the_box_ran_out_of_memory() {
        let event = OomEvent {
            in_cgroup: 1,
            boxwide: 1,
        };
        let said = oom_attribution("claude", 9, &event, Some(&mem(15.6, 0.4, 0.0))).unwrap();
        assert!(said.starts_with("the box ran out of memory: "), "{said}");
        assert!(said.contains("inside the engine's own cgroup"), "{said}");
        assert!(said.contains("`claude` died on signal 9"), "{said}");
        assert!(said.contains("no swap"), "{said}");
    }

    /// The pre-gh#529 shape: the kernel killed one child, systemd stopped the
    /// unit, and its siblings died on SIGTERM. They died of the box running out
    /// of memory too.
    #[test]
    fn a_sibling_killed_by_the_units_own_stop_is_attributed_to_the_box() {
        let event = OomEvent {
            in_cgroup: 1,
            boxwide: 1,
        };
        assert!(oom_attribution("codex", 15, &event, None).is_some());
    }

    /// A kill from somewhere else on the box is said differently: it is still
    /// why this child died, and it was not this unit's cap that did it.
    #[test]
    fn a_kill_outside_the_cgroup_is_named_as_being_outside_it() {
        let event = OomEvent {
            in_cgroup: 0,
            boxwide: 2,
        };
        let said = oom_attribution("claude", 9, &event, None).unwrap();
        assert!(said.contains("elsewhere on the box"), "{said}");
        assert!(said.contains("2 processes"), "{said}");
    }

    // ---- the journal -----------------------------------------------------

    /// The night of 2026-08-19, as the unit journal has it. Four kills, and the
    /// engine stopping after each one — which is the half gh#529 fixed.
    #[test]
    fn the_journal_yields_every_oom_line_with_its_timestamp() {
        let journal = "\
2026-08-19T22:14:03+0200 mylder systemd[1401]: comet-native.service: Started.
2026-08-19T23:41:02+0200 mylder systemd[1401]: comet-native.service: A process of this unit has been killed by the OOM killer.
2026-08-19T23:41:02+0200 mylder systemd[1401]: comet-native.service: Failed with result 'oom-kill'.
2026-08-20T00:12:44+0200 mylder systemd-oomd[900]: Killed /user.slice/comet-native.service due to memory pressure
";
        let found = parse_oom_journal(journal);
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(found[0].at, "2026-08-19T23:41:02+0200");
        assert!(
            found[0].line.contains("killed by the OOM killer"),
            "{:?}",
            found[0]
        );
        // systemd-oomd words it differently and must still be found.
        assert!(found[2].line.contains("memory pressure"), "{:?}", found[2]);
    }

    #[test]
    fn a_quiet_journal_yields_nothing() {
        assert!(
            parse_oom_journal("2026-08-20T09:00:00+0200 h systemd[1]: x: Started.\n").is_empty()
        );
    }

    /// `kill -9` from a shell looks exactly like an OOM kill from the signal
    /// alone. Only the counters may decide, and a normal exit never can.
    #[test]
    fn an_ordinary_exit_is_never_called_an_oom() {
        let event = OomEvent {
            in_cgroup: 1,
            boxwide: 1,
        };
        assert!(oom_attribution("claude", 1, &event, None).is_none());
        assert!(oom_attribution("claude", 11, &event, None).is_none());
    }
}
