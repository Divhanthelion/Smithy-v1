# Smithy

> **Work in progress.** It runs, and the parts described below as verified have
> been used rather than merely tested. It is not finished and not stable.
> [What is verified, and what is not](#what-is-verified-and-what-is-not) is a
> section of this file rather than a footnote — four subsystems here have
> shipped fully green and completely inert, so the distinction between "the
> tests pass" and "someone has seen it work" is load-bearing.

A native Rust IDE with a local-first coding agent. Open a project, edit files,
and ask an agent to do work in it — running against a model on your own machine,
through a capability-sandboxed tool layer with human review on writes and shell
commands.

*(`smithy` is a placeholder name; nothing depends on it beyond crate names.)*

## Running it

```bash
cargo run -p smithy                        # opens the last project you had open
cargo run -p smithy -- ~/code/thing        # or a specific one
cargo test --workspace                     # 726 passing, 0 warnings
```

Two examples, both useful when something is not behaving:

```bash
cargo run -p smithy-agent --example smoke  # live end-to-end check; needs LM Studio up
cargo run -p smithy-project --example dump # exactly what the model is told about a project
```

### What you need

The editor, the terminal and the file browser work on their own. The **agent**
needs a local OpenAI-compatible endpoint — [LM Studio](https://lmstudio.ai) by
default, with a tool-capable model loaded. Smithy checks at startup that the
model is not merely downloaded but actually resident, and says so if it is not.

**One LM Studio setting matters:** context overflow must be **"stop at limit"**,
not truncate-middle or rolling-window. Both of those mutate the prompt prefix,
which throws away the model's KV cache on every turn and can split a tool call
from its result. Smithy stops cleanly below the ceiling on its own, so this
should never fire.

If the server is not up when Smithy launches, the agent panel shows a red dot
and a **Reconnect** control — start the server, click it.

### Configuration

Everything is an environment variable; there is no settings file.

| variable | what it does |
|---|---|
| `LMSTUDIO_URL` | endpoint, default `http://localhost:1234/v1` |
| `LMSTUDIO_MODEL` | model id, default `qwen3.6-27b`. Resolved against what the server actually has loaded, so a quantisation suffix needs no reconfiguration |
| `SMITHY_WORKER_THREADS` | tokio workers. Defaults to the core count, clamped — deliberately modest, because the machine is also serving a model |
| `SMITHY_LSP_LIGHT=1` | drops rust-analyzer's `checkOnSave`. The largest single memory saving available, at the cost of real compiler diagnostics |

Four debug flags, each existing because a layer had several ways to fail that
looked identical from outside:

| variable | reports |
|---|---|
| `SMITHY_VOICE_DEBUG=1` | every stage of loading and transcription — see [Dictation](#dictation) |
| `SMITHY_KEY_DEBUG=1` | every key event a handler receives |
| `SMITHY_SQUIGGLE_DEBUG=1` | what the diagnostic-squiggle layer resolved: ranges, visible rows, runs, transform |
| `SMITHY_SKY_DEBUG=1` | sky phase, darkness, and how many stars are up against how many are painted |
| `SMITHY_FISHERMAN_DEBUG=1` | the solar clock, today's sun, and what the mascot is doing where |

## Layout

| crate | what it is |
|---|---|
| `apps/smithy` | the application shell — the binary |
| `smithy-editor` | editor UI: panels, menus, design tokens, syntax styling, LSP client, terminal, file browser, ornament |
| `smithy-tools` | the agent's tools and the capability sandbox |
| `smithy-agent` | the agent loop, budgets, persistence, LM Studio provider |
| `smithy-project` | project detection and context extraction |
| `smithy-sky` | where everything in the sky is. No dependencies at all |
| `smithy-voice` | microphone in, string out. Whisper in-process |

**`smithy-agent`, `smithy-tools`, `smithy-sky` and `smithy-voice` have no floem
dependency.** A second front-end — a TUI, or something more ornate — is a new
consumer of the same core, not a rewrite. That constraint is worth preserving.

**`smithy-editor`'s public surface is only what `apps/smithy` uses.** Everything
else is `pub(crate)`, so `dead_code` can see it — `pub` items in a library are
exempt from that lint, and this crate inherited a great deal of API that had no
caller and warned about nothing.

## How it fits together

The agent runs on a tokio runtime; the UI runs on floem's main thread. They
communicate over a crossbeam channel that floem drains into a signal. Nothing in
the agent or tool layer references a UI type.

Human-in-the-loop gates are `ToolHook`s registered on the tool registry, not
special cases inside the loop:

| gate | intercepts | behaviour |
|---|---|---|
| write review | `write`, `edit` | computes the resulting diff, queues it, and **denies** the tool — so the model is told the change awaits review rather than believing it landed |
| shell approval | `bash` | suspends the loop on a oneshot until the modal answers; fails closed if the UI is gone |

The loop only ever sees a tool that ran or was refused.

Review is per hunk: each hunk gets Apply or Skip, and only the accepted ones are
written. The outcome then reaches the model at the head of its next turn rather
than as that call's tool result, which is frozen in history by the time the user
decides. That matters most for a partial acceptance — the file is then in a state
the model has never seen, so it is told to re-read before editing again.

## Dictation

The microphone button in the agent panel — or `⌘⇧V` — transcribes speech into
the prompt box. **Nothing is uploaded**: Whisper runs in this process, through
`candle`, on a dedicated thread.

### Getting it working

There is nothing to install and no key to obtain. The first press downloads the
model; every press after that, and every launch after that, is immediate and
works offline.

| | |
|---|---|
| Model | [`openai/whisper-large-v3-turbo`](https://huggingface.co/openai/whisper-large-v3-turbo) |
| Downloaded | on the **first press** of the microphone, not at launch |
| Size | roughly 1.6 GB (`model.safetensors`, `config.json`, `tokenizer.json`) |
| Stored in | `~/.local/share/smithy/models` |
| Needs | a network connection **once**, and no Hugging Face account |

`large-v3-turbo` rather than plain `large-v3`: about eight times faster to
decode for a barely measurable accuracy cost, and dictation is judged on the
wait.

**The first press only loads the model — it does not start recording.** The
download takes tens of seconds, and a microphone opening silently at the end of
it would be recording a room whose owner had looked away minutes ago. Press
again once the button is ready.

Dictation **appends** rather than replacing: say a sentence, read it, say
another. There is no undo on a text box somebody is mid-thought in.

To fetch the weights yourself — an air-gapped machine, or a slow link:

```bash
huggingface-cli download openai/whisper-large-v3-turbo \
  --cache-dir ~/.local/share/smithy/models
```

### When it does not work

```bash
SMITHY_VOICE_DEBUG=1 cargo run -p smithy
```

Every stage reports: which input device was chosen, how many samples were
captured and for how long, and how long decoding took. Without it, "no network",
"no cache directory", "no microphone" and "bad tokenizer" all look identical
from outside — a button that does nothing.

macOS asks for microphone permission the first time. If it was refused, the
button reports `no microphone`, and the fix is in System Settings.

## The decisions that matter

Each has a non-obvious reason; changing one without understanding it causes a
subtle regression.

**History is append-only.** Prefix caching is a strict byte-prefix match, so
mutating an earlier turn costs a full cold prefill — minutes at real context
sizes. `History` exposes no `remove`, `truncate`, or `get_mut`.

**Project context goes in the system prompt, once.** `cargo metadata` plus
tree-sitter produce crate layout, dependencies, module tree, and public API
signatures. Injected at the head of the prompt so it sits inside the cached
prefix rather than being re-sent per turn. Budgeted in layers and dropped from
the bottom; truncation is announced so the model greps instead of assuming
absence. It is skipped entirely when resuming, because a resumed session replays
its stored prompt verbatim.

**Persistence replays verbatim.** Store the messages, replay the exact bytes. A
test asserts the round-trip is byte-identical, because otherwise resuming a
conversation costs a cold prefill.

**The sandbox is a capability.** `Workspace` holds a `cap-std` `Dir`; the OS
refuses escapes, symlinks included. A lexical pre-check survives only to produce
a better error message. The two tools that cannot use the capability — `grep`
and `glob`, which hand a path to a directory walker — go through
`Workspace::absolute_real`, which canonicalises and re-checks containment. A
symlink out of the workspace was a real escape before that existed.

**Empty or truncated answers are failures, not finishes.** At high context the
model can reason correctly while emitting nothing; treating that as success
produces a silent no-op turn.

**floem owns the text.** The editor is floem's `text_editor` — caret, selection,
undo, clipboard all come from it. `Buffer` is a loader and metadata type,
explicitly *not* a second editing model. Syntax colours and inline diagnostics
share one `Styling` implementation so they cannot disagree about a range.

**Everything derived from the open project is read live, never snapshotted.**
Five separate bugs came from capturing the project root once at startup, the
worst of which wrote an accepted file review into the wrong repository.

## What is verified, and what is not

Both matter, and they are not the same thing. Everything below passes
`cargo test`; the distinction is whether a person has watched it work.

**Used, and working**

- Project open, edit, save, tab bar, resizable panels, terminal with scrollback
- Syntax highlighting, inline diagnostics, Problems panel, hover (`⌃K`),
  go-to-definition (`F12`)
- Agent turns end to end: streaming, tool calls, the write-review diff modal
  including partial hunk acceptance, and shell approval
- Session persistence and restore across restarts
- **Dictation**, start to finish
- **Reconnect**, for launching before the model server is up
- Typing in large files — a keystroke reparses incrementally, 0.75 ms on a
  1,900-line file and 10 ms on a 13,000-line one

**Built and tested, never seen**

- **Diagnostic squiggles.** The geometry is tested and the layer is wired; no
  screenshot has yet shown a wave under a real diagnostic.
  `SMITHY_SQUIGGLE_DEBUG=1` says which of the four links broke.
- **The external-change bar.** Raised when a file changes on disk *while open
  and with unsaved edits* — a clean file reloads silently instead. Nobody has
  arranged that situation.
- Parts of the ornament: the hut, the build sequence, the sun on its arc. All
  confirmed painting; none confirmed as looking right.

**Known limitations**

- **Reconnect does not notice a model unloaded underneath it.** The connection
  flag is set once, at startup preflight, and the control only appears while it
  is false. Unload the model in LM Studio and the dot stays green; the next turn
  fails into the transcript with no offer to retry. Restarting Smithy clears it.
- **A tool call cannot be interrupted.** Stop is checked between tool calls and
  before each model call, never inside one. A shell command awaiting approval,
  or a slow tool, ignores Stop until it returns.
- **Stale project context is not detected.** The description in the system
  prompt is a snapshot from when the session started and is deliberately not
  refreshed — the prompt is frozen for cache reasons. Nothing notices when the
  project has moved on; the answer is a new session.
- **`⌘`-shortcuts also accept `Ctrl`,** which the embedded terminal wants for
  itself: `Ctrl-L`, `Ctrl-B`, `Ctrl-S` and `Ctrl-O` are intercepted before the
  terminal sees them.
- Only rust-analyzer is exercised. Other language servers are configured and
  untried.

## Origins

Smithy consolidates six earlier prototypes (`forge`, `coda`, `divcli`,
`rustcoder`, `kimi-sec`, `app-ottex`) that had independently built overlapping
pieces — four LLM clients, four tool registries, four agent loops. Those
prototypes were the starting point, not the design; most of what they contained
has since been replaced or deleted outright.

Not carried forward: `rig-core` (awkward wrapping — a hand-rolled wire protocol
is cleaner), kimi-sec's hand-written SAST (1,030 findings on a real repo, nearly
all false), rustcoder's compiler-error knowledge base (built for weaker models,
never verified against current ones), and `shanegillis` (Python/MLX, out of
scope).

## Licence

MIT.
