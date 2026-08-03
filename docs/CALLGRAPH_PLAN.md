# Call graph — implementation plan

An interactive, compiler-resolved map of the code, persisted to disk so it
outlives the process that produced it.

The point is **verification**: when the agent says it changed `restore_session`,
you can see what calls it and what it calls, and check. A map you cannot trust is
decoration, so every decision below is made to keep edges correct or to make
their absence obvious.

---

## What measurement established

Everything here is measured on this machine, not assumed.

| Question | Answer | How we know |
|---|---|---|
| Can rust-analyzer persist its own index? | **No, deliberately** | salsa has no serialization; upstream argues persistence would mask initial-analysis regressions. Waiting on Salsa 3.0. |
| Can we persist the *derived* graph? | **Yes — 8,000× smaller** | 5.12 GB resident vs 667 KB JSON / 79 KB gzipped for 1,840 nodes + 6,402 edges. |
| Is there a batch indexer? | **Yes** | `rust-analyzer scip` and `rust-analyzer lsif`, both present on 1.92.0. |
| What does a batch run cost? | **~10 s, 2.3 GB peak, then exits** | kernelos (109 crates): SCIP 9.9 s / 2.31 GB / 1.2 MB out; LSIF 10.7 s / 2.20 GB / 6.2 MB out. |
| Does SCIP carry `enclosing_range`? | **No — rust-analyzer omits it** | Walked 25 documents of real output: occurrences carry only `range`, `symbol`, `symbol_roles`. |
| Can we resolve calls by name instead? | **No** | 55% unambiguous on this workspace, 71% on a small one. Failures concentrate on `new`, `default`, `run` — the most-called names. |

Two consequences follow, and they drive the whole design:

1. **Edges must come from rust-analyzer.** Name matching is wrong ~45% of the
   time here, and wrong in a way that looks plausible — the exact failure this
   feature exists to remove.
2. **Enclosure must come from us.** rust-analyzer tells us *"this occurrence of
   `foo` is a reference to definition X"* but not *"it happens inside function
   Y"*. Without the second half there is no edge, only a scatter of references.

---

## Architecture: three layers, degrading honestly

Each layer is useful alone and none blocks on the one below it.

```
  structure          tree-sitter (SymbolIndex)      always available, ~470 ms
     ↓               nodes + exact spans
  resolution         rust-analyzer scip             occasional, ~10 s, 2.3 GB, exits
     ↓               reference → definition
  persistence        callgraph.json                 < 1 MB, survives restarts
```

- **No index yet?** You get the module map — exact, from `use crate::…`, no
  inference. 18 edges for kernelos.
- **Index built, analyzer stopped?** Full call graph, served from the file. This
  is the normal state.
- **Index stale?** The map says so, per file, and offers a refresh.

The pairing with `Code → Stop Language Server` is deliberate: you pay 2.3 GB for
ten seconds to build, then reclaim it and keep the map.

### Why tree-sitter owns enclosure

We already have `SymbolIndex` — 3,225 symbols across 110 files in 469 ms,
with file, line and column for every function including private methods inside
`impl` blocks. It needs one addition: `end_line`, which `Node::end_position()`
gives for free.

Then attribution is an interval lookup: a reference at `(file, line)` belongs to
the innermost function whose `[start_line, end_line]` contains it. Exact, and it
reuses code that already exists and is tested.

The alternative — deriving enclosure from LSIF's `foldingRangeResult` — is
indirect, ties us to the bulkier format, and gains nothing.

---

## Format: SCIP, with a hand-rolled reader

| | SCIP | LSIF |
|---|---|---|
| Size (kernelos) | **1.2 MB** | 6.2 MB |
| Encoding | protobuf | line-delimited JSON |
| Parse cost for us | ~80 lines | `serde_json`, already a dep |

**Choose SCIP.** We need exactly three fields — `Document.relative_path`,
`Occurrence.range`, `Occurrence.symbol`, `Occurrence.symbol_roles` — and a
minimal protobuf walker for that is under a hundred lines with no `prost`
dependency and no `build.rs` codegen. A working prototype of that walk already
exists in Python and was used to produce the table above.

Five times smaller matters because this file is read on every launch.

---

## Data model

```rust
/// A function or type in the graph.
pub struct Node {
    pub symbol: String,     // SCIP moniker: "rust-analyzer cargo kernelosv2 0.2.0 filesystem/read()."
    pub name: String,       // "read"
    pub kind: SymbolKind,   // reuses smithy-project::symbols
    pub file: String,
    pub line: usize,
    pub end_line: usize,
    pub is_public: bool,
}

/// One resolved call.
pub struct Edge {
    pub from: u32,          // index into nodes — not a string, for size
    pub to: u32,
    pub sites: u16,         // how many times; thickness of the drawn line
}

pub struct CallGraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub built_at: u64,
    /// path → content hash at index time. Drives staleness, per file.
    pub sources: HashMap<String, u64>,
    /// Edges we could not attribute to an enclosing function, and why.
    /// Never silently dropped: a graph that hides what it missed is a graph
    /// that cannot be trusted.
    pub unattributed: u32,
}
```

`from`/`to` as indices rather than strings keeps the file small: SCIP monikers
are ~60 bytes each and would otherwise dominate.

---

## Storage and staleness

`~/.local/share/smithy/projects/<key>/callgraph.json` — beside `sessions/`,
using the existing `ProjectRegistry::project_key`.

Staleness is **per file, by content hash**, not a global timestamp:

- On load, hash every indexed file. Any mismatch marks that file's nodes stale.
- Stale nodes render dimmed with a dashed border; the header says
  *"7 files changed since indexing"* with a Refresh action.
- A file the agent edits this session is marked stale immediately — we know it
  changed without hashing.

This is the honesty requirement. A cached graph is a snapshot, and a snapshot
that presents itself as current is worse than no map: it would let you verify the
agent against an architecture that no longer exists.

---

## Milestones

Each lands independently, is verifiable on its own, and leaves the tree green.

### 1. Spans in the symbol index — **done**
`Symbol::end_line`, a per-file `FnSpan` index, and
`SymbolIndex::enclosing(file, line)`. Innermost wins; `None` is a real answer.

Verifiable by hand:

```bash
cargo run -p smithy-project --example symbols -- <PROJECT> --at src/x.rs:400
```

Checked against real code: `desktop.rs:400` → `Desktop::restore_session`
(lines 392–428); `desktop.rs:31`, inside an enum, correctly returns no function.

**It found a bug worth recording.** The walker never descended into function
*bodies*, so every nested `fn`, `struct` and `const` was invisible to the entire
index — not just to enclosure. Without the fix, a call inside a nested function
would have been attributed to the function around it, producing an edge from the
wrong caller. Descending added 49 symbols here (3,225 → 3,274) for 11 ms
(469 → 480 ms). `container` is dropped on the way in: an item declared inside a
method body is not a member of the `impl` type.

### 2. SCIP reader — **done**
`smithy_project::scip` — a ~120-line protobuf walk, no `prost`, no `build.rs`.

**Acceptance met exactly.** The counts were fixed by an independent Python parse
*before* the Rust existed, and it reproduces them: 25 documents, **14,027
occurrences, 2,445 with roles, 2,445 definitions** — in 10 ms for 1.2 MB.

```bash
cargo run -p smithy-project --example scip -- /tmp/x.scip [SYMBOL]
```

Two findings that change milestone 3:

- **rust-analyzer never sets the `Import` role** — 0 of 14,027. So `use`
  statements cannot be excluded by role. They do not need to be: a `use` line is
  at file top level, outside every function, so `enclosing()` returns `None` and
  they fall out naturally. The two halves cover each other.
- **11,582 of 14,027 occurrences are plain references** (no role at all). That is
  the raw edge candidate pool, before filtering to functions and attributing
  callers.

The reader is deliberately tolerant — a truncated index yields the documents that
parsed, because `rust-analyzer scip` was observed emitting `ERROR Bug:` lines on
real input and losing a whole graph over two bad entries would be the wrong
trade. Malformed input terminates rather than looping; this file is written by
another program and a parser that can hang on it can hang the editor.

**The pipeline is already demonstrable end to end.** SCIP: `restore_session`
defined at `desktop.rs:392`, referenced at `desktop.rs:112`. Enclosure:
`desktop.rs:112` is inside `Desktop::create` (76–114). Source confirms
`desktop.restore_session(session, &on_plugin_notify)` on that line. That is one
real, compiler-resolved edge — `Desktop::create → Desktop::restore_session` —
assembled from both halves.

### 3. Graph builder — **done**
`smithy_project::callgraph`. Joins SCIP definitions (node identity, resolved) to
tree-sitter spans (caller attribution).

```bash
cargo run -p smithy-project --example callgraph -- <PROJECT> [--scip FILE] [SYMBOL]
```

kernelos: **278 nodes, 355 edges**, assembled in 0.1 s from an existing index.

| of 11,582 references | | |
|---|---:|---|
| became edges | 489 | 4% |
| external (`std`, `yew`, deps) | 7,792 | 67% |
| locals (variables, closures) | 3,281 | 28% |
| unattributed | **20** | **0.2%** |

**The `unattributed` risk did not materialise.** The plan said that if it were
large the interval approach needed rethinking; at 20 of 11,582 the tree-sitter
spans account for essentially every reference that has a caller at all.

Verified against source, not just self-consistent:
`Desktop::restore_session` is called by `Desktop::create`, and calls
`restore_plugin_window`, `take_z_index`, `WindowState::new`, `is_installed` and
`find` — five edges across four files, all confirmed by reading lines 392–428.
The other calls in that body (`borrow_mut`, `is_none`, `push`) are `std` and are
correctly counted as external rather than drawn.

**A bug caught by an implausible number.** The first run reported
`restore_session` calling `FileSystem::rename` **38 times**, and 449 self-edges.
SCIP `local N` symbols are *document-scoped* — `local 0` in two files are
different things — and the index had 4,842 such occurrences sharing only 226
distinct strings across 24 files. Keying them globally merged unrelated symbols.
Locals are now excluded entirely: they are variables and closures, not callable
functions. Self-edges fell 449 → 4, which is real recursion.

That is exactly the failure mode this design exists to prevent, caught because a
number looked wrong rather than because a test failed. The four buckets now sum
to the reference count exactly, and a test enforces it.

### 4. Persistence — **done**
`save`/`load` (write-then-rename, schema-versioned), per-file content hashing,
and `staleness(root)` returning changed / added / deleted.

This workspace's graph: **408 KB, 59 KB gzipped** for 2,221 nodes and 3,908
edges, against 5.12 GB resident — the ~13,000× ratio that made "just write it to
a file" the right instinct.

Verified live: appending one line to `scip.rs` reported
`1 changed since indexing` naming that file; reverting it reported `current`
again. Content hashing rather than mtime is what makes the second half true — a
`cargo fmt` or a save with no edit must not mark the tree stale.

FNV-1a, not `DefaultHasher`, whose output is explicitly not stable across Rust
releases. A hash written to disk that changes meaning on a toolchain upgrade
would silently invalidate every cached graph. It is also already what
`registry::project_key` uses, so there is one hash in the codebase rather than
two.

**A mistake worth recording, because it was my own documented warning.** The
first save built `sources` from the graph's *nodes*. Files containing only
`pub mod` declarations or constants produce no nodes, so seven of them —
`lib.rs`, `theme.rs`, `mod.rs`, `error.rs` — vanished from the record and
promptly reported as *newly added* on the next check. The `sources` doc comment
says in as many words that files with no functions must be recorded too. Sources
now come from the documents the **indexer** saw, never from the nodes: 107 files
→ 114.

### 5. Rendering — **done (Overview + Focus); polish open — see `HANDOFF.md`**
floem canvas in `apps/smithy/src/call_graph.rs`. Two modes:

- **Overview** (default): Benzi-style whole map — one box per source file,
  every symbol as a chip, columns fill the center pane, zoom LOD (dots when
  zoomed out). Click a chip → Focus.
- **Focus**: layered callers / focus / callees, wrap for high fan-out, fit
  camera, bus edges, jump search, hubs, Back history.

Build/load is explicit (`Agent → Build Call Graph`); never auto. Hang hazards
(paint-path `set`, staleness on paint) are documented in `HANDOFF.md` §6.

**Acceptance (human):** Overview fills the pane and shows every symbol;
Focus on a hub (e.g. kernelos `Terminal::execute_command`) is readable —
no overflow strip, no forged grid as map ground. Double-click → `file:line`
still open.

### 6. Live linking — *not started; see `HANDOFF.md` §5*
Nodes highlight as the agent reads files, edits them, and calls `symbol`.
**Done when:** running a turn visibly lights the path the agent walked.
*This is the part that makes it verification rather than decoration.*

---

## Risks, and what would falsify the approach

**Indexing cost scales with the crate graph — measured, and it is fine.**
kernelos (109 crates): 9.9 s, 2.31 GB, 1.2 MB index → 278 nodes / 355 edges.
This workspace (834 crates): 7.9 MB index → **2,240 nodes / 3,972 edges**,
assembled in 0.6 s. Eight times the crates did not produce anything like eight
times the wait. Indexing should still be an explicit action with progress, but it
does not need to be cancellable-or-nothing.

Attribution held at scale too: **137 unattributed out of 78,096 references
(0.18%)** — the same ratio as kernelos, so the tree-sitter spans are not
degrading as the tree grows.

**`rust-analyzer scip` emits `ERROR Bug:` lines on real input** — 2 on kernelos,
5 on this workspace, for definitions it expected in a document and did not find.
Out of 2,445 and 13,778 definitions respectively, so noise rather than a blocker.
The reader tolerates it: a truncated or partial index yields what parsed.

**Attribution will not be total.** References in macro expansions, `const`
initialisers and trait default bodies may fall outside any function span. The
`unattributed` count exists so this is visible; if it is large, the interval
approach needs revisiting rather than quietly under-reporting edges.

**Cross-crate edges.** SCIP monikers cover dependencies too. Calls *into* `std`
or `yew` would swamp the graph, so the builder keeps only edges whose target
resolves to a node in the workspace — with the count of dropped external edges
reported, not hidden.

---

## Explicitly out of scope

- **Force-directed whole-program soup.** 1,840 naked nodes cannot be drawn as
  one hairball. Overview is file-clustered (Benzi-style boxes); Focus is always
  from a symbol. Do not replace either with a global force layout.
- **Data-flow, inheritance chains, runtime tracing.** Benzi's other features.
  The first needs analysis we do not have; the third is Python-only there and
  meaningless for a compiled binary.
- **Non-Rust languages.** The whole design leans on `rust-analyzer scip` and a
  Rust tree-sitter grammar.
- **Editing from the graph.** It is a lens, not a surface to change code through.
