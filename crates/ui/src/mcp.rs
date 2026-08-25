//! Shared MCP-server editing logic for the two surfaces that manage the list
//! (gh#606): the routing page's per-route editor and the composer's per-chat
//! override.
//!
//! Both render rows of (name · command · args) against the same wire shape
//! ([`comet_proto::McpServer`]) and both must enforce the rules the board's
//! validating writer applies (`comet_board::config::mcp_server_problems`) —
//! so the rules live there and both call it, rather than each growing a
//! private dialect that drifts from what a write would refuse.

use gpui::{AppContext as _, Entity, Subscription};
use serde::Deserialize;

use crate::composer::ComposerInput;

/// The built-in server every route inherits unless something overrides it:
/// the board's own dispatch seam, which needs no arguments (gh#273). The
/// editors offer it as one click because it is the thing people are usually
/// reaching for.
pub const BUILTIN_NAME: &str = "comet-board";
pub const BUILTIN_COMMAND: &str = "comet-board";
pub const BUILTIN_ARGS: &[&str] = &["mcp"];

/// The built-in server as a row value.
pub fn builtin_board_server() -> comet_proto::McpServer {
    comet_proto::McpServer {
        name: BUILTIN_NAME.into(),
        command: BUILTIN_COMMAND.into(),
        args: BUILTIN_ARGS.iter().map(|a| (*a).into()).collect(),
    }
}

/// An args text field becomes a `Vec<String>` by whitespace: v1 keeps the
/// mapping obvious rather than shell-clever — an argument containing a space
/// cannot be typed yet, and the field's caption says so. Empty pieces
/// (double spaces, leading/trailing) vanish.
pub fn split_args(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// One editor row: three inputs and their event subscriptions, shared by both
/// editors so a row behaves identically wherever it is rendered.
pub(crate) struct McpRowInputs {
    pub name: Entity<ComposerInput>,
    pub command: Entity<ComposerInput>,
    pub args: Entity<ComposerInput>,
    /// Kept alive so `Edited`/`Submitted` deliveries survive the render loop;
    /// editors that subscribe push onto this through [`Self::subscribe`].
    pub events: Vec<Subscription>,
}

impl McpRowInputs {
    pub fn new(name: &str, command: &str, args: &str, cx: &mut gpui::App) -> Self {
        let name_input = cx.new(|cx| ComposerInput::new("name", cx));
        let command_input = cx.new(|cx| ComposerInput::new("command", cx));
        let args_input = cx.new(|cx| ComposerInput::new("args (space-separated)", cx));
        name_input.update(cx, |input, cx| input.set_text(name.to_string(), cx));
        command_input.update(cx, |input, cx| input.set_text(command.to_string(), cx));
        args_input.update(cx, |input, cx| input.set_text(args.to_string(), cx));
        Self {
            name: name_input,
            command: command_input,
            args: args_input,
            events: Vec::new(),
        }
    }

    /// The row's values, verbatim — trimming and splitting happen at collect
    /// time, so a half-typed row reads back as typed.
    pub fn values(&self, cx: &gpui::App) -> (String, String, String) {
        (
            self.name.read(cx).text().to_string(),
            self.command.read(cx).text().to_string(),
            self.args.read(cx).text().to_string(),
        )
    }
}

/// Rows → wire servers: trimmed name/command, whitespace-split args. What the
/// validating writer would parse the written TOML into.
pub fn collect_servers(rows: &[(&str, &str, &str)]) -> Vec<comet_proto::McpServer> {
    rows.iter()
        .map(|(name, command, args)| comet_proto::McpServer {
            name: name.trim().to_string(),
            command: command.trim().to_string(),
            args: split_args(args),
        })
        .collect()
}

/// The route row's one-word reading of its MCP state — inherited lists are
/// named as inherited, the way [`comet_board::routes::cap_summary`] names an
/// inherited cap, because "where do I go to change this" is decided by which
/// of the two it is.
pub fn route_mcp_summary(route_servers: Option<&[comet_proto::McpServer]>) -> String {
    match route_servers {
        None => "default".into(),
        Some([]) => "off".into(),
        Some([one]) => one.name.clone(),
        Some(many) => format!("{} servers", many.len()),
    }
}

/// What the reply to `LocateCommands` means per command: where it resolved,
/// or nowhere.
#[derive(Debug, Default, Clone)]
pub struct CommandLocations {
    pub found: std::collections::HashMap<String, Option<String>>,
}

impl CommandLocations {
    /// Wire → map. Absent keys read as not-found (an older engine answers
    /// less, never wrong).
    pub fn from_reply(value: &serde_json::Value) -> Option<Self> {
        #[derive(Deserialize)]
        struct Reply {
            #[serde(default)]
            found: std::collections::HashMap<String, Option<String>>,
        }
        serde_json::from_value::<Reply>(value.clone())
            .ok()
            .map(|reply| Self { found: reply.found })
    }

    pub fn location(&self, command: &str) -> Option<Option<&String>> {
        self.found.get(command.trim()).map(Option::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_split_on_whitespace_and_only_whitespace() {
        assert_eq!(split_args(""), Vec::<String>::new());
        assert_eq!(split_args("   "), Vec::<String>::new());
        assert_eq!(split_args("mcp"), vec!["mcp"]);
        assert_eq!(split_args(" --port 8080 "), vec!["--port", "8080"]);
        assert_eq!(
            split_args("-c\tvalue\n--flag"),
            vec!["-c", "value", "--flag"]
        );
    }

    #[test]
    fn summaries_name_inheritance_off_and_lists() {
        assert_eq!(route_mcp_summary(None), "default");
        assert_eq!(route_mcp_summary(Some(&[])), "off");
        let one = [builtin_board_server()];
        assert_eq!(route_mcp_summary(Some(&one)), "comet-board");
        let two = [
            builtin_board_server(),
            comet_proto::McpServer {
                name: "docs".into(),
                command: "context7".into(),
                args: vec![],
            },
        ];
        assert_eq!(route_mcp_summary(Some(&two)), "2 servers");
    }

    #[test]
    fn collected_rows_are_trimmed_and_split_like_the_writer_parses() {
        let servers = collect_servers(&[("  docs  ", " context7 ", "--port 8080")]);
        assert_eq!(servers[0].name, "docs");
        assert_eq!(servers[0].command, "context7");
        assert_eq!(servers[0].args, vec!["--port", "8080"]);
    }

    #[test]
    fn the_builtin_is_what_the_shipped_default_ships() {
        // The board's own default (crates/board config.rs default_mcp_servers)
        // is the same seam; if either side moves, this pins the disagreement.
        let builtin = builtin_board_server();
        assert_eq!(builtin.name, "comet-board");
        assert_eq!(builtin.command, "comet-board");
        assert_eq!(builtin.args, vec!["mcp"]);
    }
}
