# Working in this repository

A brief for models. Short and stable on purpose — session-specific state lives in
`docs/HANDOFF.md`, not here.

---

## What this is

Smithy: a Rust code editor with a built-in coding agent. Eight crates, one
binary.

| Crate | What it is | UI dependency? |
|---|---|---|
| `apps/smithy` | the binary — window, menus, panels wired together | yes |
| `smithy-editor` | panels, syntax styling, LSP client, terminal, file browser | yes (floem) |
| `smithy-fisherman` | the rail figure: routine, poses, drawing via `Ink` | **no** |
| `smithy-agent` | agent loop, budgets, session persistence, providers | **no** |
| `smithy-tools` | the agent's tools and the capability sandbox | **no** |
| `smithy-project` | project detection, context extraction, call graph, symbols | **no** |
| `smithy-sky` | astronomy for the backdrop; zero dependencies | **no** |
| `smithy-voice` | microphone in, string out | **no** |

The five UI-free crates are a deliberate boundary: a different front-end would
be a new consumer, not a rewrite. Do not reach for floem inside them.

## Build and test

```bash
cargo test --workspace     # 963 tests, ~10s, all green
cargo build --workspace    # 0 warnings, and it stays 0
cargo clippy --workspace --all-targets
```

- **There is no GitHub CI, on purpose.** Checks run here, as a pre-push hook.
  One-time setup per clone:

  ```bash
  git config core.hooksPath .githooks
  ```

  It builds with warnings-as-errors and runs the suite before anything leaves
  the machine. `SKIP_CHECKS=1 git push` bypasses it deliberately. GitHub
  Actions was tried and removed — the account is Actions-locked, so every run
  failed in four seconds having executed nothing, and a permanently red check
  is worse than none: it teaches you to ignore the colour. Do not re-add a
  workflow without checking that Actions can actually start.
- **macOS only.** `smithy-voice` uses `candle-core` with the `accelerate`
  feature, which is Apple's framework. The workspace does not build on Linux
  as configured.
- **The tree is not rustfmt-clean and that is not a bug to fix.** Doc comments
  are hand-wrapped throughout. Do not run `cargo fmt` across the workspace; the
  diff would be enormous, unreviewable, and would fight the formatting on
  purpose. Format your own new code to match what is around it.
- `floem` is pinned to a **git revision**, not a crates.io version. The release
  is ~20 months behind and lacks the APIs used here. Bump it deliberately,
  never casually.
- Running the app while iterating: `cargo run -p smithy --release`. Do **not**
  `cargo install --path apps/smithy --force` in a loop — it invalidates macOS
  Keychain ACLs and re-triggers a prompt per stored key.

## House conventions

These are the things most likely to be eroded by an agent optimising locally.
They are not stylistic preferences; each one is load-bearing.

1. **Every constant carries the failure that motivated it.** Not what it does —
   *what went wrong without it*. `ARRIVAL = 0.80` is documented as the beat that
   stops the fisherman ceasing to exist at the wall; the secondary animation
   periods are irrational to each other with Chuck Jones cited for why. Follow
   this for anything you add. A bare number with no story is the thing this
   codebase does not have.

2. **Tests assert behaviour, not implementation.** They are named as sentences
   (`he_turns_around_rather_than_walking_backwards`,
   `the_model_is_warned_before_the_step_ceiling_not_at_it`) and each carries a
   doc comment describing the real failure it guards. Write new tests the same
   way.

3. **Never loosen a threshold to make a check pass.** If a check is wrong,
   change it in a commit that says which real case motivated the change and
   shows it. Widening a bound to get green is deleting the test.

4. **Comments explain decisions, not mechanics.** Especially the ones that
   record a road not taken — "we tried name-matching calls, it was wrong 45% of
   the time" is worth more than any amount of restating the code.

5. **Dependencies are argued for in `Cargo.toml`.** Every non-obvious one has a
   comment saying why it is there and what hand-rolling it would get wrong. Add
   dependencies the same way, or not at all.

## Landmines

Each of these has already cost time here.

**floem / UI**
- Effects created outside a long-lived owner — a menu-click handler, for
  instance — are disposed immediately and silently no-op. Use `poll_once` /
  `exec_after` (the settings module is the pattern to copy).
- **Never** call `signal.set` unconditionally from a paint or canvas path. It
  re-triggers the paint and the app hangs.
- Never compute anything expensive on the paint path. `CallGraph::staleness` is
  the specific one that has been paid for; cache it off-path
  (`CallGraphUi.stale`) and only read while painting.
- The mono font tofu-boxes `↑`, `↓` and `·`. Use ASCII in UI chrome.

**Agent / context**
- `History` is **append-only**. No `remove`, `truncate`, `insert` or `get_mut`.
  This is what keeps the provider's prefix cache valid. If compaction is ever
  added it must append a summary and start a new history, never rewrite one.
- The system prompt, the project context block and the tool schema array are
  **byte-stable for the life of a session** by design. Tool order is fixed;
  schema parameters are `Vec` not `HashMap` for this reason. Anything that makes
  these vary between turns silently doubles cost and latency.
- Reasoning is stored in a sidecar and **never** enters `History`.
- Never resolve calls by name matching. It is ~45% wrong on this workspace and
  wrong in a way that looks plausible. Edges come from rust-analyzer via SCIP.

## Where things are written down

| File | What it holds |
|---|---|
| `README.md` | user-facing: setup, usage, configuration, known gaps |
| `docs/HANDOFF.md` | **session state** — what just landed, what is open. Read this second. |
| `docs/CALLGRAPH_PLAN.md` | call graph architecture and milestones |
| `docs/CONTEXT_AUDIT.md` | what we send models, what it costs, what is unmeasured |
| `docs/FISHERMAN_VERIFICATION_PLAN.md` | the animation harness plan |

## Before you finish

- `cargo test --workspace` green, `cargo build --workspace` at zero warnings.
- Work on a branch. Do not push to `main`.
- If you changed something a comment claims is true, change the comment.
- If you left something undone, say so plainly. An honest gap is worth more
  than a confident summary that does not survive a relaunch.
