# Remote routing surface — **done** (gh#75)

`routing.toml` is a hand-edited file on the box, documented as "not managed
config", and no RPC touched it. Adding a repo, pointing a route at a different
agent account, or lifting a cap was an ssh-and-edit job — fine for whoever set
the box up, a dead end for the teammate #66 just let onto the board.

An RPC pair, `ReadBoardConfig` / `WriteBoardConfig`, both forwardable for the
same reason the board four are: the file lives on the host.

- **`crates/board/src/routes.rs`** is the file half. `read` answers with the
  text, its parse, and *everything* wrong with it; `edit` applies one change.
  Every write goes through `adopt::apply` — the writer §adopt-doctor-init already proved out —
  so the discipline is one implementation and not two: it has to parse, it has
  to validate, and the previous contents land in `routing.toml.bak` first. An
  edit that would break the config is refused naming what it would have broken,
  and the file is untouched. `RoutingConfig` gained `Serialize` so a reader can
  be handed the parse; nothing writes TOML from it, because every edit is a
  *text* edit for the reason `add_to_array` gives — the file is full of
  comments explaining choices, and re-serializing throws all of them away.
- **`RoutingConfig::problems`** collects what `validate` refuses on instead of
  stopping at the first. Same checks, same strings; `validate` is now "the first
  problem, if any". An editor that shows one at a time turns fixing three into
  three round trips, and the reader of a remote box's config cannot see the file
  to spot the rest.
- **The edits are a closed list.** `text` (whole file, with an optional `base`
  precondition), `route` and `default` (one typed key each), plus `adopt` and
  `ignore`, which live on the engine because they need the space list and a git
  probe. An unknown key is refused *by name*: a misspelt key in a TOML file is
  not an error — it parses, it is ignored, and the route goes on behaving the
  way it did while somebody believes they changed it. Multi-line-string tracking
  is load-bearing in the key finder, exactly as it is in `header_lines`: a
  route's `prompt = """…"""` containing a line that reads `base = …` is prose,
  and editing it would rewrite the agent's brief.
- **`comet-board routes`** — `list` (routes, problems, what is unadopted),
  `show`, `add`/`ignore`, `set <n> <key> <value>`, `defaults <key> <value>`, and
  `edit` for `$EDITOR`. All over the RPC, so `--device` reaches the box; `adopt`
  stays the local-files command it was. `routes edit` carries the text it
  started from, so a hand-edit on the box in the meantime is refused rather than
  overwritten — this is still a file people edit by hand.
- **Settings → Board routing** (`crates/ui/src/settings/routing.rs`) is the
  desktop half: the routes, the problems as warning strips, per-route Runtime /
  Account / Cap, and the unadopted list with Add and Ignore. It finds the host
  by the same contract the board panel sweeps on — the engine refuses a board
  method outright when it hosts no board, so a candidate that errors has
  answered "not me" — which keeps it independent of whether the board panel has
  ever been opened.
- **Not touched: `.env`.** Credentials are the other hand-edited file, and
  moving secrets over the wire is a different decision from moving routes.
  `doctor` still says which keys are missing.
