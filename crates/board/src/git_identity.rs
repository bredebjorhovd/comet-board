//! Who a dispatched agent's commits are by (gh#107).
//!
//! [`git_credentials`](crate::git_credentials) answers *what may push*. This
//! answers *whose commits those are*, which is a different question with its
//! own failure: a box with no `git config user.*` at all. Git does not stop
//! there — it invents an identity from the box user and hostname, or takes
//! whatever the harness's own setup wrote — and the first dispatched agent
//! commits under an address belonging to no GitHub account. Nothing rejects it;
//! the commits simply attribute to nobody, and a contributor gate on the deploy
//! side (Vercel's, in the case that produced this module) refuses to build the
//! push.
//!
//! Two halves, matching the two questions an operator has:
//!
//! - **The box's own identity** — [`box_identity`], which `doctor` reports. A
//!   box with none is the anonymous case above; a box whose email is a GitHub
//!   *noreply* address (`<id>+<login>@users.noreply.github.com`) is the one
//!   form that is attributable by construction, because GitHub mints it and no
//!   other account can hold it.
//! - **Per dispatch** — [`author_env`], from the `[users]` map in
//!   `routing.toml`. The attempt already records who released it (gh#74); with
//!   an address for that person, the engine stamps `GIT_AUTHOR_*` on the
//!   harness child so their commits read as theirs on GitHub. The box's pinned
//!   identity is left alone and becomes the *committer* — which is the honest
//!   split: the teammate wrote it, this box committed it.
//!
//! Nothing here is a credential or a check. A commit's author is a claim
//! anybody can write, and the board is not in the business of proving it — the
//! board's App cannot read `GET /user/emails` for a person, so `doctor` offers
//! guidance where it cannot verify.

use std::path::Path;
use std::process::Command;

use comet_proto::GitAuthor;

/// The suffix of an address GitHub itself minted, and therefore the one form
/// that is attributable without asking anybody: `<id>+<login>@users.noreply.github.com`.
pub const NOREPLY_SUFFIX: &str = "@users.noreply.github.com";

/// Is this an address GitHub minted for an account?
pub fn is_github_noreply(email: &str) -> bool {
    email.trim().to_ascii_lowercase().ends_with(NOREPLY_SUFFIX)
}

/// The login inside a noreply address — `22494697+ana@users.noreply.github.com`
/// → `ana`. Also accepts the older loginless form (`ana@users.noreply.github.com`).
///
/// Used to name an author whose config entry gave only an address: the login is
/// a better display name than the raw local part, and it is the name the commit
/// will show beside the avatar anyway.
pub fn noreply_login(email: &str) -> Option<&str> {
    let local = email.trim().split('@').next()?;
    let login = local.rsplit('+').next()?;
    (!login.is_empty() && is_github_noreply(email)).then_some(login)
}

/// Parse a `[users]` value into an author.
///
/// Both spellings people reach for are accepted, because both are unambiguous:
/// a bare address (`22494697+ana@users.noreply.github.com`), or the RFC-ish
/// form git itself prints (`Ana Ruiz <22494697+ana@users.noreply.github.com>`).
/// With no name given, the noreply login is the name — GitHub shows the account
/// either way, and a raw local part like `22494697+ana` is nobody's name.
///
/// `None` for anything without an `@` in it: a value that is not an address is
/// a typo, and stamping it onto `GIT_AUTHOR_EMAIL` would produce exactly the
/// unattributable commits this exists to prevent.
pub fn parse_author(value: &str) -> Option<GitAuthor> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (name, email) = match value.split_once('<') {
        Some((name, rest)) => (name.trim(), rest.trim_end_matches('>').trim()),
        None => ("", value),
    };
    if !plausible_email(email) {
        return None;
    }
    let name = if name.is_empty() {
        noreply_login(email)
            .unwrap_or_else(|| email.split('@').next().unwrap_or(email))
            .to_string()
    } else {
        name.to_string()
    };
    Some(GitAuthor {
        name,
        email: email.to_string(),
    })
}

/// One `@`, something on each side, no whitespace. Not validation — the address
/// is the operator's to get right — only enough to tell an address from a name
/// somebody typed into the wrong half of the map.
fn plausible_email(s: &str) -> bool {
    let mut parts = s.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !s.chars().any(char::is_whitespace)
}

/// The environment that makes a dispatched agent's commits read as `author`.
///
/// Author only, deliberately. The committer stays whatever the box is pinned to
/// (see [`box_identity`]), so `git log --format=%an %cn` on a teammate's branch
/// says "the teammate, committed by the box" — which is what actually happened,
/// and what GitHub renders as *authored by them*.
pub fn author_env(author: &GitAuthor) -> Vec<(String, String)> {
    vec![
        ("GIT_AUTHOR_NAME".into(), author.name.clone()),
        ("GIT_AUTHOR_EMAIL".into(), author.email.clone()),
    ]
}

/// What `git` on this box will sign a commit with, when nothing overrides it.
///
/// Both halves are optional because that is the state this module exists for: a
/// fresh box has neither, and git fills the gap by guessing rather than by
/// refusing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoxIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl BoxIdentity {
    /// Is anything configured at all? The anonymous box is `false` on this.
    pub fn configured(&self) -> bool {
        self.name.is_some() && self.email.is_some()
    }
}

/// Read the box's git identity, from a directory that is not one of its repos.
///
/// `dir` is deliberately somewhere neutral (the board's own config directory):
/// asking inside a checkout would report that repo's local override, and the
/// question is what a *fresh* worktree — every attempt cuts one — inherits. A
/// directory that does not exist yet is not an answer about git, so the ask
/// falls back to the temp dir rather than reporting an unconfigured box.
pub fn box_identity(dir: &Path) -> BoxIdentity {
    let dir = if dir.is_dir() {
        dir.to_path_buf()
    } else {
        std::env::temp_dir()
    };
    BoxIdentity {
        name: git_config(&dir, "user.name"),
        email: git_config(&dir, "user.email"),
    }
}

fn git_config(dir: &Path, key: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_noreply_address_becomes_an_author_named_after_the_login() {
        let a = parse_author("22494697+bredebjorhovd@users.noreply.github.com").unwrap();
        assert_eq!(a.name, "bredebjorhovd");
        assert_eq!(a.email, "22494697+bredebjorhovd@users.noreply.github.com");
        // The loginless form GitHub minted for older accounts still resolves.
        assert_eq!(
            parse_author("ana@users.noreply.github.com").unwrap().name,
            "ana"
        );
        // A non-GitHub address has no login to read; the local part is all
        // there is, and it is better than an empty name.
        assert_eq!(parse_author("ana@example.com").unwrap().name, "ana");
    }

    #[test]
    fn the_spelling_git_itself_prints_is_accepted() {
        let a = parse_author("Ana Ruiz <22494697+ana@users.noreply.github.com>").unwrap();
        assert_eq!(a.name, "Ana Ruiz");
        assert_eq!(a.email, "22494697+ana@users.noreply.github.com");
    }

    /// A value that is not an address must not reach `GIT_AUTHOR_EMAIL`: it
    /// would produce exactly the unattributable commits gh#107 is about, and
    /// silently. `RoutingConfig::problems` reports these as config errors.
    #[test]
    fn something_that_is_not_an_address_is_refused_rather_than_stamped() {
        for bad in [
            "",
            "   ",
            "bredebjorhovd",
            "Brede <>",
            "two@at@signs.com",
            "no domain@localhost",
            "spaces in@example.com",
        ] {
            assert!(parse_author(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn only_github_minted_addresses_read_as_noreply() {
        assert!(is_github_noreply("22494697+ana@users.noreply.github.com"));
        assert!(is_github_noreply("ANA@USERS.NOREPLY.GITHUB.COM"));
        assert!(!is_github_noreply("ana@example.com"));
        // The near-miss: a domain that merely ends in the right letters is a
        // domain somebody else owns.
        assert!(!is_github_noreply("ana@evil-users.noreply.github.com.test"));
    }

    /// Author, and only author. The committer is the box's pinned identity —
    /// the split that keeps "who wrote this" honest without pretending the
    /// teammate's laptop ran the commit.
    #[test]
    fn the_environment_names_the_author_and_leaves_the_committer_alone() {
        let env: std::collections::BTreeMap<_, _> = author_env(&GitAuthor {
            name: "Ana Ruiz".into(),
            email: "22494697+ana@users.noreply.github.com".into(),
        })
        .into_iter()
        .collect();
        assert_eq!(
            env.get("GIT_AUTHOR_NAME").map(String::as_str),
            Some("Ana Ruiz")
        );
        assert_eq!(
            env.get("GIT_AUTHOR_EMAIL").map(String::as_str),
            Some("22494697+ana@users.noreply.github.com")
        );
        assert!(
            !env.keys().any(|k| k.starts_with("GIT_COMMITTER")),
            "the box commits: {env:?}"
        );
    }

    /// The check `doctor` runs. A directory with a git config of its own is the
    /// only way to test this without touching the box's real one.
    #[test]
    fn the_box_identity_is_read_from_git_itself() {
        let dir = tempfile::tempdir().unwrap();
        // An empty HOME-less read: nothing configured is the fresh-box state.
        let bare = BoxIdentity::default();
        assert!(!bare.configured());

        let repo = dir.path();
        assert!(
            Command::new("git")
                .args(["-C", &repo.to_string_lossy(), "init", "-b", "main"])
                .output()
                .unwrap()
                .status
                .success()
        );
        for (k, v) in [
            ("user.name", "The Box"),
            ("user.email", "1+box@users.noreply.github.com"),
        ] {
            Command::new("git")
                .args(["-C", &repo.to_string_lossy(), "config", k, v])
                .output()
                .unwrap();
        }
        let id = box_identity(repo);
        assert!(id.configured());
        assert_eq!(id.name.as_deref(), Some("The Box"));
        assert_eq!(id.email.as_deref(), Some("1+box@users.noreply.github.com"));
    }
}
