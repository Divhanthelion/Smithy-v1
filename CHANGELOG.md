# Changelog

## Unreleased

- MCP servers in `.smithy/mcp.json` are wrapped as Smithy tools (`{server}_{tool}`) over HTTP or stdio. Enabled at Session start; Explore does not inherit them. Frozen tool JSON is stored with the Session so resume keeps the prefix if a server is down.
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
