# Smithy

A native Rust editor whose agent runs on the user's machine. Distinctive pieces exist so the user can *check* the agent rather than trust it.

## Language

### The thing being edited

**Project**:
The directory the user opened. Grounding for the agent, the sandbox, the file browser, and the session.
_Avoid_: workspace (except the sandbox type), folder, repo (unless you mean git)

**Cargo workspace**:
A Cargo `[workspace]` with members. Say this in full when you mean the Rust package graph, never "workspace" alone.

**Workspace**:
The `cap-std` directory capability rooted at the Project. The OS refuses escapes, including via symlinks. It is a handle, not a synonym for Project.

### The agent

**Session**:
One conversation with a byte-stable prefix. History is append-only. Regenerating the system prompt means a new Session.
_Avoid_: chat, thread, conversation (UI copy may say conversation; the type is Session)

**Turn**:
One user message through the tool loop to an Outcome (answer or stop).
_Avoid_: request, call, run

**Step**:
One tool call inside a Turn.

**Map**:
The `ProjectContext` block in the system prompt: crate layout, dependencies, module paths, public API. Built once per Session, sized to a slice of the window. Querying it is free; changing it is a new Session.
_Avoid_: context (too generic), dump, preamble

**Index**:
The symbol table (structs, variants, methods, …) queried through the `symbol` tool. Paid only when asked. Variants live here, not in the Map.
_Avoid_: putting Index facts into the Map "to be helpful"

**Review**:
Hunk-level approval of `edit` / `write`. The tool call *waits* and hears the real outcome. Not a queue that reports later.
_Avoid_: confirm, pending-as-success, auto-land (that is a mode, not Review)

**Explore**:
The read-only sub-agent tool. Answers one bounded question; intermediate reads die in its own history. Cannot write, bash, or call itself.
_Avoid_: swarm, orchestrator, subagent as a type name

**Attachment**:
Files on the *next* user message only, then cleared. Already in history after that.
_Avoid_: pin, always-on context, project files

**Provider**:
Where completions come from: LM Studio, OpenRouter, or DeepSeek.
_Avoid_: backend as the type name (the menu may still say Backend Settings)

**MCP**:
A server listed in `.smithy/mcp.json` (Project, then user; union by name). Enabled servers' tools are wrapped as Smithy Tools in the frozen tool block. Smithy still dispatches. GitHub is one server, not the feature.
_Avoid_: Session kind, Skill, Command, `/mcp`, treating MCP as the agent, a GitHub-shaped client

### The interface

**Aesthetic**:
The visual treatment: Flat or Forged. A different structure, not a colour swap.
_Avoid_: Look, Theme, Skin as type names. "Switch Look" is menu copy only.

**Forged**:
The ornamented Aesthetic: carved frame, circuitry, the Fisherman on the rail.
_Avoid_: ornate, decorated, smith-mode

**Fisherman**:
The figure on the Forged bottom rail. Ornament. Not the agent.
_Avoid_: smith, mascot-as-agent, putting Fisherman types in `smithy-agent`

**Ink**:
The drawing trait the Fisherman (and harness) paint through. UI-crate adapters implement it; the agent never sees it.

### Commands and skills

**Command**:
Composer `/name` plus optional args. May rebuild the Session (new system prompt and tool block) then send the remainder as the first user message.
_Avoid_: slash as a mid-chat tool swap; the tool JSON is frozen for the Session

**Skill**:
A `SKILL.md` (Project `.smithy/skills/<name>/`, then `~/.smithy/skills/<name>/`) loaded only when the user types that Command. Editing the file mid-Session does not hot-reload.
_Avoid_: auto-invoke from ambient text, marketplace, stuffing `@` files into the system prompt

**Research**:
Session kind / tool profile for `/research`. Search, fetch, read, write a note through Review. Not Explore.
_Avoid_: calling this Explore; loosening Explore's 12/180s/48k caps to make Research work

**Grill**:
Session kind for `/grill-me`. Facts via read/explore; no `write` / `edit` / `bash` until they leave Grill. Not Research.
_Avoid_: implementing during the grill; treating Grill as a coding Session with extra prompt text

### Provenance (do not leak into names)

**coda, forge, divcli, rustcoder**:
Ancestor projects. Fine in comments that explain *why*. Not crate names, type names, or user-facing copy.
_Avoid_: `ForgeSession`, `CodaTool`, `divcli`-prefixed APIs
