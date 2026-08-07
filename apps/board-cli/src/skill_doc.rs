//! The skill's verb table, rendered from clap (gh#133).
//!
//! `assets/skills/comet-board/SKILL.md` is prose an agent reads before acting,
//! and prose about a CLI goes stale the moment a flag is added. The prose is
//! authored; the reference table under "Every verb" is not — it is generated
//! from the very [`clap::Command`] this binary parses with, and the test below
//! fails the build when the committed file and the parser disagree.
//!
//! Regenerate instead of hand-editing:
//!
//! ```console
//! $ UPDATE_SKILL=1 cargo test -p comet-board-bin
//! ```

use clap::Command;

const BEGIN: &str = "<!-- BEGIN generated verbs — rendered from clap by \
                     `cargo test -p comet-board-bin`; rewrite with UPDATE_SKILL=1 -->";
const END: &str = "<!-- END generated verbs -->";

/// The generated block's body: the global flags, then one row per verb.
pub fn verb_table(cli: &Command) -> String {
    let mut out = String::new();
    let globals: Vec<String> = cli
        .get_arguments()
        .filter(|a| a.is_global_set())
        .filter_map(|a| a.get_long())
        .map(|l| format!("`--{l}`"))
        .collect();
    if !globals.is_empty() {
        out.push_str(&format!(
            "Global flags, on every verb: {}.\n\n",
            globals.join(", ")
        ));
    }
    out.push_str("| verb | flags | what it is for |\n| --- | --- | --- |\n");
    for sub in visible(cli) {
        rows(sub, "", &mut out);
    }
    out
}

/// One verb's row, then its own subcommands' — `routes`, `skill`. One level of
/// nesting is all the CLI has and all this renders; a deeper tree would need a
/// shape the table cannot carry anyway.
fn rows(cmd: &Command, prefix: &str, out: &mut String) {
    let name = format!("{prefix}{}", cmd.get_name());
    let flags: Vec<String> = cmd
        .get_arguments()
        .filter(|a| !a.is_global_set() && !a.is_positional())
        .filter_map(|a| a.get_long())
        .filter(|l| *l != "help" && *l != "version")
        .map(|l| format!("`--{l}`"))
        .collect();
    out.push_str(&format!(
        "| `{}` | {} | {} |\n",
        cell(&format!("{name}{}", positionals(cmd))),
        if flags.is_empty() {
            "—".to_string()
        } else {
            flags.join(", ")
        },
        cell(&about(cmd)),
    ));
    for sub in visible(cmd) {
        rows(sub, &format!("{name} "), out);
    }
}

fn visible(cmd: &Command) -> impl Iterator<Item = &Command> {
    cmd.get_subcommands().filter(|s| !s.is_hide_set())
}

fn positionals(cmd: &Command) -> String {
    cmd.get_positionals()
        .map(|a| {
            let id = a.get_id().as_str();
            if a.is_required_set() {
                format!(" <{id}>")
            } else {
                format!(" [{id}]")
            }
        })
        .collect()
}

/// clap's short about — the doc comment's first paragraph, which is wrapped
/// across source lines. One table cell, so one line.
fn about(cmd: &Command) -> String {
    cmd.get_about().map(|a| a.to_string()).unwrap_or_default()
}

/// Whitespace collapsed, and `|` escaped so a cell cannot become two.
fn cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// Put `table` between the markers in `md`.
pub fn splice(md: &str, table: &str) -> Result<String, String> {
    let start = md
        .find(BEGIN)
        .ok_or_else(|| format!("SKILL.md has no `{BEGIN}` marker"))?
        + BEGIN.len();
    let end = md
        .find(END)
        .ok_or_else(|| format!("SKILL.md has no `{END}` marker"))?;
    if end < start {
        return Err("SKILL.md's generated-verbs markers are in the wrong order".into());
    }
    Ok(format!("{}\n{table}{}", &md[..start], &md[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn asset() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/skills/comet-board/SKILL.md")
    }

    /// The point of the whole module: the shipped skill cannot describe a CLI
    /// this binary does not have.
    #[test]
    fn the_shipped_skill_matches_the_parser() {
        let path = asset();
        let on_disk = std::fs::read_to_string(&path).expect("read SKILL.md");
        let want = splice(&on_disk, &verb_table(&crate::Cli::command())).unwrap();
        if on_disk == want {
            return;
        }
        if std::env::var("UPDATE_SKILL").is_ok_and(|v| !v.is_empty()) {
            std::fs::write(&path, &want).expect("write SKILL.md");
            return;
        }
        panic!(
            "assets/skills/comet-board/SKILL.md's verb table is not what clap says.\n\
             Rewrite it: UPDATE_SKILL=1 cargo test -p comet-board-bin"
        );
    }

    #[test]
    fn every_verb_is_in_the_table() {
        let cli = crate::Cli::command();
        let table = verb_table(&cli);
        for sub in cli.get_subcommands().filter(|s| !s.is_hide_set()) {
            assert!(
                table.contains(&format!("| `{}", sub.get_name())),
                "`{}` is missing from the generated table",
                sub.get_name()
            );
        }
        // The two the agents must never be told about: git and `gh` run them,
        // people do not.
        assert!(!table.contains("git-askpass"));
        assert!(!table.contains("gh-token"));
    }

    #[test]
    fn a_pipe_in_an_about_cannot_break_the_row() {
        assert_eq!(cell("a | b"), "a \\| b");
        assert_eq!(cell("wrapped\n   over lines"), "wrapped over lines");
    }
}
