# The agents could not run the board — **done** (gh#184)

Read off a live agent on the box, not inferred: `PATH=/usr/local/bin:…:/snap/bin`,
and `comet-board` nowhere in it. `install.sh` links the CLI into `~/.local/bin`,
which a systemd **user** service does not inherit and a non-interactive ssh
shell never sources — so on the one machine that runs dispatched agents, every
verb §gh#133's skill hands them was `command not found`.

- **The failure had no symptom.** An agent that cannot reach the board does not
  crash. It stops checking `dispatchable`, stops releasing sub-work through the
  board, stops `wait`ing, and gets on with the ticket — and the provenance the
  board exists to guarantee quietly does not happen. Nothing logs a shell's
  failed lookup.
- **The engine already had the seam.** `PushCredentials::apply` prepends the
  `gh` shim's directory to the harness child's PATH (§gh#68), carefully reading
  the adapter's PATH back rather than the process's. That is now one of two
  layers over a shared `prepend_dirs_to_path`: `RunControls::bin_dirs` goes on
  first, the shim on top, so a `gh` beside either still loses to the one
  carrying the credential.
- **The directory is the one §gh#156 made real.** `agent_bin_dir()` is
  `resolve_board_exe()`'s parent, which buys two things a configured path would
  not: it cannot name a directory with no `comet-board` in it, and it is the
  same copy `GIT_ASKPASS` runs as. On a managed box that is `app/<version>/`,
  the payload the engine itself shipped in — so an agent gets the CLI that
  shipped with the engine running it, by construction. §gh#156's drift check asks
  whether the two are in step; this makes them in step for the caller that
  matters.
- **Every run, not only dispatched ones.** The skill is installed for the whole
  box, so an orchestrator the board never dispatched reads the same page of
  verbs; gating the PATH entry on a push credential would have left it
  `command not found` there. Resolved once at engine assembly — it is a property
  of the install, not of the run — and empty resolves to no change at all,
  never an empty PATH entry.
- **`doctor` asks it now.** The **agent PATH** check fails when the payload
  holds no `comet-board` for the engine to point at, which is the one way the
  guarantee comes apart and is exactly the state §gh#156 was about. It answers for
  the payload on this disk, not for the running process, and says so.
- **The skill no longer promises what the box may not keep.** It says the engine
  puts `comet-board` on the agent's PATH, and tells an agent that cannot find it
  to say so and stop rather than work on without the board — the silent
  degradation above is worse than a refusal anybody can read.
