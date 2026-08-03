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

### 2. SCIP reader
Minimal protobuf walk over `Index.documents[].occurrences[]`.
**Done when:** parsing `kernelos.scip` yields 14,027 occurrences with 2,445
carrying `symbol_roles`, matching the Python probe exactly.
*Isolated and pure — testable against a checked-in fixture.*

### 3. Graph builder
Run `rust-analyzer scip`, parse, attribute each reference to its enclosing
function, emit `CallGraph`.
**Done when:** `restore_session` in kernelos shows its real callers, and the
`unattributed` count is reported rather than hidden.
*The first point at which the thing is real. Ship the CLI example here —
`--example callgraph <project> <symbol>` — so it is verifiable before any pixel
is drawn.*

### 4. Persistence
Write, load, hash-check, refresh.
**Done when:** the graph survives a restart, and editing one file marks exactly
that file's nodes stale.

### 5. Rendering
floem `canvas` — precedent exists in `celestial.rs` and `squiggle.rs`.
Layered layout (callers above, callee below, breadth-first from a focus node),
bounded to ~60 visible nodes with "+N more" expansion. Click to re-focus, hover
for the signature, click-through to `file:line`.
**Done when:** you can start at a symbol and walk the graph without reading any
text.
*Largest single piece. Deliberately last, so everything under it is already
proven.*

### 6. Live linking
Nodes highlight as the agent reads files, edits them, and calls `symbol`.
**Done when:** running a turn visibly lights the path the agent walked.
*This is the part that makes it verification rather than decoration.*

---

## Risks, and what would falsify the approach

**Indexing cost scales with the crate graph, not the project.** kernelos took
9.9 s at 109 crates. This workspace has 834. If it takes minutes rather than
seconds, indexing must move to an explicit, cancellable, progress-reporting
action rather than anything automatic. *Measure this at milestone 3, before
building any UI on top.*

**`rust-analyzer scip` reported two `ERROR Bug:` lines** on kernelos —
definitions it expected in a document and did not find. Two out of ~2,900, so
noise rather than a blocker, but the builder must tolerate missing documents
rather than assuming the index is complete.

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

- **Whole-program layout.** 1,840 nodes cannot be drawn usefully at once. The
  graph is always viewed from a focus node.
- **Data-flow, inheritance chains, runtime tracing.** Benzi's other features.
  The first needs analysis we do not have; the third is Python-only there and
  meaningless for a compiled binary.
- **Non-Rust languages.** The whole design leans on `rust-analyzer scip` and a
  Rust tree-sitter grammar.
- **Editing from the graph.** It is a lens, not a surface to change code through.
