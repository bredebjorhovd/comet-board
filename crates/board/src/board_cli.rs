//! Which `comet-board` this box will actually run, and whether it is the one
//! the engine beside it shipped with (gh#156).
//!
//! `doctor` answers this from the inside — it *is* the CLI, so it reads its own
//! `CARGO_PKG_VERSION`. That answer is worthless on the one machine that has the
//! problem: a CLI old enough to have drifted is old enough not to carry the
//! check, so the box goes on reporting a clean board with a three-week-old
//! binary driving it. The drift has to be visible to code that *is* current, and
//! on this box the current code is the engine.
//!
//! So this probes from the outside: find the binary, run `--version`, compare.
//! Used by `comet status` and by the engine's own boot log — both of which the
//! release upgrades on every install, and neither of which depends on the thing
//! it is reporting on.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::git_credentials::resolve_board_exe;

/// What the `comet-board` on this box turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliState {
    /// Nothing named `comet-board` beside the engine, on PATH, or under the
    /// app root. Not a failure by itself — a machine that only runs the engine
    /// has no use for the CLI.
    Missing,
    /// It answered, and this is what it said.
    Found { exe: PathBuf, version: String },
    /// It is there and it runs, but it does not understand `--version`. That is
    /// an answer and not an error: the flag ships with the first release that
    /// carries this binary at all, so its absence dates the copy to before
    /// that, which is precisely the state worth naming.
    PredatesVersionFlag { exe: PathBuf },
    /// Found and could not be run — wrong architecture, not executable, a
    /// dangling symlink resolved to nothing.
    Unusable { exe: PathBuf, why: String },
}

impl CliState {
    /// The path involved, when there is one.
    pub fn exe(&self) -> Option<&Path> {
        match self {
            CliState::Missing => None,
            CliState::Found { exe, .. }
            | CliState::PredatesVersionFlag { exe }
            | CliState::Unusable { exe, .. } => Some(exe),
        }
    }

    /// Is this CLI in step with `engine_version`?
    ///
    /// `Missing` is not drift — there is nothing to be out of step. Everything
    /// else that is not an exact match is, including a copy too old to say:
    /// "cannot tell you" from a binary that would answer if it were current is
    /// the same news as a wrong number.
    pub fn drifted(&self, engine_version: &str) -> bool {
        match self {
            CliState::Missing => false,
            CliState::Found { version, .. } => version != engine_version,
            CliState::PredatesVersionFlag { .. } | CliState::Unusable { .. } => true,
        }
    }

    /// One line for a status report: `(ok, text)`.
    pub fn line(&self, engine_version: &str) -> (bool, String) {
        match self {
            CliState::Missing => (
                true,
                "not installed on this box (the engine does not need it)".into(),
            ),
            CliState::Found { exe, version } if version == engine_version => {
                (true, format!("v{version}, same as this engine · {}", shown(exe)))
            }
            CliState::Found { exe, version } => (
                false,
                format!(
                    "v{version}, but this engine is v{engine_version} — {} is not what \
                     this release installed, and a verb it does not have does not exist \
                     on this box. {FIX}",
                    shown(exe)
                ),
            ),
            CliState::PredatesVersionFlag { exe } => (
                false,
                format!(
                    "{} predates `--version`, so it is older than any release that \
                     ships comet-board — this engine is v{engine_version}. {FIX}",
                    shown(exe)
                ),
            ),
            CliState::Unusable { exe, why } => (
                false,
                format!("{} could not be run ({why}). {FIX}", shown(exe)),
            ),
        }
    }
}

/// Said the same way everywhere, because it is the same command.
const FIX: &str = "Re-run the installer to put them back in step: \
                   curl -fsSL https://edge.comet.offhand.dev/install.sh | sh";

fn shown(p: &Path) -> String {
    crate::config::shorten_home(p)
}

/// Probe the `comet-board` this box would run.
pub fn probe() -> CliState {
    probe_with(resolve_board_exe(), run_version)
}

/// The testable half: the lookup and the subprocess are both injected, because
/// every interesting state here is one this machine is not currently in.
pub fn probe_with<F>(exe: Option<PathBuf>, run: F) -> CliState
where
    F: FnOnce(&Path) -> std::io::Result<std::process::Output>,
{
    let Some(exe) = exe else {
        return CliState::Missing;
    };
    match run(&exe) {
        Err(e) => CliState::Unusable {
            exe,
            why: e.to_string(),
        },
        Ok(out) if out.status.success() => match parse_version(&String::from_utf8_lossy(&out.stdout))
        {
            Some(version) => CliState::Found { exe, version },
            // Exited 0 and said nothing a version could be read out of. Treated
            // as "too old" rather than invented: some other binary is answering
            // to the name, and that is at least as wrong as a stale one.
            None => CliState::PredatesVersionFlag { exe },
        },
        // clap exits 2 on an unknown flag. Anything non-zero means it did not
        // understand the question.
        Ok(_) => CliState::PredatesVersionFlag { exe },
    }
}

fn run_version(exe: &Path) -> std::io::Result<std::process::Output> {
    Command::new(exe).arg("--version").output()
}

/// `comet-board 0.3.4` → `0.3.4`. clap's format, and nothing else is accepted:
/// a number scraped out of arbitrary output would be a guess presented as a
/// fact.
fn parse_version(stdout: &str) -> Option<String> {
    let line = stdout.lines().next()?.trim();
    let rest = line.strip_prefix("comet-board")?.trim();
    let version = rest.split_whitespace().next()?;
    version
        .starts_with(|c: char| c.is_ascii_digit())
        .then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::{ExitStatus, Output};

    fn out(code: i32, stdout: &str) -> std::io::Result<Output> {
        Ok(Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    fn exe() -> PathBuf {
        PathBuf::from("/home/comet/.local/bin/comet-board")
    }

    #[test]
    fn a_current_cli_reports_its_version() {
        let s = probe_with(Some(exe()), |_| out(0, "comet-board 0.3.5\n"));
        assert_eq!(
            s,
            CliState::Found {
                exe: exe(),
                version: "0.3.5".into()
            }
        );
        assert!(!s.drifted("0.3.5"));
        assert!(s.line("0.3.5").0);
    }

    /// The gh#156 box: a source build old enough that `--version` is not a flag
    /// it knows. clap exits 2 and says so on stderr.
    #[test]
    fn a_cli_without_the_version_flag_is_dated_by_that_alone() {
        let s = probe_with(Some(exe()), |_| out(2, ""));
        assert_eq!(s, CliState::PredatesVersionFlag { exe: exe() });
        assert!(s.drifted("0.3.5"));
        let (ok, line) = s.line("0.3.5");
        assert!(!ok);
        assert!(line.contains("predates `--version`"), "{line}");
        assert!(line.contains("install.sh"), "{line}");
    }

    #[test]
    fn a_cli_a_version_behind_is_drift() {
        let s = probe_with(Some(exe()), |_| out(0, "comet-board 0.2.9\n"));
        assert!(s.drifted("0.3.5"));
        let (ok, line) = s.line("0.3.5");
        assert!(!ok);
        assert!(line.contains("v0.2.9"), "{line}");
        assert!(line.contains("v0.3.5"), "{line}");
    }

    /// No CLI at all is not a problem to report: plenty of machines run the
    /// engine and never drive a board.
    #[test]
    fn no_cli_at_all_is_not_drift() {
        let s = probe_with(None, |_| out(0, ""));
        assert_eq!(s, CliState::Missing);
        assert!(!s.drifted("0.3.5"));
        assert!(s.line("0.3.5").0);
    }

    #[test]
    fn a_binary_that_cannot_be_run_is_reported_not_swallowed() {
        let s = probe_with(Some(exe()), |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "permission denied",
            ))
        });
        assert!(s.drifted("0.3.5"));
        let (ok, line) = s.line("0.3.5");
        assert!(!ok);
        assert!(line.contains("permission denied"), "{line}");
    }

    #[test]
    fn only_claps_own_format_is_read_as_a_version() {
        assert_eq!(parse_version("comet-board 0.3.4\n").as_deref(), Some("0.3.4"));
        assert_eq!(parse_version("comet-board 0.3.4-rc1").as_deref(), Some("0.3.4-rc1"));
        // Some other program answering to the name, or a usage dump: no answer
        // beats a guessed one.
        assert_eq!(parse_version("comet-board"), None);
        assert_eq!(parse_version("Usage: comet-board <COMMAND>"), None);
        assert_eq!(parse_version("comet-board vNext"), None);
        assert_eq!(parse_version(""), None);
    }
}
