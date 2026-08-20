//! The engine's systemd unit as a thing that has to keep being true (gh#533).
//!
//! gh#529 put three lines into the unit — `OOMPolicy=continue`, `MemoryHigh`,
//! `MemoryMax` — and they are what stops one OOM-killed agent taking the engine,
//! every warm session and the pinned dispatcher chat with it. But a unit file is
//! written *once*, at install: every box installed before that release kept the
//! unit it already had, and an update that only swaps the binary leaves it
//! there. The production box got these lines by hand on 2026-08-20, which is not
//! a distribution mechanism.
//!
//! So they ship as a **drop-in** instead, re-asserted on every update. A drop-in
//! rather than a re-render because the rendered unit carries the environment
//! captured at install time and the `ExecStart` of whichever installer wrote it;
//! regenerating that from an updater — which knows neither — would replace a
//! working unit with a plausible one. A drop-in overrides exactly the three keys
//! it names and leaves everything else the operator's.
//!
//! Beside [`crate::restart_service`] because it is the same subject at the same
//! moment: what the updater does to the service after it has swapped the binary
//! under it. `comet daemon install` renders the same lines into the unit it
//! writes, and `comet-board doctor` reads back what systemd *actually* has —
//! the drop-in, the unit, and any override an operator added — through
//! [`effective_governance`].

use std::path::{Path, PathBuf};

/// The unit both installers write and this drop-in extends.
pub const UNIT: &str = "comet-native.service";

/// The `[Service]` keys that scope an OOM kill to the process the kernel chose
/// (gh#529), as they appear in a unit file or a drop-in.
///
/// - `OOMPolicy=continue` — the default is `stop`, which answers any oom-kill
///   inside the unit's cgroup by stopping the whole service. That is what turned
///   one overweight agent build into a dead engine four times on 2026-08-19.
/// - `MemoryHigh=75%` — the cgroup throttles into reclaim before the kernel
///   acts, and the throttling lands on the big allocators, which are the agent
///   builds rather than the engine.
/// - `MemoryMax=90%` — past that the kill happens inside this cgroup rather
///   than boxwide, so the kernel is choosing among the unit's own processes.
///
/// Percentages, so one line fits a 16G VPS and a 64G box. No negative
/// `OOMScoreAdjust`: a user manager lacks `CAP_SYS_RESOURCE` to lower scores,
/// and a setting systemd cannot apply fails the start.
pub const RESOURCE_GOVERNANCE: &str = "OOMPolicy=continue\nMemoryHigh=75%\nMemoryMax=90%\n";

/// The drop-in file, whole. `50-` so it sorts after a distributor's drop-in and
/// before an operator's `90-`: this is a default worth re-asserting, not a
/// decision that should win against somebody who deliberately set otherwise.
pub fn resource_dropin() -> String {
    format!(
        "# Written by comet (gh#529, gh#533) — re-asserted on every engine update.\n\
         # An OOM-killed agent child is an event, not an engine death.\n\
         # Remove this file and `systemctl --user daemon-reload` to opt out.\n\
         [Service]\n{RESOURCE_GOVERNANCE}"
    )
}

/// Where it goes, under a given config dir.
pub fn resource_dropin_path_in(config_dir: &Path) -> PathBuf {
    config_dir
        .join("systemd/user")
        .join(format!("{UNIT}.d"))
        .join("50-comet-resources.conf")
}

/// Where it goes on this box — `$XDG_CONFIG_HOME`, else `~/.config`, the same
/// pair `comet daemon` resolves the unit path with.
pub fn resource_dropin_path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(resource_dropin_path_in(&config))
}

/// What [`ensure_resource_governance`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropIn {
    /// Written or rewritten — the caller owes systemd a `daemon-reload`.
    Written,
    /// Already exactly right; nothing was touched.
    Unchanged,
    /// Not a systemd box, or no config dir to write into. Not a failure.
    NotApplicable,
}

/// Put the drop-in where systemd will read it, if it is not already there.
///
/// Idempotent and content-addressed: a box whose drop-in already matches is
/// left alone, so an update on an up-to-date box does not churn the file or
/// trigger a reload. Writing it does **not** apply it — the caller reloads and
/// restarts, which on the update path it was going to do anyway.
///
/// Deliberately does not remove an operator's own override. If somebody wrote
/// `90-my-limits.conf` saying something else, they meant it, and a `50-` file
/// loses to it by sort order.
pub fn ensure_resource_governance() -> std::io::Result<DropIn> {
    if !cfg!(target_os = "linux") {
        return Ok(DropIn::NotApplicable);
    }
    let Some(path) = resource_dropin_path() else {
        return Ok(DropIn::NotApplicable);
    };
    let wanted = resource_dropin();
    if std::fs::read_to_string(&path).is_ok_and(|have| have == wanted) {
        return Ok(DropIn::Unchanged);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, wanted)?;
    Ok(DropIn::Written)
}

/// What systemd has for the unit right now — the merged answer over the unit
/// file, every drop-in, and anything set at runtime.
///
/// `None` when the question cannot be asked at all: not Linux, no `systemctl`,
/// or a call that failed. Never a guessed answer — a box whose service manager
/// could not be reached knows less about itself, not worse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Governance {
    /// `loaded`, `not-found`, `masked` — systemd's `LoadState`. A box running
    /// the engine from a terminal has no unit and is not misconfigured.
    pub load_state: String,
    /// `stop` (systemd's default) or `continue` (what gh#529 needs).
    pub oom_policy: String,
    /// Bytes, or `infinity` when unset — `systemctl show` resolves the
    /// percentage the unit file writes against the box's memory.
    pub memory_high: String,
    pub memory_max: String,
}

impl Governance {
    /// Is there a unit here to have an opinion about?
    pub fn loaded(&self) -> bool {
        self.load_state == "loaded"
    }

    /// The one setting whose absence is the gh#529 failure itself: with
    /// `OOMPolicy=stop`, systemd answers a kill inside the cgroup by stopping
    /// the engine.
    pub fn survives_an_oom_kill(&self) -> bool {
        self.oom_policy == "continue"
    }

    /// Does the cgroup throttle before the kernel picks a victim? Either limit
    /// being `infinity` is that half unset.
    pub fn throttles(&self) -> bool {
        self.memory_high != "infinity" && !self.memory_high.is_empty()
    }

    pub fn capped(&self) -> bool {
        self.memory_max != "infinity" && !self.memory_max.is_empty()
    }

    /// Everything gh#529 asked for, in place.
    pub fn complete(&self) -> bool {
        self.survives_an_oom_kill() && self.throttles() && self.capped()
    }
}

/// Read the four properties off `systemctl --user show`.
pub fn effective_governance() -> Option<Governance> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let output = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            UNIT,
            "--property=LoadState",
            "--property=OOMPolicy",
            "--property=MemoryHigh",
            "--property=MemoryMax",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_governance(&String::from_utf8_lossy(&output.stdout))
}

/// `systemctl show`'s `Key=value` lines, in any order. `None` unless
/// `LoadState` is among them — anything less is an answer that cannot be read.
pub fn parse_governance(text: &str) -> Option<Governance> {
    let value = |key: &str| -> Option<String> {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().to_string())
    };
    Some(Governance {
        load_state: value("LoadState")?,
        // A systemd too old to know a property prints it empty rather than
        // omitting it; either way, unknown is not `continue`.
        oom_policy: value("OOMPolicy").unwrap_or_default(),
        memory_high: value("MemoryHigh").unwrap_or_default(),
        memory_max: value("MemoryMax").unwrap_or_default(),
    })
}

/// The commands an operator runs to put this on a box by hand — what a `doctor`
/// failure prints, and what the production box was walked through on
/// 2026-08-20.
///
/// A whole heredoc rather than "add three lines to your unit": the box where
/// this fails is a headless VPS reached over ssh at an hour nobody wants to be
/// editing unit files, and a command that can be pasted is the difference
/// between fixed tonight and fixed eventually.
pub fn resource_dropin_fix() -> String {
    format!(
        "mkdir -p ~/.config/systemd/user/{UNIT}.d && \
         cat > ~/.config/systemd/user/{UNIT}.d/50-comet-resources.conf <<'EOF'\n\
         {}EOF\n\
         systemctl --user daemon-reload && systemctl --user restart {UNIT}",
        resource_dropin()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dropin_carries_all_three_keys_under_a_service_section() {
        let text = resource_dropin();
        assert!(text.contains("[Service]\n"), "{text}");
        assert!(text.contains("OOMPolicy=continue\n"), "{text}");
        assert!(text.contains("MemoryHigh=75%\n"), "{text}");
        assert!(text.contains("MemoryMax=90%\n"), "{text}");
        // Says how to opt out, in the file itself — the operator finds it
        // there, not in a release note.
        assert!(text.contains("Remove this file"), "{text}");
    }

    #[test]
    fn the_dropin_sorts_after_a_distributors_and_before_an_operators() {
        let path = resource_dropin_path_in(Path::new("/home/u/.config"));
        assert_eq!(
            path,
            Path::new(
                "/home/u/.config/systemd/user/comet-native.service.d/50-comet-resources.conf"
            )
        );
    }

    #[test]
    fn a_configured_unit_reads_as_complete() {
        let g = parse_governance(
            "LoadState=loaded\nOOMPolicy=continue\nMemoryHigh=12252659712\n\
             MemoryMax=14703191654\n",
        )
        .unwrap();
        assert!(g.loaded());
        assert!(g.complete());
    }

    /// The state every box installed before gh#529 is in, and the one the
    /// production box was in on the night of 2026-08-19.
    #[test]
    fn the_default_unit_is_the_gh529_failure() {
        let g = parse_governance(
            "LoadState=loaded\nOOMPolicy=stop\nMemoryHigh=infinity\nMemoryMax=infinity\n",
        )
        .unwrap();
        assert!(!g.survives_an_oom_kill());
        assert!(!g.throttles());
        assert!(!g.capped());
        assert!(!g.complete());
    }

    /// A box with no unit at all — a source build in a terminal — is not a
    /// misconfigured one.
    #[test]
    fn a_box_with_no_unit_is_not_a_broken_box() {
        let g =
            parse_governance("LoadState=not-found\nOOMPolicy=\nMemoryHigh=\nMemoryMax=\n").unwrap();
        assert!(!g.loaded());
    }

    /// An answer with no `LoadState` in it is not an answer.
    #[test]
    fn an_unreadable_answer_is_none() {
        assert!(parse_governance("").is_none());
        assert!(parse_governance("OOMPolicy=continue\n").is_none());
    }

    /// The paste has to be the whole fix: make the directory, write the file,
    /// reload, restart.
    #[test]
    fn the_printed_fix_is_runnable_end_to_end() {
        let fix = resource_dropin_fix();
        assert!(fix.contains("mkdir -p"), "{fix}");
        assert!(fix.contains("OOMPolicy=continue"), "{fix}");
        assert!(fix.contains("daemon-reload"), "{fix}");
        assert!(fix.contains("restart comet-native.service"), "{fix}");
    }
}
