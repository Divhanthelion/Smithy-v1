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

To put `smithy` on your PATH — a release build, into `~/.cargo/bin`:

```bash
cargo install --path apps/smithy --force
```

`--force` is what makes it a reinstall; without it cargo declines to overwrite a
binary of the same version, and since the version rarely changes, an upgrade
would silently do nothing.

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

- **LM Studio** — everything downloaded locally, resident models first, with size
  and context. **Load** makes one resident without leaving the editor. That's
  optional — LM Studio's JIT loader pulls in an unloaded model on the first
  request — but it turns a minute of apparent hang into a progress line.
- **OpenRouter** — the full catalogue, free tier first. **Free only** is on by
  default; switch it off for paid models. Each row shows its context window and
  price per million tokens. The catalogue is public, so the list populates before
  you have a key (you still need one to *call* anything, including free models).
- **DeepSeek** — `deepseek-v4-flash` and `deepseek-v4-pro`, both 1M context and
  tool-capable. Needs a key from [platform.deepseek.com](https://platform.deepseek.com)
  before it will list anything, since its `/models` endpoint is authenticated.
  Context windows and prices shown for DeepSeek are a **compiled-in snapshot** —
  its API reports neither, and it has announced peak-hour rates at double list
  price. Use them to compare models, not to estimate a bill. OpenRouter's prices,
  by contrast, are live from its API.

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

Two ways in:

- **`+` in the Explorer** — every row has one. Click it and the file goes to the
  agent's next message; the panel opens if it was hidden. This is the one to
  reach for, because the file you want is usually already on screen.
- **Drag and drop** onto the agent panel — for files from outside the project,
  where the Explorer cannot see them.

Either way they appear as chips above the composer with their size and token
cost; click one to include or exclude it, and the row totals what the next
message will spend. Attachments go out with that one
message and are then cleared — they're already in the conversation's history, so
re-sending them would just cost twice.

A dropped folder is walked gitignore-aware and skips dotfiles, so dropping a
project doesn't paste `target/` or `.env` into a prompt. Binaries and anything
over 256 kB are named rather than inlined, so the agent knows they exist and can
`read` a slice.

### Reviewing what the agent writes

Every `edit` and `write` is held for review: the diff modal opens and **the
agent's tool call waits for your decision**, then hears the real outcome —
"accepted in full", "3 of 5 hunks", "rejected" — as that call's own result.

That waiting is the point. It used to queue the change, tell the model "waiting
for the user to approve", and deliver the outcome only at the start of the *next*
turn. Inside one long turn the model therefore never learned whether anything had
landed. A measured session made 25 edits in a single turn and spent **26 of its
76 tool calls** re-editing files and polling them with `grep` and escalating
`sleep`s, trying to find out. The edits had all been approved and written.

The trade: a turn blocked on a review counts against its wall-clock budget, so
walking away mid-review will eventually end the turn. The header says which mode
you are in — **`✓ edits reviewed`** or **`⚠ edits land directly`**. Click it to
switch. Auto-approve is worth it for a long implementation run against a plan you
have already read; it skips the modal entirely.

**New session** in the panel header (or **Agent → New Session**) is the one that
forgets. The `↺` icon beside it only clears the transcript you're looking at —
the model still remembered everything. New Session throws away the history, the
pending review bookkeeping, and rebuilds with a freshly extracted project
context. The previous conversation stays on disk rather than being deleted.

### Knowing the code

Two layers, deliberately separate.

**The map** goes in the system prompt: crate layout, dependencies with version
requirements, every module path, and the public API. It is sized against the
model's window (~5% of it) and is what stops the agent guessing at file paths.
Inspect it with:

```bash
cargo run -p smithy-project --example dump .
```

**The index** is queried, not read. Every symbol in the project — structs, enums,
**enum variants**, traits, functions, **methods inside `impl` blocks**, consts,
type aliases — with file, line and exact signature, public and private alike.
Built once per session (~460ms for 3,168 symbols across 109 files) and exposed as
the `symbol` tool, so a lookup is one hash rather than a search of the tree.

The split matters because the map is prefilled on *every* request while the index
is paid for only when asked. That is also why enum variants live in the index and
not the map: one 20-variant enum is ~150 tokens of preamble on every turn, for a
fact that is one call away.

This exists because of a specific failure. The map said `DesktopMsg` existed but
not what was in it, so the agent wrote `DesktopMsg::PluginsChanged` — no such
variant. It called `restore_session` with two arguments; the method took one, and
being neither `pub` nor top-level it was in no map at all. Four of seven build
errors were that one shape: **a name it could see existed, whose shape it could
not.** `symbol DesktopMsg` answers that in a single call.

```bash
cargo run -p smithy-project --example symbols -- . DesktopMsg
```

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
| `SMITHY_PROVIDER` ✱ | first key found, else `lmstudio` | `lmstudio`, `openrouter`, or `deepseek` |
| `OPENROUTER_API_KEY` | *(none)* | OpenRouter key, if it isn't in the credential store |
| `OPENROUTER_MODEL` ✱ | `anthropic/claude-3.5-sonnet` | model ID to use on OpenRouter |
| `OPENROUTER_URL` ✱ | `https://openrouter.ai/api/v1` | OpenRouter API base URL |
| `DEEPSEEK_API_KEY` | *(none)* | DeepSeek key, if it isn't in the credential store |
| `DEEPSEEK_MODEL` ✱ | `deepseek-v4-flash` | model ID to use on DeepSeek |
| `DEEPSEEK_URL` ✱ | `https://api.deepseek.com` | DeepSeek API base URL |
| `LMSTUDIO_URL` ✱ | `http://localhost:1234/v1` | LM Studio endpoint |
| `LMSTUDIO_MODEL` ✱ | `qwen3.6-27b` | LM Studio model name to ask for |
| `BRAVE_API_KEY` | *(none)* | Brave Search key, if it isn't in the credential store. Absent means no `web_search` tool |
| `SMITHY_WORKER_THREADS` | core count | background threads; kept modest, since the machine is also serving a model |
| `SMITHY_LSP_LIGHT=1` | off | trades real compiler diagnostics for rust-analyzer's largest memory saving |

The dictation hotkey is stored in `~/.local/share/smithy/voice-hotkey` as the
string you'd type — `cmd+shift+v`, any order, any case. Edit it to rebind.

## Reading back a session

Every conversation is written to
`~/.local/share/smithy/projects/<project>/sessions/*.json` — the whole thing,
every tool call and result. The model's **reasoning** is stored beside the
messages rather than inside them, so the transcript still replays byte-for-byte
into a warm prefix cache while the thinking survives the session.

```bash
cargo run -p smithy-agent --example transcript -- list
```

Then `show <FILE> --reasoning` to read one in the terminal, or
`md <FILE> > session.md` to export it with reasoning in collapsible blocks.

Reasoning only exists for sessions recorded after this was added; older files
list `0` and replay without it.

## The meters

Top-right of the menu bar, beside the clock.

**Spend** — what this session has cost, from the endpoint's own token accounting
times the model's list price, plus the balance left on the account. DeepSeek is
the only backend here with a balance endpoint, polled every three minutes; the
session figure updates every five seconds. A local model or an unpriced one shows
tokens instead of a number that might be wrong.

Session cost is the figure that teaches you something: a conversation re-sends
its whole prefix on every request, so the same question costs more at turn forty
than at turn four. Balance is the one that matters when you have put ten dollars
on an account.

**Memory** — Smithy's own resident set, and every `rust-analyzer` on the machine
summed. Turns amber past 4 GB.

### Why rust-analyzer is so large

It indexes your **dependencies**, not just your code, so its footprint tracks the
size of the crate graph rather than the size of the project. Measured:

| Project | Crates in graph | rust-analyzer RSS |
|---|---|---|
| a small Yew app | 109 (1 yours) | 724 MB |
| this workspace | 834 (7 yours) | 5.1 GB |

7.6× the crates, ~7× the memory. That is normal, not a leak — 1–3 GB is typical
and 5 GB is the high end for a graph this size. Smithy already re-roots and stops
the old servers on a project switch, so they do not accumulate.

Two levers:

- **Code → Stop Language Server** reclaims it immediately, and **Start Language
  Server** brings it back. Distinct from the shutdown at app exit, which also
  ends the worker and cannot be recovered from.
- `SMITHY_LSP_LIGHT=1` disables `checkOnSave`, which stops a *second* cargo
  process holding a full build in memory. The largest single saving; the cost is
  real compiler diagnostics, leaving rust-analyzer's own inference.

Lower levers — disabling proc macros or build scripts — break serde derives and
most of the build, and are not worth it.

## Budgets

A turn stops on whichever ceiling it reaches first: tool calls, wall clock, or
context. The step ceiling **scales with the model's context window** — 60 at 32k,
120 at 128k, 180 at 1M — because a flat 60 killed a turn that had used 6% of its
context budget. At four-fifths of the way through, the agent is told how many
calls remain and asked to finish and report what is outstanding, rather than
being cut off mid-edit with no warning.

Project context scales the same way: ~5% of the window rather than a flat 6k
tokens, floored at the old value and capped at 40k.

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
cargo test --workspace     # 947 passing
cargo build --workspace    # 0 warnings, and it stays 0
cargo clippy --workspace --all-targets
```

Eight crates. `apps/smithy` is the binary; the rest are libraries:

| crate | what it is |
|---|---|
| `smithy-editor` | the UI: panels, menus, syntax styling, LSP client, terminal, file browser |
| `smithy-agent` | the agent loop, budgets, session persistence, backend selection, the `explore` sub-agent |
| `smithy-tools` | the agent's tools and the capability sandbox |
| `smithy-project` | project detection and context extraction |
| `smithy-fisherman` | the figure on the bottom rail: his day, his poses, and the drawing |
| `smithy-sky` | astronomy for the backdrop. No dependencies at all |
| `smithy-voice` | microphone in, string out |

`smithy-agent`, `smithy-tools`, `smithy-fisherman`, `smithy-sky` and
`smithy-voice` have **no UI dependency**, so a different front-end would be a
new consumer of the same core rather than a rewrite.

The sandbox is a capability, not a path check: the tool layer holds a `cap-std`
directory handle for your project root, so the OS itself refuses to let the
agent out — symlinks included.

## Licence

MIT.
