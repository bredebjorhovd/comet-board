//! Who else drives this board, and what the box owes them (gh#162).
//!
//! Every mechanism for a second person shipped separately — org visibility
//! (gh#66), invitations (gh#76), per-run agent accounts (gh#59), per-dispatch
//! authorship (gh#107) — and what an operator must actually *do* survived only
//! as three unrelated `doctor` lines, each discovered after the fact. This
//! module is the writer for the one of those three that was a hand-edit:
//! [`crate::git_identity`] parses the `[users]` map, and nothing wrote it.
//!
//! Two answers, matching the two questions:
//!
//! - **`member add`** — [`parse_github`] and [`resolve_login`] turn what
//!   somebody types (`--github ana`, or the noreply address pasted from
//!   <https://github.com/settings/emails>) into the one value `[users]` should
//!   hold, and [`users_value`] spells it. The write itself is
//!   [`crate::routes::Edit::User`], so it lands through the same parse +
//!   validate + `.bak` discipline as every other config edit.
//! - **`member list`** — [`roster`], which is the pairing nobody thinks about:
//!   a mapped teammate whose box has no agent-account slot for them commits
//!   under their own name and spends the box owner's subscription. The two
//!   facts live in different places (`routing.toml` and the engine's saved CLI
//!   logins), which is exactly why nothing put them side by side before.
//!
//! Nothing here is authority. A `[users]` entry is a claim about which GitHub
//! account a sign-in email belongs to, and the board cannot verify it — the
//! App may not read anybody's verified addresses. What it can do is stop the
//! claim being a hand-edit, and say when half of it is missing.

use anyhow::{Result, bail};
use comet_proto::{AgentAccount, GitAuthor, HarnessId};
use serde::{Deserialize, Serialize};

use crate::config::RoutingConfig;
use crate::git_identity::{self, NOREPLY_SUFFIX};
use crate::sources::github::{Github, Rest};

/// The longest a GitHub login may be.
const MAX_LOGIN: usize = 39;

/// What `--github` was, once read.
///
/// The two spellings are both unambiguous and neither is guessable from the
/// other: a login is what a person knows, and the numeric id in the address is
/// what GitHub knows. Anything that is neither is refused rather than written,
/// because a `[users]` value that is not an address is the unattributable
/// commit gh#107 exists to prevent, arriving through the front door.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubRef {
    /// An address already: written as given.
    Author(GitAuthor),
    /// A bare login, for [`resolve_login`] to turn into the address GitHub
    /// minted for that account.
    Login(String),
}

/// A GitHub login: ASCII alphanumerics and hyphens, not starting or ending with
/// one, at most [`MAX_LOGIN`] characters.
///
/// Deliberately lenient about consecutive hyphens, which GitHub refuses for new
/// accounts and grandfathered for old ones — refusing `a--b` here would refuse
/// a real teammate over a rule that is not ours.
pub fn is_github_login(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_LOGIN
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Read a `--github` value.
///
/// Three spellings, because all three are things people have in hand: the
/// noreply address (`22494697+ana@users.noreply.github.com`), the `Name
/// <address>` form git itself prints, and the login (`ana`, or `@ana` as every
/// other tool writes it). Any address is accepted, not only a noreply one —
/// GitHub attributes to whichever account holds it, and refusing a work address
/// that *is* on the account would be the gh#96 false alarm again. `member list`
/// and `doctor` both say which entries took that weaker form.
pub fn parse_github(value: &str) -> Result<GithubRef> {
    let value = value.trim();
    if value.is_empty() {
        bail!(
            "--github is empty. Give their GitHub login, or the \
             `<id>+<login>@users.noreply.github.com` address from \
             https://github.com/settings/emails"
        );
    }
    // A leading `@` is how logins are written everywhere else; it is not part
    // of the login, and stripping it here is what stops `@ana` reading as an
    // address with an empty local part.
    let bare = value.strip_prefix('@').unwrap_or(value);
    if value.contains('<') || bare.contains('@') {
        return match git_identity::parse_author(value) {
            Some(author) => Ok(GithubRef::Author(author)),
            None => bail!(
                "`{value}` is not a git author — write an email address, `Name <email>`, \
                 or just their GitHub login"
            ),
        };
    }
    if !is_github_login(bare) {
        bail!(
            "`{value}` is neither an email address nor a GitHub login (letters, digits \
             and hyphens, at most {MAX_LOGIN})"
        );
    }
    Ok(GithubRef::Login(bare.to_string()))
}

/// The address GitHub mints for an account: `<id>+<login>@users.noreply.github.com`.
pub fn noreply(id: i64, login: &str) -> String {
    format!("{id}+{login}{NOREPLY_SUFFIX}")
}

/// Turn a bare login into the author to write, by asking GitHub for the id.
///
/// The name is the login, which is what [`git_identity::parse_author`] would
/// derive from the address anyway — so a caller with no `--name` writes the
/// bare address and the two spellings of one person are byte-identical.
pub fn resolve_login<T: Rest>(gh: &Github<T>, login: &str) -> Result<GitAuthor> {
    let user = gh.user(login).map_err(|e| {
        anyhow::anyhow!(
            "asking GitHub who `{login}` is: {e:#}. Paste their \
             `<id>+<login>@users.noreply.github.com` address from \
             https://github.com/settings/emails instead — it needs no lookup"
        )
    })?;
    Ok(GitAuthor {
        email: noreply(user.id, &user.login),
        name: user.login,
    })
}

/// The `[users]` value for an author: the address alone, or `Name <address>`.
///
/// The bare form whenever the name is the one the parser would derive anyway.
/// That is not tidiness: it is what makes `member add ana@example.com --github
/// ana` and the same command with the address pasted in produce the same line,
/// which is what "idempotent" has to mean here.
pub fn users_value(author: &GitAuthor) -> String {
    let derived = git_identity::parse_author(&author.email).map(|a| a.name);
    if derived.as_deref() == Some(author.name.as_str()) {
        author.email.clone()
    } else {
        format!("{} <{}>", author.name, author.email)
    }
}

/// Apply an operator-supplied display name to a resolved author.
pub fn named(author: GitAuthor, name: Option<&str>) -> GitAuthor {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => GitAuthor {
            name: name.to_string(),
            ..author
        },
        None => author,
    }
}

// ---- one member, several logins (gh#546) --------------------------------

/// The `[users]` entries grouped into members: entries whose values resolve to
/// the same git-author address are one person signing in from two places.
///
/// The grouping is the point of the map — its value answers "which GitHub
/// account is this person?" — so two keys sharing one answer are two logins of
/// one human, not two humans who happen to spell alike. This is the whole fix
/// for gh#546: a Claude plan and a Codex plan bill two different addresses, and
/// before this the billing guard compared those two addresses literally and
/// concluded that one of them belonged to a stranger. Writing the second
/// address down with the same GitHub value as the first says otherwise:
///
/// ```toml
/// [users]
/// "brede@tally.no" = "22494697+bredebjorhovd@users.noreply.github.com"
/// "brede.bjorhovd@recognition.no" = "22494697+bredebjorhovd@users.noreply.github.com"
/// ```
///
/// An entry whose value does not resolve ([`git_identity::parse_author`] is
/// `None`) groups alone — there is nothing to tie it to anything else with.
pub fn member_groups(cfg: &RoutingConfig) -> Vec<Vec<String>> {
    let mut authors: Vec<String> = Vec::new();
    let mut groups: Vec<Vec<String>> = Vec::new();
    for (key, value) in &cfg.users {
        let ix = match git_identity::parse_author(value).as_ref().map(|a| a.email.as_str()) {
            Some(email) => match authors.iter().position(|a| a.eq_ignore_ascii_case(email)) {
                Some(ix) => ix,
                None => {
                    authors.push(email.to_string());
                    groups.push(Vec::new());
                    authors.len() - 1
                }
            },
            // Nothing to tie it with, so nothing to group it into but itself.
            None => {
                groups.push(Vec::new());
                groups.len() - 1
            }
        };
        groups[ix].push(key.clone());
    }
    groups
}

/// Every other sign-in address in `[users]` belonging to the same member as
/// `email`, and empty when `email` is unmapped or maps alone.
///
/// This is the billing guard's second chance to be right: a run billed to one
/// of these is spending the dispatcher's own plan, however unlike their sign-in
/// address the login looks.
pub fn co_signins(cfg: &RoutingConfig, email: &str) -> Vec<String> {
    let needle = email.trim();
    if needle.is_empty() {
        return Vec::new();
    }
    member_groups(cfg)
        .into_iter()
        .find(|keys| keys.iter().any(|k| k.eq_ignore_ascii_case(needle)))
        .map(|keys| {
            keys.into_iter()
                .filter(|k| !k.eq_ignore_ascii_case(needle))
                .collect()
        })
        .unwrap_or_default()
}

// ---- the roster ---------------------------------------------------------

/// One saved CLI login on the box, as `member list` names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Slot {
    pub id: String,
    pub harness: HarnessId,
    /// The login's own email — what the billing guard compares against, and
    /// therefore what pairs a slot with a `[users]` entry. `None` for a slot
    /// whose credentials the engine could not read.
    pub email: Option<String>,
    /// Is this the login a dispatch naming no slot would spend?
    pub active: bool,
}

impl Slot {
    fn of(a: &AgentAccount) -> Slot {
        Slot {
            id: a.id.clone(),
            harness: a.harness,
            email: a
                .email
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_string),
            active: a.active,
        }
    }
}

/// One `[users]` entry, read beside the box's agent accounts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberRow {
    /// The map key: the email the teammate signs in to comet with, which is
    /// what a dispatch arrives as (gh#74).
    pub user: String,
    /// The value verbatim, so a surface can show what is actually in the file.
    pub value: String,
    /// What it parses to. `None` is the config error
    /// [`RoutingConfig::problems`] already reports — those dispatches commit as
    /// the box.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<GitAuthor>,
    /// Is the address one GitHub minted, and therefore attributable by
    /// construction?
    pub noreply: bool,
    /// The slots whose login is this person's — usually none or one, two when
    /// they have signed a Claude and a Codex account in.
    pub accounts: Vec<Slot>,
}

impl MemberRow {
    /// The pairing gh#162 exists to surface: mapped, so their commits are
    /// theirs, and no account of their own, so their runs spend somebody
    /// else's subscription.
    pub fn needs_account(&self) -> bool {
        self.accounts.is_empty()
    }
}

/// The `[users]` map and the box's agent accounts, read as one answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Roster {
    pub members: Vec<MemberRow>,
    /// Slots belonging to nobody in the map. Not a fault — the box owner's own
    /// login is normally here — but it is the other half of the same pairing,
    /// and the answer to "why did their commit land as the box".
    pub unmapped: Vec<Slot>,
    /// The engine could not be asked for the slots, so every pairing above is
    /// unknown rather than absent. The distinction is the whole of gh#155: a
    /// question nobody could answer must not render as a `no`.
    pub accounts_known: bool,
}

/// Pair the map against the slots.
///
/// The comparison is the billing guard's, deliberately: slot email against the
/// identity a dispatch arrives with, case-insensitively
/// ([`comet_proto::view::board::cross_billed`]). Anything cleverer would pair
/// differently from the thing that decides who pays, and two rules for one
/// question is how a surface ends up confidently wrong.
///
/// Since gh#546 "the billing guard's comparison" includes what the map says
/// about who is who: a slot pairs with every entry in its member group, so the
/// Codex login mapped beside a Claude login counts as an account of that
/// person's rather than as nobody's. A slot whose email matches a key outright
/// still pairs when its value does not resolve — equality needs no author.
pub fn roster(cfg: &RoutingConfig, accounts: Option<&[AgentAccount]>) -> Roster {
    let slots: Vec<Slot> = accounts.unwrap_or(&[]).iter().map(Slot::of).collect();
    let groups = member_groups(cfg);
    // Are these two addresses one member's? Same string, or two keys of one
    // group. The map is tiny; a scan per question costs nothing worth caching.
    let same_member = |a: &str, b: &str| {
        a.eq_ignore_ascii_case(b.trim())
            || groups.iter().any(|keys| {
                keys.iter().any(|k| k.eq_ignore_ascii_case(a))
                    && keys.iter().any(|k| k.eq_ignore_ascii_case(b.trim()))
            })
    };
    let members: Vec<MemberRow> = cfg
        .users
        .iter()
        .map(|(user, value)| {
            let author = git_identity::parse_author(value);
            MemberRow {
                noreply: author
                    .as_ref()
                    .is_some_and(|a| git_identity::is_github_noreply(&a.email)),
                accounts: slots
                    .iter()
                    .filter(|s| s.email.as_deref().is_some_and(|e| same_member(e, user)))
                    .cloned()
                    .collect(),
                user: user.clone(),
                value: value.clone(),
                author,
            }
        })
        .collect();
    let unmapped = slots
        .into_iter()
        .filter(|s| {
            !cfg.users
                .keys()
                .any(|u| s.email.as_deref().is_some_and(|e| same_member(e, u)))
        })
        .collect();
    Roster {
        members,
        unmapped,
        accounts_known: accounts.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::HarnessId::{ClaudeCode, Codex};

    fn account(id: &str, email: Option<&str>, harness: HarnessId) -> AgentAccount {
        AgentAccount {
            id: id.into(),
            harness,
            email: email.map(str::to_string),
            plan_label: None,
            active: false,
            usage_windows: Vec::new(),
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        }
    }

    #[test]
    fn a_bare_login_is_a_login_however_it_is_written() {
        assert_eq!(parse_github("ana").unwrap(), GithubRef::Login("ana".into()));
        // The spelling every other tool uses.
        assert_eq!(
            parse_github(" @Ana-Ruiz ").unwrap(),
            GithubRef::Login("Ana-Ruiz".into())
        );
    }

    #[test]
    fn an_address_is_taken_as_written() {
        let GithubRef::Author(a) = parse_github("22494697+ana@users.noreply.github.com").unwrap()
        else {
            panic!("read as a login");
        };
        assert_eq!(a.name, "ana");
        // The form git itself prints, name and all.
        let GithubRef::Author(a) =
            parse_github("Ana Ruiz <22494697+ana@users.noreply.github.com>").unwrap()
        else {
            panic!("read as a login");
        };
        assert_eq!(a.name, "Ana Ruiz");
    }

    /// A value that is neither must never reach the file: `[users]` holding a
    /// non-address is the unattributable commit gh#107 exists to prevent.
    #[test]
    fn anything_that_is_neither_is_refused_by_name() {
        for bad in [
            "",
            "   ",
            "@",
            "Ana Ruiz",
            "ana ruiz",
            "ana@",
            "-ana",
            "ana-",
            "ana_ruiz",
            "https://github.com/ana",
            "two@at@signs.com",
        ] {
            assert!(parse_github(bad).is_err(), "accepted {bad:?}");
        }
        // 39 is the limit, 40 is not a login.
        assert!(parse_github(&"a".repeat(MAX_LOGIN)).is_ok());
        assert!(parse_github(&"a".repeat(MAX_LOGIN + 1)).is_err());
    }

    /// The two spellings of one person have to produce one line, or `member
    /// add` run twice is two entries and a map whose behaviour depends on
    /// which one TOML kept.
    #[test]
    fn a_resolved_login_and_a_pasted_address_write_the_same_value() {
        let resolved = GitAuthor {
            name: "ana".into(),
            email: noreply(22494697, "ana"),
        };
        let GithubRef::Author(pasted) =
            parse_github("22494697+ana@users.noreply.github.com").unwrap()
        else {
            panic!("read as a login");
        };
        assert_eq!(users_value(&resolved), users_value(&pasted));
        assert_eq!(
            users_value(&resolved),
            "22494697+ana@users.noreply.github.com"
        );
        // A name that the address cannot produce is kept, in git's own form.
        assert_eq!(
            users_value(&named(resolved, Some("Ana Ruiz"))),
            "Ana Ruiz <22494697+ana@users.noreply.github.com>"
        );
    }

    /// A `--name` of nothing is not a name.
    #[test]
    fn an_empty_name_leaves_the_derived_one_alone() {
        let a = GitAuthor {
            name: "ana".into(),
            email: noreply(1, "ana"),
        };
        assert_eq!(named(a.clone(), Some("  ")), a);
        assert_eq!(named(a.clone(), None), a);
    }

    /// The pairing: mapped and slotless is the state nobody thinks about, and
    /// the reason a teammate's run spends the box owner's plan.
    #[test]
    fn the_roster_pairs_the_map_against_the_slots() {
        let cfg: RoutingConfig = toml::from_str(
            "[users]\n\
             \"ana@example.com\" = \"22494697+ana@users.noreply.github.com\"\n\
             \"sam@example.com\" = \"Sam Ito <sam@corp.example>\"\n\
             \"kim@example.com\" = \"kim\"\n",
        )
        .unwrap();
        let accounts = [
            // Case differs, as it does between a WorkOS sign-in and a CLI login.
            account("slot-ana", Some("Ana@Example.com"), ClaudeCode),
            account("slot-ana-codex", Some("ana@example.com"), Codex),
            account("slot-owner", Some("brede@tally.no"), ClaudeCode),
            // A slot the engine could not read the login of pairs with nobody
            // rather than with everybody.
            account("slot-blind", None, ClaudeCode),
        ];
        let r = roster(&cfg, Some(&accounts));
        assert!(r.accounts_known);

        // The map's own order — it is a `BTreeMap`, so the roster reads
        // alphabetically wherever it is printed.
        let by = |who: &str| {
            r.members
                .iter()
                .find(|m| m.user == who)
                .unwrap_or_else(|| panic!("{who} is not in the roster"))
        };

        let ana = by("ana@example.com");
        assert!(ana.noreply);
        assert!(!ana.needs_account());
        assert_eq!(ana.accounts.len(), 2, "both her logins: {:?}", ana.accounts);

        let sam = by("sam@example.com");
        assert!(sam.author.is_some());
        // An address on a domain of their own still authors — it is just not
        // attributable by construction.
        assert!(!sam.noreply);
        assert!(sam.needs_account());

        let kim = by("kim@example.com");
        assert!(kim.author.is_none(), "`kim` is not an address");
        assert!(!kim.noreply);

        // The box owner's own login, and the unreadable one.
        let unmapped: Vec<&str> = r.unmapped.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(unmapped, ["slot-owner", "slot-blind"]);
    }

    /// "No slots" and "could not ask" are different answers (gh#155): the
    /// second must not render every mapped teammate as needing an account.
    #[test]
    fn an_engine_that_could_not_be_asked_says_so_rather_than_reporting_none() {
        let cfg: RoutingConfig =
            toml::from_str("[users]\n\"ana@example.com\" = \"1+ana@users.noreply.github.com\"\n")
                .unwrap();
        let unknown = roster(&cfg, None);
        assert!(!unknown.accounts_known);
        assert!(unknown.unmapped.is_empty());

        let none = roster(&cfg, Some(&[]));
        assert!(none.accounts_known);
        assert!(none.members[0].needs_account());
    }

    /// gh#546: one human, two subscriptions, two addresses. Mapping the second
    /// address with the same GitHub value as the first makes the pairing — and
    /// therefore every surface built on it — read them as one person.
    #[test]
    fn two_logins_with_one_github_value_are_one_member() {
        let author = "22494697+bredebjorhovd@users.noreply.github.com";
        let cfg: RoutingConfig = toml::from_str(&format!(
            "[users]\n\
             \"brede@tally.no\" = \"{author}\"\n\
             \"brede.bjorhovd@recognition.no\" = \"{author}\"\n\
             \"ana@example.com\" = \"1+ana@users.noreply.github.com\"\n"
        ))
        .unwrap();

        // The grouping: Ana alone, Brede's two sign-ins together. `[users]`
        // is a `BTreeMap`, so the pairs read alphabetically.
        assert_eq!(
            member_groups(&cfg),
            vec![
                vec!["ana@example.com".to_string()],
                vec![
                    "brede.bjorhovd@recognition.no".to_string(),
                    "brede@tally.no".to_string()
                ],
            ]
        );
        assert_eq!(
            co_signins(&cfg, "brede@tally.no"),
            ["brede.bjorhovd@recognition.no"]
        );
        // Case-insensitive on the way in, like every other email comparison.
        assert_eq!(
            co_signins(&cfg, "BREDE@Tally.No"),
            ["brede.bjorhovd@recognition.no"]
        );
        // Unmapped, empty, and a member of nobody.
        assert!(co_signins(&cfg, "sam@example.com").is_empty());
        assert!(co_signins(&cfg, "  ").is_empty());

        // And the roster pairs each login's slot with both entries: either row
        // answers "does this person have an account of their own".
        let accounts = [
            account("slot-claude", Some("brede@tally.no"), ClaudeCode),
            account(
                "slot-codex",
                Some("brede.bjorhovd@recognition.no"),
                Codex,
            ),
            account("slot-ana", Some("ana@example.com"), ClaudeCode),
        ];
        let r = roster(&cfg, Some(&accounts));
        for m in &r.members {
            if m.user.starts_with("brede") {
                assert_eq!(m.accounts.len(), 2, "{} sees both logins", m.user);
                assert!(!m.needs_account());
            }
        }
        assert!(r.unmapped.is_empty(), "{:?}", r.unmapped);
    }

    /// The grouping never merges two people: same name spelling is not the
    /// same as same address, and an entry that resolves to nothing groups
    /// alone rather than with everything.
    #[test]
    fn grouping_is_by_resolved_address_and_nothing_looser() {
        let cfg: RoutingConfig = toml::from_str(
            "[users]\n\
             \"ana@work.example\" = \"Ana Ruiz <1+ana@users.noreply.github.com>\"\n\
             \"ana@home.example\" = \"2+ana@users.noreply.github.com\"\n\
             \"broken@example.com\" = \"not an address\"\n",
        )
        .unwrap();
        // Two different GitHub ids are two different accounts, whatever the
        // display names say.
        assert_eq!(co_signins(&cfg, "ana@work.example"), Vec::<String>::new());
        // An unresolvable value has no group to belong to.
        assert!(member_groups(&cfg)[2].len() == 1);
    }
}
