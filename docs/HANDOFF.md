# Handoff — 2026-08-03 session

Continuation of the prior handoff (`03c7e13`). This document is for the next
model/session: what was done, what the user verified by eye, what is still
broken, and the open design questions.

**Manual test project:** `~/Desktop/kernelos-master`  
**Install:** `cargo install --path apps/smithy --force` → `~/.cargo/bin/smithy`  
**Data dir:** `~/.local/share/smithy/`

---

## 1. Verdict in one paragraph

§2.1 automated floor passed (938 tests). Milestone 5 call-graph UI is wired and
**has rendered successfully** on kernelos (278 nodes · 355 edges · focus on
`Terminal::execute_command`). Immediately after, smithy went **Not Responding**
(~563 MB) — two paint-path bugs were then fixed in this commit (reactive size
loop + hashing the whole tree every frame); that hang fix is **compiled and
unit-tested but not yet re-verified in the GUI**. Keychain still prompted the
user **three times** despite deferring Brave; root cause is macOS per-item ACL
+ ad-hoc signed `cargo install` binaries, not “one password for the whole
keychain.” Better architectures are sketched in §5.

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

### 2.3 Milestone 5 — call graph UI (DONE, hang fixed in same commit)

**New file:** `apps/smithy/src/call_graph.rs` (~800 lines)

**Wiring**
- `AgentState.call_graph: CallGraphUi`
- Center pane `dyn_container` swaps editor ↔ graph when `visible`
- Menus: `Agent → Build Call Graph`, `Agent → Show Call Graph`,
  `View → Call Graph` toggle
- Load from `registry.callgraph_path` on project open; clear on project switch
- Explicit build only (`CallGraph::build` ~10 s / ~2 GB); never auto

**Behavior (verified by user screenshot)**
- Focus-relative layered layout (callers above, callees below)
- Header: `278 nodes · 355 edges · 2 added since indexing`
- Hop toggle, Rebuild, Editor
- Click to refocus; double-click → open file:line
- Persisted at
  `~/.local/share/smithy/projects/kernelos-master-ebfea94305350e37/callgraph.json`
  (~44 KB)

**Bug that made Build look like a no-op (fixed)**
- Menu actions created `Effect::new` with **no reactive owner** → disposed
  before the worker replied. Switched to settings-style `poll_once`
  (`exec_after` polling). Progress/errors also push `AgentEntry::Notice/Error`.

**Hang after graph appeared (fixed in this commit, GUI retest pending)**
1. Canvas paint did `ui.size.set(...)` every frame; `dyn_container` depended on
   `size` → infinite reactive rebuild → **Not Responding**.
2. `layout()` called `CallGraph::staleness(root)` every paint — full
   `ignore::Walk` + content-hash of every `.rs` file. Cached on
   `CallGraphUi.stale` at build/load instead.

---

## 3. What the user saw

1. Call graph **did** appear (Benzi-style focus map on `Terminal::execute_command`
   with `cmd_*` callees). Success for M5 rendering.
2. App then froze; Activity Monitor showed `smithy (Not Responding)`, ~563 MB,
   41 threads, low system memory pressure (64 GB machine, ~14 GB used). Not an
   OOM — a UI-thread / reactive hang.
3. Keychain: **three** password entries despite “ask once” work.

§2.2 checklist items C–G (attachments, review gate, budgets, LSP stop/start,
web_search/explore) were **not** completed this session.

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

### Milestone 5 polish
- [ ] Reinstall, reopen kernelos, confirm graph still builds and **does not hang**
      after pan/zoom/click (hang fix unverified in GUI).
- [ ] Staleness refresh only on Rebuild / project focus — not continuously (now
      cached; may want a manual “Recheck freshness”).
- [ ] Confirm double-click opens editor at line; Editor button returns to buffer.

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

1. `cargo install --path apps/smithy --force` once; launch
   `smithy ~/Desktop/kernelos-master`.
2. Expect possibly **one** Keychain dialog (OpenRouter) if ACL was reset by
   install — note count carefully.
3. `Agent → Build Call Graph` (or Show if `callgraph.json` exists). Pan, zoom,
   click a callee, double-click open. Confirm no hang.
4. Then either implement **single vault keychain item (§4.4 A)** or resume
   §2.2 D (review gate) — highest product value after map stability.
