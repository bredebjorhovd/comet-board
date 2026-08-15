# Dispatches carry MCP servers — **done** (gh#273)

The board had one half of MCP support: every harness normalized MCP tool calls
after an agent made them, but no dispatch could give an agent an MCP server to
call. The missing seam now follows the same ownership path as the other route
policy:

```text
routing.toml → DispatchSpec → ChatConfig → RunControls → harness launch
```

`McpServer` is deliberately the common local-stdio denominator: a stable name,
an executable, and arguments. `[defaults].mcp_servers` supplies the list for
every route; `mcp_servers` on a `[[route]]` replaces that list, and an explicit
empty list opts out. The shipped default is the first concrete consumer:

```toml
[defaults]
mcp_servers = [
  { name = "comet-board", command = "comet-board", args = ["mcp"] },
]
```

The default is implicit so existing boards gain the structured channel without
rewriting their configuration. Names and commands are validated before a
dispatch; names that would collapse to the same MCP tool prefix are refused.

### One description, three process-local adapters

- Claude Code receives one inline `--mcp-config` JSON value.
- Codex receives complete `mcp_servers.<name>` TOML overrides before its
  `app-server` subcommand.
- OpenCode receives a merge into its highest-precedence
  `OPENCODE_CONFIG_CONTENT` JSON.

None of the adapters writes the checkout or a reused account directory. That
is the important difference from §gh#272: MCP servers are chat configuration,
not account furniture. Two concurrent routes can share an account slot without
one route's servers leaking into the other, and a review follow-up reads the
same list stamped onto its chat.

Server children inherit the harness process environment, so credentials can be
stamped for a run without putting them in `routing.toml` or a config file. This
first shape does not pretend remote transports and their different auth models
are uniform; one earns a protocol variant when it has a concrete consumer.

### The board server

The hidden `comet-board mcp` process is a newline-delimited stdio JSON-RPC
server over the board CLI's existing typed engine client. It exposes three
tools:

- `task_status` reads a task row, defaulting to the task behind the current
  dispatched or review chat.
- `related_attempts` returns the current task, sibling attempts released by the
  same parent chat, and tasks this chat released.
- `dispatch_task` uses the ordinary checked dispatch path, including routing,
  runtime/model validation, billing warnings, one-live-attempt protection, and
  inherited provenance. Its schema and description repeat the existing rule:
  dispatch starts a real agent and requires explicit human authorization.

Tool results contain both an MCP text fallback and `structuredContent`. The
agent no longer shells out to the CLI, parses terminal JSON, or carries its chat
id through a command line; the MCP process inherits the run identity once and
uses it internally.

The existing `ToolCall::Mcp` normalization paths need no changes. Once the
harness starts a server, its calls already render in transcripts like any other
typed tool call.
