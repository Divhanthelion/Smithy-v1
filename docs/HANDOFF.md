# Handoff

Everything built in one session, what is actually proven, what to test, and how
to finish the call graph.

**Read the verification table first.** 938 tests pass, and that is a weaker claim
than it sounds: a large amount of this is floem view code, and this repository
has **no view-level test harness at all**. A passing suite here means the logic
under the UI is right. It says nothing about whether anything renders.

---

## 1. State of play

Fourteen commits, `ba036dd` → `f2e14b8`. Grouped by how much they can be trusted.

### Proven against live external systems

| What | Evidence |
|---|---|
| DeepSeek backend | Real API. Catalogue lists 2 models; balance endpoint returned $9.93 of $10. |
| LM Studio catalogue + load | Real server. 13 models listed, `liquid/lfm2.5-1.2b` loaded in 4.7 s and unloaded. |
| OpenRouter catalogue | Real API. 337 models, 17 free, 14 of those tool-capable. |
| Symbol index + enclosure | Checked by eye: `desktop.rs:400` → `Desktop::restore_session` (392–428); `:31`, in an enum, correctly nothing. |
| SCIP reader | Cross-checked against an independent Python parse written *first*: 25 docs / 14,027 occurrences / 2,445 roles, identical. |
| Call graph | `Desktop::restore_session`'s five callees confirmed by reading lines 392–428. Pointed at itself, `CallGraph::assemble` correctly reports its own three callees, ×2 each. |
| Persistence + staleness | Appended a line to `scip.rs` → `1 changed`, named it; reverted → `current`. |
| Reasoning capture | **37 reasoning blocks** stored in the kernelos session. Previously always 0. |
| Attachments | User attached `TRIAGE.md` in a real run; the panel showed *"Attached 1 file"*. |
| DeepSeek doing real work | 27 build errors → 0 on kernelos, verified independently by `cargo check`. |

### Unit-tested, never seen running

Everything here compiles, has tests, and **has never been looked at in the GUI**.
This is the real risk surface.

- Settings dialog: rendering, model picker, filters, save-and-reconnect
- Blocking write-review gate against a real model
- `⚠ edits land directly` auto-approve toggle
- Explorer `+` attach button
- Menu-bar meters (spend, memory)
- Project map behind an empty editor
- `Code → Stop / Start Language Server`
- Panel chrome fixes — the user saw the *broken* version; the fix is installed but unseen
- `explore` sub-agent driven by a real model
- `web_search` — the Brave key is in the keychain and the tool has never been called
- The step wrap-up warning at 4/5 of budget

### Not started

- Call graph milestones 5 (rendering) and 6 (live linking)
- Nothing in the app writes a `callgraph.json` yet — the module is library-only

---

## 2. Test plan

Ordered by *risk × cheapness*. Roughly 40 minutes to get through §2.1 and §2.2.

### 2.1 Automated — the floor

```bash
cargo test --workspace          # expect 938 passing, 0 failing
cargo build --workspace         # expect 1 pre-existing warning (longitude_from_timezone)
cargo install --path apps/smithy --force
```

Then the CLI harnesses, which cover the parts with no UI:

```bash
cargo run -p smithy-project --example symbols   -- . 
cargo run -p smithy-project --example symbols   -- . --at crates/smithy-project/src/symbols.rs:300
cargo run -p smithy-project --example scip      -- /tmp/x.scip
cargo run -p smithy-project --example callgraph -- . --scip /tmp/x.scip restore_session
cargo run -p smithy-agent   --example models    -- deepseek
cargo run -p smithy-agent   --example transcript -- list
```

### 2.2 Manual — the GUI, in dependency order

Each step should be done before the one below it, because a failure early
invalidates what follows.

**A. It starts and the chrome is intact.**
1. `smithy ~/Desktop/kernelos-master`
2. The agent panel header reads `Agent` — **not `A ge nt`**. Tool rows read
   `bash`, not `bas h`. This is the fix the user has not yet seen.
3. Top-right of the menu bar shows memory, and spend if DeepSeek is selected.
4. With no file open, the editor area shows the project map behind the shortcut
   list.

**B. Settings.** `Agent → Backend Settings…`
1. The model list populates on open. Free-only is on for OpenRouter.
2. `Tool-capable` filter: unchecking it should reveal ~4 more models on
   DeepSeek/LM Studio.
3. Change the model, **Save & reconnect**. Header shows the new model.
4. Reopen: the setting persisted. Check `~/.local/share/smithy/provider.json`.
5. The key field must be **empty** on open and say a key is saved.

**C. Attachments.**
1. Click `+` on a file in the Explorer. The agent panel opens; a chip appears.
2. Click the chip → it dims (excluded). Click again → included.
3. Drag a file from Finder onto the panel; the drop outline appears.
4. Send a message. Transcript shows `Attached 1 file: …`; chips clear.

**D. The review gate — the highest-value test.** This is the fix for the
failure that started all of this.
1. Ensure the header reads `✓ edits reviewed`.
2. Ask for a small edit: *"add a doc comment to the top of src/lib.rs"*.
3. The diff modal opens **and the agent visibly waits** — the step row stays
   spinning. That waiting is the entire point.
4. Accept. The tool result must read **accepted in full**, as a *success*.
   If you see `waiting for the user to approve`, the blocking gate did not take.
5. Reject on a second edit → the model should stop, not retry.
6. Toggle to `⚠ edits land directly` and repeat: no modal, edit lands.

**E. Budgets and traces.**
1. Run a long task. Around step 144 of 180 the agent should be told how many
   calls remain and asked to wrap up.
2. Afterwards: `transcript list` shows a non-zero reasoning count;
   `transcript show <FILE> --reasoning` renders it.

**F. Memory.** `Code → Stop Language Server` → the meter says
`analyzer stopped` and RSS drops in Activity Monitor. `Start Language Server`
brings it back.

**G. Tools.** Ask *"search the web for the yew 0.23 changelog"* (needs the Brave
key, which is stored) and *"use explore to find where plugin instantiation
happens"*.

### 2.3 Known-untested edge paths

Worth a look if time allows; none are blocking.

- Project switch while a review modal is open (`review.abandon` answers the
  blocked call — the path exists and has never fired).
- Turn hitting `max_seconds` while blocked on a review.
- Settings dialog when the keychain is unavailable.
- A project with no Rust at all (the `symbol` tool should say "use grep").

---

## 3. Remaining work

### Milestone 5 — rendering

The largest single piece, deliberately last so everything beneath it is proven.

**Prerequisite, and it is small:** nothing in the app builds or loads a graph
yet. Add to `apps/smithy`:
- `CallGraph::build(root)` on a worker, triggered by an explicit menu action
  (`Agent → Build Call Graph`), never automatically — it costs ~10 s and 2.3 GB.
- Load `registry.callgraph_path(root)` at startup; hold it in `AgentState`.
- Show `staleness(root).describe()` in the header when non-empty.

**The view.** floem `canvas`; precedent for drawing in this codebase is
`squiggle.rs` (overlay geometry) and `celestial.rs` (projection).

- **Always focus-relative.** 2,221 nodes cannot be laid out usefully. Render the
  focus node, its callers above, its callees below, one hop by default, two on
  request. Cap at ~60 visible with `+N more`.
- **Layered, not force-directed.** Callers → focus → callees is a DAG-ish
  layering; a physics simulation would move nodes between frames and make the
  map unreadable as a *reference*. Deterministic layout matters more than
  prettiness here.
- Node width from the label; edge thickness from `Edge::sites`.
- **Stale nodes dim and dash**, driven by `node_is_stale`. This is the honesty
  requirement: the map must never present a snapshot as current.
- Interactions: click a node to re-focus; double-click to open `file:line` in
  the editor (`handle_file_open` already exists); hover for the signature;
  scroll to pan, ⌘-scroll to zoom.
- Empty state: no graph yet → a Build button and the cost, stated plainly.

**Where it lives.** A new panel, sharing the editor area — probably a tab
alongside the editor rather than a fourth resizable pane, since the layout is
already at three.

### Milestone 6 — live linking

What makes it verification rather than decoration.

- `AgentPanelState` (or a sibling) gains `touched: RwSignal<Vec<String>>` —
  `file:line` or qualified names the agent has read, edited, or looked up.
- Fill it from the existing `TurnEvent::ToolStarted`/`ToolFinished` stream in
  `app_state::setup_agent_effect`: `read`/`edit`/`write` give a path, `symbol`
  gives a name.
- The canvas highlights matching nodes, brightest most-recent, fading over ~30 s.
- Clicking a highlighted node re-focuses the graph there.

**The one design question left open:** whether the graph should auto-focus to
follow the agent, or stay where you put it and only highlight. Following is
impressive and makes the view useless for checking anything, because it moves
while you read. Recommend highlight-only, with a "follow agent" toggle default
off.

---

## 4. Landmines

Decisions already made that will look wrong without their reason.

**Do not put enum variants in the context map.** The map is prefilled on every
request; one 20-variant enum is ~150 tokens per turn for a fact the `symbol`
tool answers in one call. This was asked for and deliberately declined.

**Do not resolve calls by name.** Measured at 55% unambiguous on this workspace,
failing hardest on `new`, `default`, `run`. A name-matched graph draws confident
wrong edges — the exact disease the feature cures.

**SCIP `local N` symbols are document-scoped.** `local 0` in two files are
different things. Keying them globally produced an edge claiming one function
called another 38 times, and 449 fictional self-edges. They are excluded.

**`sources` must come from the indexer's document list, never from nodes.**
Files of `pub mod` declarations produce no nodes and would silently drop out,
then report as newly added forever.

**Reasoning must never enter `History`.** The endpoint does not replay it, and
putting it there changes the cached prefix every turn — a full cold prefill,
minutes at real context sizes. It lives in a sidecar; `into_history` still
round-trips byte-exactly.

**The write-review gate blocks.** A turn waiting on a review burns `max_seconds`
(900). That is a real cost, accepted deliberately, because the alternative was
the model spending a third of its turn guessing whether its edits landed.

**`SMITHY_LSP_LIGHT=1` and the call graph are compatible.** Light mode disables
`checkOnSave`; call-graph indexing needs the analyzer's inference, not its
cargo-check process. Stopping the server entirely is what would take the map
with it.

**Two bugs this session were caught by numbers looking implausible, not by
failing tests** — `rename ×38`, and seven files reporting as newly added. For
this feature, sanity-checking output against real data has been worth more than
the unit tests. Keep doing it.
