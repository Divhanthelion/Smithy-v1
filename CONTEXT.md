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
One conversation the user is in: resume id, History, frozen tools, Map. New session starts another. Compact stays in this one.
_Avoid_: using Session to mean "a new prefix"; chat, thread as type names

**History**:
The messages this Session will send on the next completion. Ordinary turns append. After a write lands, some Providers replace superseded `read`/`edit`/`write` payloads for that path with a stub (disk is the source of truth). Compact replaces the rest with a summary. Resume replays them verbatim.
_Avoid_: the Project; the panel's view-only transcript; Handoff notes

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

**YOLO**:
A mode. Inside the Project, `edit` / `write` skip Review and `bash` that stays down in the tree runs without a prompt. A command that names a path up out of the Project, or over into a sibling, still asks. Off by default. Not a sandbox: the shell is still a subprocess.
_Avoid_: auto-land as the name; using YOLO as a synonym for Review; claiming bash cannot leave

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

**Harness**:
The files that assemble a Session: `SYSTEM.md` plus any `include` listed in `harness.toml`. Project `.smithy/harness/`, then `~/.smithy/harness/`, then what Smithy ships. A file in that directory that is not listed is not sent. Editing mid-Session does not hot-reload.
_Avoid_: Skill as the harness; always-on rules that are not in the include list; stuffing Cursor rules into every Turn

**Command**:
Composer `/name` plus optional args. A Skill Command injects `SKILL.md` into this turn. Compact and Handoff are harness Commands, not Skills.
_Avoid_: every slash being a Skill; slash as a mid-chat tool swap

**Skill**:
A `SKILL.md` (Project `.smithy/skills/<name>/`, then `~/.smithy/skills/<name>/`, then the procedures Smithy ships) loaded only when the user types that Command. A context-injection macro: the body is prefixed onto the current user message (the panel shows what they typed). Optional frontmatter `tools` (allowlist), `include` (sibling files), `max-seconds`. `tools` and `max-seconds` wait for New session, not Compact. Editing the file mid-Session does not hot-reload.
_Avoid_: auto-invoke from ambient text, marketplace, stuffing `@` files into the system prompt, a Session kind per skill, compressing history to apply a skill

**Compact**:
A Command that replaces this Session's History with a lossy summary so the same conversation can continue in a smaller window. Same Session. Not Handoff, not New session.
_Avoid_: writing a Project file; starting a new conversation; using Handoff to free tokens

**Handoff**:
A Command that writes a Project-owned note for a later Session. This Session's History is unchanged. Not Compact.
_Avoid_: summarizing in-context to free tokens; naming this Compact

**Research**:
The `/research` Skill. Search, fetch, read, write a note through Review. Procedure in the user turn, not a Session rebuild. Not Explore, not a Session kind.
_Avoid_: calling this Explore; loosening Explore's 12/180s/48k caps to make Research work

**Grill**:
The `/grill-me` Skill. Interview; facts via read/explore. Not Research, not a Session kind.
_Avoid_: implementing during the grill; treating Grill as a harness type

### Provenance (do not leak into names)

**coda, forge, divcli, rustcoder**:
Ancestor projects. Fine in comments that explain *why*. Not crate names, type names, or user-facing copy.
_Avoid_: `ForgeSession`, `CodaTool`, `divcli`-prefixed APIs
