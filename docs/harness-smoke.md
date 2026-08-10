# Harness smoke tests

- 2026-08-09 · codex · gpt-5.6-luna · clean
- 2026-08-09 · opencode · deepseek/deepseek-v4-flash · the board's `GIT_ASKPASS` (with the `git-askpass` subcommand appended) was exec'd literally by git 2.53 rather than through `sh -c`, so the push needed a wrapper script that still minted through the board's App credential — §gh#233: not a git 2.53 change and not an opencode one, `GIT_ASKPASS` has always been exec'd directly; the subcommand now rides in a shim script instead, and a push the board's credential did not make is reported rather than improvised around
