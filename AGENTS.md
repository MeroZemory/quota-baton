# AGENTS.md: rules for agents working on quota-baton

The documentation map is in `docs/README.md`. The normative behaviour is in `docs/contract.md`.

## Build hygiene (C: has little free space; D: and E: are full)

- **One target per worktree.** Use the worktree's default `target/` only.
  - Do not create extra `CARGO_TARGET_DIR`s per feature set, per reviewer or per test run. Examples to avoid: `.sandbox/target-*`, `.sandbox/targets/*`.
  - Build the feature variants one after another in the same target.
- **No incremental caches.** `.cargo/config.toml` sets `incremental = false`. Do not override it.
- **Clean up at the end of a milestone.** Run `cargo clean` in the worktree after the milestone report is sent, unless the next milestone starts immediately in the same worktree.
- **Run a review build in the implementer's worktree only while the implementer is idle.** Use the default target, run `cargo test` / `cargo clippy` sequentially, and do not keep a second copy.
- Never move build output to D: or E:.
- If a cleanup is blocked, report the exact path and size instead of working around it.
