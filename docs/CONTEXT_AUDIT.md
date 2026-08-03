# Context audit — what we send, what it costs, what we cannot see

Two questions: **are we giving the models good concise context**, and **can we
build the per-segment usage panel Cursor has**. They turn out to be the same
question, because the reason nobody can answer the first is that nothing
measures it.

Measured on this workspace, 2026-08-03.

---

## 1. What a cold session actually costs

| Segment | Measured | How |
|---|---|---|
| System prompt (base) | 2,155 chars ≈ **540 tok** | the literal at `session.rs:539` |
| Project context block | 24,106 chars ≈ **6,026 tok** | `cargo run -p smithy-project --example dump .` |
| Tool schemas (core 8, internal form) | 4,711 chars ≈ **1,177 tok** | `Registry::core().definitions()` serialized |
| Tool schemas (OpenAI wire form) | larger — `{"type":"function",…}` + JSON-schema wrapping | `Registry::openai_schemas()` |
| Plus `web_fetch`, `web_search`, `symbol` | not in `core()`; added in `agent.rs:326–360` | |

So roughly **8k tokens before the user types a character**, on a workspace of
seven crates. That is a reasonable number, and the layering design in
`context.rs` is the reason — layout, deps, modules, API, truncate from the
bottom, and *say so in the text*. That last part is genuinely good and rarer
than it should be.

The problems are not in what the first request costs. They are in what the
hundredth costs, and in the fact that nothing on the machine can tell you.

---

## 2. Findings

### A. The context ceiling is a wall, not a valve — and it lets the expensive call through

`Budget::new` is called **inside** `run_turn_inner` (`session.rs:300`), so
`last_prompt_tokens` starts at `0` on every turn. `tick()` checks it *before*
the request. Therefore:

> The context ceiling can never stop the first provider call of a turn.

At 130k of history against `context_hard = 110_000`, every turn: makes one full
130k-token prefill, gets an answer, records 130k, and stops on the *next* tick.
The user is billed for a call that was guaranteed to be thrown away, once per
turn, forever, with no way out but "clear context" — which discards everything.

There is also no compaction at all. `message.rs:128` is explicit and correct
about why:

> There is deliberately no `remove`, `truncate`, `insert`, or `get_mut`. If a
> compaction strategy is ever added it must append a summary and start a new […]

Append-only is the right invariant — it is what keeps the prefix cache valid.
But it was chosen *and then nothing was built on top of it*, so the only tool
for a long session is a guillotine.

**Fix, in two sizes:**

1. *Small, today.* Carry `last_prompt_tokens` on `Session`, seed the new
   `Budget` from it. A doomed turn then refuses before the network call and says
   why. Perhaps twenty lines.
2. *Real.* Compaction, done the way `message.rs` prescribes: keep the system
   prompt (the cache root), append a generated summary of the dropped span, keep
   the last N turns verbatim, start a new history. **Note the tension and design
   around it:** compaction invalidates the prefix cache by definition. So it
   must be a rare, deliberate, user-visible event — offered at ~70 % of window,
   done on confirm, showing what is about to be dropped. Not automatic, not
   silent, not every turn.

### B. The project block is priced as if it were paid once. It is paid every request.

`ContextBudget::for_window` gives 5 % of the window, floor 6k tokens, ceiling
40k. The reasoning is sound as far as it goes, and the comment is right that a
1M-token model should not be handed the same 6k a 32k model gets.

But the block is frozen into the system prompt precisely so it sits in the
cached prefix — which means it is **prefilled on every request of the session**.
A 60-step turn against a 1M model carries 40k tokens of `pub fn` signatures 60
times. Sizing it as a share of the *window* prices it like a one-off; it is a
per-request rent.

Worse, the system prompt already tells the model not to trust it for detail:

> The project summary below is a *map*: it tells you what exists, not what shape
> it has. Guessing a variant name or an argument count from the map is the single
> commonest way to write code that does not compile.

If the model must call `symbol` before using anything anyway, then every
signature past the point where the *map* is complete is being paid for on every
request to deliver information the model has been instructed to re-fetch.

**Recommendation:** re-derive the ceiling from expected requests per session
rather than window share. Concretely, cap around **8–10k tokens regardless of
window**, and let `symbol` serve the tail. Then measure: run the same task at
8k and at 40k and compare tool-call counts and total prompt tokens. This project
measures things; this is worth measuring rather than arguing.

### C. The API layer is 6k tokens of undifferentiated list

`render_api` emits signatures per crate in source order. A model receives
`pub fn arg_bool`, `pub struct ToolParameter`, and `pub fn run_turn` at equal
weight, and the truncation notice fires with no principle about *what* got cut
beyond "the end of the list."

Three cheap improvements, in order of value:

1. **Rank by fan-in.** This repo now builds and persists a call graph
   (`smithy-project/src/callgraph.rs`, ~278 nodes / ~360 edges on the test
   project). Node degree is exactly "how central is this symbol." Ordering the
   API layer by degree — and truncating from the *bottom* of that order — makes
   the same 6k tokens carry the API that actually gets called. Two subsystems
   that already exist, joined by a sort.
2. **Drop the obvious noise.** `arg_str`, `arg_i64`, `arg_bool` and friends are
   in the block today. They are argument-parsing helpers; nobody needs them in a
   map.
3. **First doc line for the top items, instead of signatures for all of them.**
   `pub fn extract(project, budget) -> ProjectContext` says less than
   `extract — build the context block for a project`. For the top ~50 by degree,
   the doc line is the better token.

### D. Nothing caps tool results in aggregate

Per-call caps exist and are sensible: `read` 2000 lines, `grep` 200 matches ×
400 chars, `glob` 300, `ls` 500, `web_fetch` 64,000 chars. But nothing bounds
their **sum across a turn**, and one `web_fetch` at default is ~16k tokens —
about 15 % of `context_hard` in a single call. Three greps and two fetches clear
`context_warn` inside five steps.

**Fix:** track cumulative tool-result chars in `Budget`. Past a threshold, start
appending a line *to the tool result itself*: "results are being trimmed; narrow
your query." This is append-only-safe, needs no history rewriting, and puts the
signal where the model will act on it.

### E. `context_warn` is flat and fires once

`context_warn = 32_000` is a constant. `agent.rs:448` adjusts `context_hard`
when the model reports a real window, but `warn` stays put. On a 1M-token model
it trips almost immediately and — being warn-once by design, correctly — is then
silent for the rest of the session. The one context signal the user gets is
miscalibrated on exactly the models where context management matters most.
Make it a fraction of the window, same as the hard ceiling.

### F. We built the whole thing for prefix caching and never measured whether it works

This is the important one.

The architecture is *organised* around a byte-stable cached prefix. It is the
stated reason for: a fixed tool order (`registry.rs:129`), `Vec` instead of
`HashMap` in the schema (`schema.rs:26`), freezing the project block into the
system prompt (`session.rs:529`), never gating tools mid-session, appending the
step warning at one specific point in the loop, and keeping reasoning out of
`History`. Six separate design decisions, all paying rent to the same idea.

And then:

```rust
// providers/sse.rs:34–41 — everything we read from `usage`
out.prompt_tokens          = usage.prompt_tokens
out.completion_tokens      = usage.completion_tokens
out.reasoning_tokens       = usage.completion_tokens_details.reasoning_tokens
```

Cached-token fields are **dropped**. OpenAI-compatible endpoints report
`usage.prompt_tokens_details.cached_tokens`; DeepSeek — a first-class provider
here (`providers/deepseek.rs`) — reports `prompt_cache_hit_tokens` and
`prompt_cache_miss_tokens`. None of it is read.

Two consequences:

1. **The cost meter overstates cost.** `Usage::cost` multiplies all prompt
   tokens by the full prompt rate. Cached tokens bill at a fraction (DeepSeek
   roughly a tenth). The number in the menu bar is wrong in the direction that
   makes the codebase's own best feature look expensive.
2. **Nothing notices when the cache breaks.** If someone makes the tools array
   or the system prompt vary between turns — the exact failure all six decisions
   above exist to prevent — the hit rate collapses, latency and cost roughly
   double, and there is no indicator anywhere. The invariant is enforced by
   comments and one byte-stability test; it is not observed in production.

**Fix:** read the cache fields in `sse.rs`, carry `cached_tokens` on
`Completion` and `Usage`, price it separately in `cost()`, and surface hit rate.
Confirm the field names from live frames rather than from docs — the providers
disagree and this project measures.

This single change is worth more than everything else in this document: it turns
the central architectural bet from an assertion into a reading.

---

## 3. What is right, and should not be "improved"

Worth writing down so a later pass does not helpfully undo it:

- **Layered context with truncation from the bottom, and a truncation notice in
  the text.** "Silence here would teach the model that an item it cannot see
  does not exist" is exactly correct.
- **Token counts from the endpoint, never from a local tokenizer.** The comment
  at `session.rs:84` is right, and §4 below is designed not to violate it.
- **Reasoning stored in a sidecar, never in `History`.**
- **Append-only history.** The invariant is good; it just needs something built
  on it (§2A).
- **The step warning at 4/5, once, with instructions.** Model-facing budget
  signals are usually either absent or nagging. This is neither.

---

## 4. The Context Usage panel

Yes, and most of the data already exists.

### 4.1 Mapping

| Cursor's row | Smithy |
|---|---|
| System prompt | `default_system_prompt` base — 540 tok measured |
| Rules | — (the project block is the analogue) |
| Tool definitions | `Registry::openai_schemas()` |
| MCP & dynamic tools | the conditionally-added `web_search` / `web_fetch` / `symbol` |
| Subagent definitions | n/a |
| Summarized conversation | n/a **until compaction exists** (§2A) — then it appears |
| Conversation | `History` |
| — | **Attachments** (already tracked: `attachment::total_tokens`) |
| — | **Project context** (already has `char_len()`, `layers`, `warnings`) |

Already in the UI: `AgentPanel` has `context_tokens`, `context_limit`,
`context_label` and a thin bar (`agent_panel.rs:88–95, 1118`). What is missing is
**attribution**, and the app already shows the total in two places, so the panel
is an expansion of something real rather than a new subsystem.

### 4.2 How to attribute without adding a tokenizer

The endpoint reports one number: `prompt_tokens`. Everything else is local. The
honest construction:

1. Every segment reports **chars** — all of them are `String` or `Value`, so
   this is `.len()`.
2. Estimate tokens as `chars / 4`, as `context.rs` and `attachment.rs` already do.
3. On each completion, compute
   `calibration = actual_prompt_tokens / Σ(estimated)`
   and scale every segment by it.

The result: **the breakdown sums exactly to the billed number**, so the panel can
never contradict the meter, while the *split between rows* is an estimate.
Zero dependencies, self-correcting per model and per tokenizer, and it does not
introduce the "second opinion that is wrong in a way nobody notices until the
invoice" that `session.rs:84` rules out.

State the limitation in the UI: a single scalar under-attributes token-dense
segments (JSON tool schemas, minified code in a `read` result) relative to prose.
For a usage panel that is fine. Say it in a tooltip; do not pretend to precision.

### 4.3 Where it comes from

`Session` already holds everything. No new plumbing, no new state:

```rust
impl Session {
    /// Chars per segment, as of right now. Cheap: no serialization of anything
    /// not already serialized, no walking the workspace.
    pub fn ledger(&self) -> ContextLedger;
}
```

- system base + project block: `history` message 0, split on the known joiner
  (or better: keep the two lengths on `SessionConfig` at construction, which
  removes the string-searching entirely)
- tools: `self.tools` is already a serialized `Value`
- conversation: `self.history`, per message, with role
- attachments: the panel already has them
- reasoning: report as **"generated, not sent"** — it is real spend that never
  enters the prefix, and showing it explains a completion-token bill that
  otherwise looks impossible

### 4.4 Two rows Cursor does not have, and should

1. **Cached vs cold.** Shade the portion of the bar served from prefix cache on
   the last request. This is the number that says whether the architecture is
   working, and it is the natural home for §2F.
2. **Frozen vs live.** System prompt, project block and tools are fixed for the
   life of the session; only the conversation grows. Drawing the frozen part
   differently makes the available levers obvious: *clear context* only touches
   the bottom half, and the top half needs a new session. Right now users learn
   that by being surprised.

### 4.5 The landmine

**Compute the ledger on completion, once per request, stash it in a signal, and
only read it while painting.**

HANDOFF §6 already lists this failure twice — "never unconditional `signal.set`
from a paint/canvas path" and "`CallGraph::staleness` never on the UI/paint
path." Serializing `self.tools` or walking `History` inside a `Label::derived`
would be the same bug a third time, and this one would be paid at 60 Hz.

---

## 5. Suggested order

Ranked by value per unit of work.

| # | Item | Why first |
|---|---|---|
| 1 | §2F cached tokens | Turns the central bet into a measurement. Fixes the cost meter. Small. |
| 2 | §2A carry `last_prompt_tokens` across turns | Stops billing for calls that are guaranteed to be discarded. ~20 lines. |
| 3 | §2E `context_warn` as a fraction of window | One line; the signal is currently useless on big models. |
| 4 | §4 `Session::ledger()` + panel | Now everything above has somewhere to be seen. |
| 5 | §2D per-turn tool-result budget | Closes the last uncapped inflow. |
| 6 | §2C rank the API layer by call-graph degree | Best tokens-per-token improvement available; needs the graph, which exists. |
| 7 | §2B re-derive the block ceiling, then **measure** | Do it after 4, so the panel can show the before/after. |
| 8 | §2A compaction | Largest, and wants the panel first so the drop is visible. |
