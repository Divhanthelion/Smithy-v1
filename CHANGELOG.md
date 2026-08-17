# Changelog

## Unreleased

- YOLO: the header toggle (`⚠ YOLO` / `✓ reviewed`) skips Review for in-Project writes and skips the shell prompt for `bash` that stays down in the Project. `cd ..`, `../sibling`, `~/...`, and `/etc/...` still ask. Off by default.
- Opening a large Project no longer freezes the window before it is shown: the file watcher walks and subscribes on its own thread, one recursive watch instead of a syscall per folder. A stalled model stream fails the Turn after 90s of silence instead of sitting on `working...` until the 15-minute request timeout. DeepSeek V4 tool calls round-trip `reasoning_content` so the next request is not a 400.
- Answers render a markdown subset: `**bold**`, `*italic*`, `` `code` ``, headings, lists, and fenced code with a copy button. Streaming answers stay in the transcript scroll; the scrollbar follows the live edge until you wheel, then lets go. Three or more consecutive tool steps collapse into one expandable batch. A screenshot drop that never sends Leave no longer leaves the drop outline stuck (timeout + click to dismiss).
- `/research` and `/grill-me` ship with the app. They used to live only in this repository's `.smithy/skills/`, so any other Project (and empty stub dirs under `~/.smithy/skills/`) answered `No skill named`. The picker lists them everywhere; `~/.smithy/skills/` is filled in if `SKILL.md` is missing, without overwriting a copy you already edited.
- `/` picker lists Skills and harness Commands; Tab completes a prefix (`/c` → `/compact `), arrows move the highlight, and the list scrolls so `/research` and `/grill-me` are not clipped under `/compact`.
- Opening the app asks for the login keychain password at most once. `get` reads one Keychain item and does not write or delete on that path (the previous vault migration prompted four times: read vault, read leftover, write vault, delete leftover).
- Skills are context-injection macros: any `SKILL.md` under `.smithy/skills/<name>/` is a Command. `/name` prefixes the skill body onto the current user turn; it does not rebuild the Session or compress conversation history. `/compact` replaces this Session's History with a lossy summary (same conversation; a `.full.json` log is kept). `/handoff` writes a Project-owned note and leaves History unchanged. The left rail switches between Files and History (stored Sessions; click to resume, ☰ opens the JSON log). Optional `tools`, `include`, and `max-seconds` frontmatter; `tools` / `max-seconds` wait for New session. No Session kind per skill. MCP servers in `.smithy/mcp.json` are wrapped as Smithy tools (`{server}_{tool}`) over HTTP or stdio. Enabled at Session start; Explore does not inherit them. Frozen tool JSON is stored with the Session so resume keeps the prefix if a server is down.
- File → Open Project no longer panics on macOS. The menu used a blocking native picker inside a winit event; it now uses the same async sheet as ⌘O.
- CI on macOS runs workspace tests, fisherman harness goldens, clippy `-D warnings`, and a full build.
- Forged aesthetic: the editor pane is transparent so the sky backdrop can show through; observer location is `SMITHY_SKY_LAT` / `SMITHY_SKY_LON` (default San Francisco).
- `bash` is deny-by-default unless a `shell-approval` or `allow-bash` hook is installed. Secret-looking environment variables are scrubbed from the child.
- `web_fetch` follows redirects by hand and re-validates every hop, including resolved addresses.
- The turn clock now binds mid-request; the wrap-up warning counts tool calls, including parallel ones.
- Review apply/reject/partial paths are tested against tempfiles; abandoning a review on project switch writes neither root.
- Session kind round-trips through persist and resume.
- XML fallback parser no longer treats a prose mention of `<tool_call>` as a failed call, and no longer splits JSON on a closer that sits inside a string.
- Dead LSP completion path and unused `async-lsp` dependency removed. Only rust-analyzer is spawned.
- Docs: sandbox vs shell boundary, web_fetch SSRF, Whisper f16 / English-only / 30s chunks, terminal as a shell-output panel, XDG session store on macOS.
