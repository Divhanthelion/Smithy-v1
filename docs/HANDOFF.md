# Handoff — 2026-08-03 session

Continuation of the prior handoff (`03c7e13`). This document is for the next
model/session: what was done, what the user verified by eye, what is still
broken, and the open design questions.

**Manual test project:** `~/Desktop/kernelos-master`  
**Install:** `cargo install --path apps/smithy --force` → `~/.cargo/bin/smithy`  
**Data dir:** `~/.local/share/smithy/`

---

## 1. Verdict in one paragraph

§2.1 automated floor passed (938 tests). Milestone 5 **plumbing works** — build,
persist, load, menu, center-pane switch — and a graph *appeared* on kernelos
(278 nodes · 355 edges). The **rendering is rejected by the user as totally
unacceptable** (see §3). Immediately after that first render, smithy also went
**Not Responding** (~563 MB); paint-path hang fixes are in `b7f525d` but
unverified in GUI. Keychain still prompted **three times**; see §5. Do **not**
treat M5 as done because a graph showed up — the layout is the open problem.

---

## 2. What this session changed (by theme)

### 2.1 Automated floor (§2.1) — PASSED

| Check | Result |
|---|---|
| `cargo test --workspace` | 938 passed |
| `cargo build --workspace` | OK (known warning: unused `longitude_from_timezone`) |
| CLI harnesses (symbols, scip, callgraph, models, transcript) | OK on this repo / kernelos as applicable |

### 2.2 GUI work done this session

**Meters / rust-analyzer attribution**
- Was summing *all* `rust-analyzer` processes on the machine (Claude Code’s ~9 GB
  + smithy’s). Fixed: attribute by PPID; show `+N elsewhere`; amber only on
  smithy’s analyzer.

**Empty-editor project map**
- Absolute-only stack collapsed; circuit showed through; ghost text invisible.
  Fixed layout + `FG_FAINT` + delivery via `app_state::bridge`.
- Clarified: this outline is *not* the Benzi map. Real map = Milestone 5 call
  graph. Outline uses `Project::outline()` (crates + modules only).

**Reconnect / model switch**
- Transcript notice: `Connected · {model}`.
- **Save & reconnect:** if provider/model/URL changed → `clear_context` (fresh
  session); else resume. Header Reconnect still resumes.
- User confirmed connection to `google/gemma-2-27b-it:free` (OpenRouter).

**Keyring spam (partial)**
- Settings open used to call `secrets::get` 3× (presence). Fixed:
  `secrets::is_stored` via `~/.local/share/smithy/key_presence.json` + process
  cache. Current presence file marks `openrouter-api-key` and `brave-api-key`.
- Launch used to unlock provider **and** Brave in one `spawn_blocking`. Brave
  is now deferred: register `WebSearch::deferred` when `is_stored`/`env` says a
  key exists; unlock only on first `web_search`.
- **User still got three password prompts.** See §5.

### 2.3 Milestone 5 — call graph UI (plumbing DONE, rendering REJECTED)

**New file:** `apps/smithy/src/call_graph.rs` (~800 lines)

**Wiring (OK)**
- `AgentState.call_graph: CallGraphUi`
- Center pane `dyn_container` swaps editor ↔ graph when `visible`
- Menus: `Agent → Build Call Graph`, `Agent → Show Call Graph`,
  `View → Call Graph` toggle
- Load from `registry.callgraph_path` on project open; clear on project switch
- Explicit build only (`CallGraph::build` ~10 s / ~2 GB); never auto

**First live render (kernelos)** — data OK, presentation not:
- Header: `278 nodes · 355 edges · 2 added since indexing`
- Focused `Terminal::execute_command` with `cmd_*` callees
- Persisted at
  `~/.local/share/smithy/projects/kernelos-master-ebfea94305350e37/callgraph.json`
- **User rejected the look** — see §3b. Treat as failed acceptance, not polish.

**Bug that made Build look like a no-op (fixed)**
- Menu actions created `Effect::new` with **no reactive owner** → disposed
  before the worker replied. Switched to settings-style `poll_once`.

**Hang after graph appeared (fixed in `b7f525d`, GUI retest pending)**
1. Canvas paint did `ui.size.set(...)` every frame → reactive loop.
2. `layout()` called `staleness()` every paint — now cached on `CallGraphUi.stale`.

---

## 3. What the user saw

Screenshot (saved in the Cursor assets for this chat):
`Screenshot_2026-08-03_at_1.54.28_AM-…png` — center pane titled Call graph,
focus `Terminal::execute_command`, one caller above (`Terminal::update`), a
horizontal strip of `cmd_*` callees below, forged perspective-grid backdrop
visible in the pane.

1. Graph **appeared** (data + wiring OK).
2. User: rendering is **totally unacceptable** — not a polish nit; the map fails
   as a readable reference. Prior handoff wrongly called this “Benzi-style
   success.” Correct that.
3. App then froze; Activity Monitor `smithy (Not Responding)`, ~563 MB. Hang
   fixes in `b7f525d`, GUI retest pending.
4. Keychain: **three** password entries despite “ask once” work.

§2.2 C–G not done this session.

---

## 3b. Why the graph looks bad — root causes (not “wrong data”)

The SCIP graph for that focus is plausible (a command dispatcher with many
`cmd_*` callees). The failure is **layout + presentation**, concentrated in
`apps/smithy/src/call_graph.rs` `layout` / `graph_pane`.

### What the screenshot shows going wrong

| Symptom | Cause in code |
|---|---|
| Callees crushed into one long horizontal strip; labels truncated / clipped at pane edges | `place_row` puts **every** neighbor on a **single Y**; no wrap, no column layout, no viewport width budget |
| Bottom row cut off by the panel | Layout is abstract (±`ROW_GAP`); no fit-to-pane / initial camera; high fan-out just spills |
| Unreadable fan of edges (comb) | All edges drawn focus-center → sibling centers; with 8–30 siblings this is noise, not a path you can follow |
| Lands on the worst case first | `default_focus` picks the **busiest** node → `execute_command`-style hubs by design |
| Perspective grid behind the nodes | Forged aesthetic shell is transparent; grid shows through / around the map. A reference diagram needs an opaque, quiet ground — not the celestial backdrop |
| “Walk without reading text” fails | Plan’s done criterion. Near-identical `cmd_*` pills in a strip force reading; nothing hierarchical or spatial distinguishes them |

### What is *not* wrong

- Edge correctness for this focus (caller/callee set matches a dispatcher).
- Layered-not-force-directed decision (still right; Benzi-ish verification map).
- Cap of ~60 / `+N more` (present, but we show too many in one row before hiding).

### What “tweaking” must mean (concrete)

Not cosmetics. The layout function has to become **viewport-aware** and
**degree-aware**:

1. **Wrap or columnize high-degree layers.** If callees won’t fit in ~pane width
   at readable label size, use multiple rows (or a vertical stack under the
   focus) and raise `+N more` earlier — never a single overflowing strip.
2. **Fit camera to content on focus change.** After layout, set pan/zoom so the
   focus + one hop are inside the pane with margin. Clipping the bottom row is a
   bug.
3. **Opaque map chrome.** Force `BG_BASE` (or a dedicated map ground) for the
   whole center pane; do not let the forged grid read as part of the graph.
4. **Smarter default focus.** Prefer a node with moderate degree (e.g. 3–12
   neighbors), or the last-edited / agent-touched symbol — not global max degree.
5. **Edge routing for wide layers.** Orthogonal or bundled stubs, or draw only
   to the nearest edge of each node, so a dispatcher doesn’t become a black fan.
6. **Hover = signature**, not only `file:line` (plan already required this).

Inspiration remains [Benzi](https://github.com/shobhitx64/Benzi) / CodeMap: a map
you **click through** to verify a path. Current UI is a star diagram of labels.

Until (1)–(3) land, **do not mark Milestone 5 rendering done.**

---

## 4. Keychain deep dive — why three prompts, and can we do better?

### 4.1 Facts on this machine

Three Keychain items under service `smithy`:

| Account | In `key_presence.json` |
|---|---|
| `openrouter-api-key` | yes |
| `brave-api-key` | yes |
| `deepseek-api-key` | present in keychain, **not** in presence sidecar |

Binary: ad-hoc / linker-signed (`Signature=adhoc`, no Team ID). Every
`cargo install --force` replaces `~/.cargo/bin/smithy` and **invalidates**
Keychain ACLs that trusted the previous code directory hash.

### 4.2 What the code unlocks today

| When | What calls `secrets::get` |
|---|---|
| Agent connect | **Only** the active provider key (`build_provider`) |
| First `web_search` | Brave (deferred) |
| Settings → refresh models | That provider’s `api_key()` |
| DeepSeek balance meter | DeepSeek key, only if DeepSeek is selected |

`is_stored` does **not** unlock. So at a clean OpenRouter launch after deferral,
the *app* should only touch OpenRouter once — **one** Keychain Access dialog
*if* the ACL still trusts this binary.

### 4.3 Why the user still saw three

macOS prompts **per keychain item**, not once per app session. Typical causes
stacked during this session:

1. **Ad-hoc binary churn.** Repeated `cargo install --force` = each new binary
   is a stranger to every item’s ACL. “Always Allow” for the previous build
   does not carry over.
2. **Three separate items.** OpenRouter, Brave, DeepSeek are three credentials.
   Touching each (launch + settings browse + any residual Brave path before
   deferral landed, or an older install) = three dialogs.
3. **Not “login password once for all keys.”** Unlocking the login keychain is
   separate from granting *this app* access to *this item*. The dialog the user
   sees is usually the ACL grant (“smithy wants to use …”), which is
   item-scoped.

So: yes, today it effectively wants an individual grant per API key (and again
after every reinstall of an unsigned binary). Deferring Brave only removes *one*
launch-time touch; it does not merge items or stabilize the binary identity.

### 4.4 Better options (for the next session to choose)

Ordered from “smallest change” to “real product.”

| Option | Pros | Cons |
|---|---|---|
| **A. Single vault item** — one Keychain account `smithy-secrets` holding JSON `{openrouter, brave, deepseek}` | One ACL grant forever (until binary changes) | Migration; one unlock exposes all keys to the process (already true once cached) |
| **B. Prefer env / `.env` for day-to-day** | Zero Keychain prompts | Secrets on disk; already supported as fallback |
| **C. Stable code signature** — Developer ID or even a stable ad-hoc identity with a fixed designated requirement, install via a fixed app bundle path | ACL survives rebuilds | Signing infrastructure; `cargo install` path is hostile to this |
| **D. Data Protection keychain** (`kSecUseDataProtectionKeychain`) | iOS-like; fewer ACL surprises | Needs `keyring`/Security API work; migration |
| **E. Stop reinstalling during test** — run `target/release/smithy` without replacing `~/.cargo/bin` | Immediate relief while iterating | Easy to forget; doesn’t help end users |
| **F. On first grant UI** — document “Always Allow”; open Keychain Access and set “Allow all applications” on the three items | Zero code | Weakens isolation; manual |

**Recommendation for consult:** **A + E short-term**, **C if shipping**. Deferral
(B-style presence checks) should stay. Do **not** call `get` for unused
providers on launch.

---

## 5. Remaining work

### Milestone 5 — rendering (BLOCKED: user rejected current layout)
- [ ] **Rewrite layout** per §3b (viewport fit, wrap/columnize high fan-out,
      opaque ground, better default focus, edge routing). Re-check against the
      same kernelos `Terminal::execute_command` focus — that hub is the
      acceptance test.
- [ ] Reinstall; confirm **no hang** after pan/zoom/click (hang fix unverified).
- [ ] Hover signature; double-click → file:line; Editor returns to buffer.
- [ ] Staleness: manual recheck / rebuild only (already cached off the paint path).

### Milestone 6 — live linking (not started)
- `touched` highlights from tool events; highlight-only; follow-agent off by
  default. See prior handoff §3 for the full sketch.

### §2.2 still open (C–G)
Attachments, write-review gate, budgets/reasoning, LSP stop/start,
`web_search` / `explore` against a live model.

### Keychain
- Decide A/B/C above; implement chosen approach; verify **one** dialog on a
  cold launch after a fresh install.

---

## 6. Landmines (still true)

- Do not name-match calls; SCIP `local N` is document-scoped; no enum variants
  in context map; reasoning never in `History`; write-review gate blocks;
  `SMITHY_LSP_LIGHT=1` OK with call graph.
- **floem:** Effects created outside a long-lived owner (menu clicks) die.
  Use `poll_once` / `exec_after` (settings pattern) or an effect owned by
  `app_view`.
- **floem:** Never `signal.set` from a paint/canvas path that another view
  tracks, unless guarded (“only if changed”). That is how this graph froze.
- **`CallGraph::staleness`:** tree walk + hash — never on the UI/paint path.
- **`cargo install --force`:** re-triggers macOS Keychain ACL prompts for
  ad-hoc binaries. Prefer running the build tree binary while iterating.

---

## 7. File map (this session’s touch surface)

| Path | Role |
|---|---|
| `apps/smithy/src/call_graph.rs` | **New** — UI, layout, build/load, hang fixes |
| `apps/smithy/src/main.rs` | Center-pane switch, menus |
| `apps/smithy/src/app_state.rs` | `CallGraphUi` on agent state |
| `apps/smithy/src/agent.rs` | Deferred Brave registration |
| `apps/smithy/src/settings.rs` | `is_stored` presence, no unlock on open |
| `apps/smithy/src/meters.rs` | PPID-scoped analyzer RSS |
| `apps/smithy/src/editor.rs` | Project map / empty editor |
| `crates/smithy-agent/src/config.rs` | `secrets::{get,set,is_stored}` + presence sidecar |
| `crates/smithy-tools/src/tools/web_search.rs` | `WebSearch::deferred` |
| `crates/smithy-project/src/context.rs` | `Project::outline` |
| `crates/smithy-editor/src/code_editor.rs` | Empty-editor map visibility |

Library call-graph / SCIP code from prior commits is unchanged in spirit;
this session was almost entirely app wiring + keyring UX + map rendering.

---

## 8. Suggested next-agent first moves

1. **Read §3b first.** Do not celebrate that a graph appeared.
2. Rewrite `layout` / camera / map chrome for high fan-out; acceptance case is
   kernelos focus `Terminal::execute_command` (or any hub with ≥8 callees) —
   labels fully visible, no overflow strip, no forged grid as backdrop.
3. Then: hang retest, then either keychain vault (§5) or §2.2 D (review gate).
