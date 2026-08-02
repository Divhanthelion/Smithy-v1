# Smithy

**A native Rust IDE with a coding agent that runs on your own machine.**

Open a project, edit files, and ask an agent to do the work — against a model
you host yourself. Nothing is uploaded, no API key is needed, and it keeps
working with the network off.

The agent can read your code, search it, run commands and write files. Every
write comes back as a diff you approve hunk by hunk, and every shell command
waits for your go-ahead. It never touches anything outside the project you have
open.

---

## Getting started

You need [Rust](https://rustup.rs) and, for the agent,
[LM Studio](https://lmstudio.ai) with a tool-capable model loaded.

```bash
git clone <this repo>
cd smithy
cargo run -p smithy -- ~/code/your-project
```

That's it. After the first launch, a bare `cargo run -p smithy` reopens whatever
you had open last.

The editor, terminal, file browser and language-server features all work without
LM Studio — you just won't have an agent.

### Pointing it at a model

Load any tool-capable model in LM Studio and start the server. Smithy checks at
launch that the model is actually resident in memory, not merely downloaded, and
tells you which if it isn't.

If the server wasn't running when Smithy started, the agent panel shows a red
dot and a **Reconnect** button. Start the server, click it.

To switch backend or model, open **Agent → Backend Settings…** (or the gear in
the agent panel header). Pick LM Studio or OpenRouter, choose a model, and press
**Save & reconnect** — the session rebuilds against the new endpoint. No restart,
no dotfile.

The dialog lists what each backend actually offers, fetched when it opens:

- **OpenRouter** — the full catalogue, free tier first. **Free only** is on by
  default; switch it off for paid models. Each row shows its context window and
  price per million tokens. The catalogue is public, so the list populates before
  you have a key (you still need one to *call* anything, including free models).
- **LM Studio** — everything downloaded locally, resident models first, with size
  and context. **Load** makes one resident without leaving the editor. That's
  optional — LM Studio's JIT loader pulls in an unloaded model on the first
  request — but it turns a minute of apparent hang into a progress line.

**Tool-capable** is on by default and should stay on. Smithy's loop is entirely
tool-driven, so a model that can't emit `tool_calls` doesn't give worse answers,
it gives empty turns. It's a real filter on both backends: several free
OpenRouter models are classifiers or audio models, and a typical LM Studio
library has TTS and ASR entries that LM Studio itself types as `llm`.

The model field stays editable — a picker that replaced it would make a
brand-new id, or a self-hosted endpoint, unreachable.

To see the same lists from a terminal:

```bash
cargo run -p smithy-agent --example models -- openrouter
```

API keys go to your OS credential store — Keychain on macOS — not to the settings
file. The settings file holds the endpoint and model name only, and the dialog
never displays a stored key back to you.

Environment variables still work and are still read; they're just no longer the
only way. Precedence is: the settings file wins if you've ever saved one, and the
environment fills in when you haven't — so an existing `.env` keeps working
untouched until the first time you press Save.

```bash
SMITHY_PROVIDER=openrouter OPENROUTER_API_KEY=sk-or-v1-... OPENROUTER_MODEL=anthropic/claude-3.5-sonnet cargo run -p smithy
```

The model name is matched against what the server actually has loaded, so a
quantisation suffix like `@8bit` doesn't need configuring.

### Giving it context

Drag files or folders onto the agent panel. They appear as chips above the
composer with their size and token cost; click one to include or exclude it, and
the row totals what the next message will spend. Attachments go out with that one
message and are then cleared — they're already in the conversation's history, so
re-sending them would just cost twice.

A dropped folder is walked gitignore-aware and skips dotfiles, so dropping a
project doesn't paste `target/` or `.env` into a prompt. Binaries and anything
over 256 kB are named rather than inlined, so the agent knows they exist and can
`read` a slice.

**New session** in the panel header (or **Agent → New Session**) is the one that
forgets. The `↺` icon beside it only clears the transcript you're looking at —
the model still remembered everything. New Session throws away the history, the
pending review bookkeeping, and rebuilds with a freshly extracted project
context. The previous conversation stays on disk rather than being deleted.

### Searching and research

With a Brave Search API key set under Backend Settings, the agent gets
`web_search`. Without one it still gets `web_fetch`, so it can read any URL you
or it names — it just can't discover URLs. `web_fetch` refuses non-http schemes
and private/loopback addresses, including after a redirect.

It also gets `explore`: a read-only sub-agent that answers one bounded question
by searching on its own and returning a short written answer with `path:line`
citations. Its intermediate reads stay in its own context instead of filling
yours. It can't write, edit, run commands, or call itself, and it stops after
about a dozen tool calls and reports partially rather than grinding.

### Reference setup

This is what Smithy is developed against, if you want a known-good starting
point:

| | |
|---|---|
| Model | **Qwen 3.6 27B** (`qwen3.6-27b`) — the default |
| Context Overflow | **Stop at Limit** |
| Limit Response Length | off |

**Context Overflow is the one that matters**, and it is the only LM Studio
setting Smithy cannot control from its side. Set it to *Stop at Limit* rather
than truncate-middle or rolling-window: the other two rewrite the beginning of
the conversation behind Smithy's back, which throws away the model's cache and
makes every subsequent turn slower. Smithy tracks its own context budget and
stops cleanly before the ceiling, so this should never actually fire.

Everything under **Sampling** in LM Studio is overridden — Smithy sends these
with every request, so the sliders in the UI don't affect agent turns:

| | |
|---|---|
| Temperature | 0.6 |
| Top P | 0.95 |
| Top K | 20 |
| Min P | 0.03 |
| Repeat penalty | 1.0 |
| Max tokens | 16384 |

Generous `max_tokens` is deliberate: a reasoning block cut off mid-thought never
emits its closing tag, and running out of output budget costs more than spending
it. If you want different values, they live in `Sampling::default()` in
`crates/smithy-agent/src/provider.rs`.

---

## Using it

### The agent

Type what you want in the panel on the right and press send. The agent works in
steps — you see each tool call as it happens, and its reasoning as it streams.

When it wants to **change a file**, you get a diff. Every hunk starts marked
Apply, so approving everything is one click; skip the ones you don't want and
the button tells you what it's about to write ("Apply 2 of 5"). Only the hunks
you kept are written, and the agent is told exactly what you decided.

When it wants to **run a command**, you see the command and approve or decline.
Declining tells it why so it can try something else.

**Stop** ends the current turn at its next step.

### Keyboard

On macOS the primary modifier is `⌘`; elsewhere it's `⌃`.

| | |
|---|---|
| `⌘O` | Open project |
| `⌘S` | Save |
| `⌘B` | Toggle the file explorer |
| `⌘L` | Toggle the agent panel |
| `⌃\`` | Toggle the terminal |
| `⌘⇧V` | Dictate into the prompt box |
| `⌃K` | Hover — types and docs at the cursor |
| `F12` | Go to definition |
| `⌘Z` / `⌘⇧Z` | Undo / redo |
| `⌘X` `⌘C` `⌘V` `⌘A` | Cut, copy, paste, select all |

The **View** menu also toggles the Problems panel and a clock, and **Switch
Look** flips between the plain interface and an ornamented one. Your choice is
remembered.

### Editing

Syntax highlighting for Rust, Python, JavaScript, TypeScript, TSX, Go, C, C++,
JSON, HTML and CSS.

For Rust projects, `rust-analyzer` runs automatically if it's on your `PATH`
(`rustup component add rust-analyzer`). You get errors and warnings underlined
in the editor, a Problems panel listing them, hover, and go-to-definition.

Files changed outside the editor are picked up automatically. If the file is
clean it reloads silently; if you have unsaved edits you get a bar offering to
keep yours or take the version on disk. Nothing is discarded without asking.

### Dictation

Press the microphone in the agent panel, or `⌘⇧V`, and talk. Speech is
transcribed by Whisper **running in this process** — nothing is uploaded.

The first press downloads the model, roughly 1.6 GB, into
`~/.local/share/smithy/models`. It takes tens of seconds, needs the network
once, and needs no Hugging Face account. **That first press only loads the
model — it doesn't start recording.** Press again once it's ready, so the
microphone opens when you're actually about to speak.

Every press after that, and every launch after that, is immediate and works
offline.

Dictation appends rather than replacing, so you can say a sentence, read it,
and say another.

<details>
<summary>Fetching the model yourself</summary>

For an air-gapped machine or a slow link:

```bash
huggingface-cli download openai/whisper-large-v3-turbo \
  --cache-dir ~/.local/share/smithy/models
```

Smithy uses [`whisper-large-v3-turbo`](https://huggingface.co/openai/whisper-large-v3-turbo)
— about eight times faster to decode than plain `large-v3`, for a barely
measurable accuracy cost.
</details>

### The terminal

A real shell, with scrollback, in a panel at the bottom. New terminals open at
your project root.

---

## Configuration

Backend selection lives in **Agent → Backend Settings…**, stored as
`~/.local/share/smithy/provider.json`. API keys are held in the OS credential
store under the service name `smithy`, never in that file.

The variables below are the fallback, used when no settings file has been saved.
The four marked ✱ are superseded by the dialog the moment you press Save; the
rest have no UI and are read every time.

| variable | default | what it does |
|---|---|---|
| `SMITHY_PROVIDER` ✱ | `openrouter` (if key set) else `lmstudio` | backend provider to use (`openrouter` or `lmstudio`) |
| `OPENROUTER_API_KEY` | *(none)* | OpenRouter key, if it isn't in the credential store |
| `OPENROUTER_MODEL` ✱ | `anthropic/claude-3.5-sonnet` | model ID to use on OpenRouter |
| `OPENROUTER_URL` ✱ | `https://openrouter.ai/api/v1` | OpenRouter API base URL |
| `LMSTUDIO_URL` ✱ | `http://localhost:1234/v1` | LM Studio endpoint |
| `LMSTUDIO_MODEL` ✱ | `qwen3.6-27b` | LM Studio model name to ask for |
| `BRAVE_API_KEY` | *(none)* | Brave Search key, if it isn't in the credential store. Absent means no `web_search` tool |
| `SMITHY_WORKER_THREADS` | core count | background threads; kept modest, since the machine is also serving a model |
| `SMITHY_LSP_LIGHT=1` | off | trades real compiler diagnostics for rust-analyzer's largest memory saving |

The dictation hotkey is stored in `~/.local/share/smithy/voice-hotkey` as the
string you'd type — `cmd+shift+v`, any order, any case. Edit it to rebind.

---

## Troubleshooting

**The agent panel shows a red dot.** LM Studio isn't reachable, or the model
isn't loaded. Start it and click Reconnect. If you unloaded the model *after*
Smithy connected, restart Smithy — it won't notice on its own yet.

**No errors or warnings in the editor.** The Problems panel says which kind of
empty it is. If rust-analyzer isn't installed it names the fix; if nothing has
analysed the project yet, it says that instead of claiming a clean bill of
health.

**Stop doesn't stop it.** Stop takes effect between steps. A shell command
waiting on your approval, or a long-running tool, finishes first.

**The microphone button does nothing.** Run with `SMITHY_VOICE_DEBUG=1` — it
reports which input device was chosen, how much audio was captured, and how long
decoding took. On macOS, check microphone permission in System Settings; the
button reports `no microphone` if it was refused.

**Terminal shortcuts are being swallowed.** `⌃L`, `⌃B`, `⌃S` and `⌃O` are
currently claimed as application shortcuts before the terminal sees them.

Other debug flags, each for a layer whose failures otherwise look identical from
outside: `SMITHY_KEY_DEBUG`, `SMITHY_SQUIGGLE_DEBUG`, `SMITHY_SKY_DEBUG`,
`SMITHY_FISHERMAN_DEBUG`.

---

## Known gaps

Honest list, short:

- **Reconnect doesn't notice a model unloaded underneath it.** Restart to clear.
- **A running tool can't be interrupted** — Stop applies between steps.
- **Only rust-analyzer is exercised.** Other language servers are configured but
  untried; if you use one, we'd like to hear how it went.
- **The agent's picture of your project is a snapshot** from when the session
  started. After restructuring a project, start a new conversation.
- Completions aren't implemented yet.

**Found something else?** Please open an issue — that's genuinely the most
useful thing you can do here. Include what you were doing and, if it's the voice
or LSP layer, the output from the relevant debug flag above.

---

## Building on it

```bash
cargo test --workspace     # 726 passing
cargo build --workspace    # 0 warnings, and it stays 0
cargo clippy --workspace --all-targets
```

Seven crates. `apps/smithy` is the binary; the rest are libraries:

| crate | what it is |
|---|---|
| `smithy-editor` | the UI: panels, menus, syntax styling, LSP client, terminal, file browser |
| `smithy-agent` | the agent loop, budgets, session persistence, backend selection, the `explore` sub-agent |
| `smithy-tools` | the agent's tools and the capability sandbox |
| `smithy-project` | project detection and context extraction |
| `smithy-sky` | astronomy for the backdrop. No dependencies at all |
| `smithy-voice` | microphone in, string out |

`smithy-agent`, `smithy-tools`, `smithy-sky` and `smithy-voice` have **no UI
dependency**, so a different front-end would be a new consumer of the same core
rather than a rewrite.

The sandbox is a capability, not a path check: the tool layer holds a `cap-std`
directory handle for your project root, so the OS itself refuses to let the
agent out — symlinks included.

## Licence

MIT.
