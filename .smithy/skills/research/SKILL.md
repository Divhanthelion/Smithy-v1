---
name: research
description: Long adversarial research — pin, snowball, disconfirm, write a cited note. Use only when the user types /research.
argument-hint: "The question to research"
tools: [read, write, ls, glob, grep, todo, web_fetch, web_search]
max-seconds: 7200
---

# Research

`web_search`, `web_fetch`, `read`, `ls`, `glob`, `grep`, `todo`, and `write` are generally suited to this task. Search, fetch, and read; then `write` one note. Sequential — one model, not a swarm.

If they passed arguments, that is the question. If they gave a topic, restate it as a question in one sentence and wait for a yes or a correction. Do not start reading until the question is pinned.

Do not write a research agent, a scraper, or a new toolchain. Fetch, read, score, write the note.

Sibling files in this skill directory (read them when the procedure names them): `snowball.md`, `sift.md`, `ach.md`.

## Pin

Write down, in the open:

1. **Pinned question** — one sentence
2. **Decision this informs** — one sentence, or `none`
3. **Hypotheses** — mutually exclusive competing **claims**, not subtopics. "Papers about A vs papers about B" is a table of contents. A literature-shaped question still needs claims about that literature (it broke vs it didn't; this is settled vs contested; the owner is A vs B). If you cannot state two such claims, the question is not pinned — restate it.

   One seed pass is allowed before the first freeze. Then **freeze**. Do not add a favorite after the evidence is in.

   **Re-freeze:** if a later pass reveals a hypothesis *class* the frozen set never covered, stop. Re-open the pin in the open, say what was missing, put the discarded set in Dropped, re-freeze, then continue. That is the only sanctioned unfreeze. Quietly expanding the set is confirmation. Quietly ignoring the new class is path drift.
4. **Stop rule** — hop generations, or "no new diagnostic evidence." A run without a stop rule is a wander.

## Loop

Work in passes. Each pass is: **plan → discover → verify → score → drift-check**. Do not write the note until scoring has survived a drift-check.

### Plan

Coarse-to-fine. One roadmap, then sub-inquiries. Every sub-inquiry must earn its keep: it discriminates hypotheses or it is cut.

When findings are contradictory or thin, **backtrack**: reformulate the query, drop the branch, or open a sibling. Record what you dropped. Silent abandonment is path drift.

### Discover

Start from a **seed set** you can defend (owners of the artifact, original papers, specs). Keyword search is how you find seeds, not how you finish.

When you have seeds, read `snowball.md` and traverse. Stop when the stop rule fires, not when the context window is bored.

A search-only run with no snowball must say why snowball was impossible (no citable artifacts, closed corpus, seeds have no graph).

### Verify before ingest

For an unfamiliar publisher, account, or extraordinary claim, read `sift.md` **before** treating the document as evidence. Vertical reading (it sounds coherent) is not verification.

A load-bearing claim needs at least two independent sources that did not copy each other. Divergence is a finding; put it under Disagreements. Do not average.

### Score

Read `ach.md`. Evaluate evidence against **all** frozen hypotheses. Rank by what is least disproved, not what is most supported. Do not sum a matrix and call it precision.

### Drift-check

Before the next pass, and before the note:

- Does this branch still serve the pinned question?
- Is this evidence diagnostic, or merely consistent with everyone?
- Are we still inside the stop rule?
- Did a hypothesis class appear that the freeze never covered? If yes, re-open the pin — do not score against a known-incomplete set.

If a branch no longer serves: cut it in the open. If a pass adds no diagnostic evidence, stop even if hop budget remains.

## Sources

Primary sources, fetched in this session, quoted or located precisely.

- Official docs, specs, RFCs, first-party API references
- Source code (this repo counts)
- Original papers, from the paper or its official HTML/PDF, not a blog about the paper
- First-party changelogs and GitHub issues/PRs on the project that owns the behavior

Not sources: SEO roundups, "X vs Y" listicles, unverified tweets, secondary "deep research" reports, YouTube, or anything you cannot fetch and quote in this session.

Citation links are discovery, not authority — snowball to the document, then read it.

When this repo's code disagrees with a paper or docs, report both and say which you observed.

Tag each finding `owner` | `spec` | `empirical` | `opinion`. If a claim cannot be tied to a fetched source, **drop it**. Fluent prose is not evidence. An uncited sentence is a defect.

## The note

One Markdown file, through `write` (Review-gated — do not treat it as landed until the tool result says so):

`docs/research/YYYY-MM-DD-<slug>.md`

```markdown
# <pinned question>

**Status:** draft
**Skill:** research
**Pinned question:** <one sentence>
**Decision this informs:** <one sentence or none>
**Hypotheses:**
- H1: ...
- H2: ...
**Stop rule:** <what fired, or what would have>

## Seeds
The starting documents and why they qualified.

## Findings
Each bullet: kind (`owner` | `spec` | `empirical` | `opinion`), claim, source (path or URL), short quote or precise location.

## Disconfirmation
For each hypothesis: diagnostic evidence against it. Which survive, which are rejected, why. Name the evidence that would flip the ranking if it were wrong (sensitivity).

## Disagreements
Where sources conflict. Do not pick a winner unless the evidence forces it.

## Dropped
Branches, queries, seeds, and discarded hypothesis sets you backtracked. One line each.

## Unknowns
What you did not search, and what would be needed to close it.

## Watch
What we would have to observe *later* for this ranking to move. Forward-looking. Not Unknowns (what we didn't search) and not sensitivity (if this already-in-hand evidence is wrong).

## Implication
2–5 sentences aimed at the pinned question (and the decision, if any). No new unsourced facts here.
```

Before marking done, walk Implication against Findings. Every Implication sentence must be entailed by a tagged finding. No orphan citations. No citations to URLs you did not fetch.

Redact secrets. Do not invent bibliography entries.

## Done

The run is done when:

1. The question was pinned and the hypotheses were frozen (a re-freeze counts only if it happened in the open)
2. At least one discover pass used seeds (search-only requires saying why snowball was impossible)
3. Unfamiliar sources were laterally verified before ingest
4. Every finding has a fetched source
5. Each hypothesis is rejected, survived, or explicitly open — none are silently ignored
6. The file exists at the path you reported and Review accepted the write
7. Unknowns, Dropped, and Watch are explicit — a note that pretends to be complete has failed

Tell the user the path and the implication. Do not start implementing unless they ask.
