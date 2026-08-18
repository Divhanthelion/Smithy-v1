---
name: pointed-research
description: Investigate a decision against primary sources and write a cited note. Use only when the user types /pointed-research.
argument-hint: "The decision this research must inform"
include: rust-first.md, authority.md
tools: [read, write, ls, glob, grep, web_fetch, web_search, explore, todo]
---

# Pointed research

Stay in the Rust docs/RFC/source world unless the pinned decision is about a foreign boundary.

Research is for a **decision**, not a topic. A topic produces a survey. A decision produces a note someone can act on.

If the user passed arguments, that is the decision. If they gave a topic, restate it as a decision in one sentence and wait for a yes or a correction. Do not start reading until the decision is pinned.

This is not `/research`. `/research` wanders on purpose, then disconfirms. Pointed pins a decision and treats wandering as failure. Do not run both in one Session.

Use `explore` for bounded reads. Sequential — one model, not a swarm.

## What “pointed” means

Before searching, write down the **few facts that would change the answer**. Those are the only questions the reading is allowed to pursue. Everything else is out of scope, including interesting tangents.

## Sources

Primary sources only:

- Official docs, specs, RFCs, first-party API references
- Source code (this Project counts)
- Original papers, from the paper or its official HTML/PDF, not a blog about the paper
- First-party changelogs and GitHub issues/PRs on the project that owns the behavior

Not sources: SEO roundups, “X vs Y” listicles, unverified tweets, secondary “deep research” reports, YouTube, or anything you cannot fetch and quote in this Session.

If a claim cannot be tied to a fetched primary source, **drop the claim**. Do not leave it in with a weasel citation. An uncited sentence is a defect.

When this Project’s code disagrees with official docs, report both and say which you observed.

Follow authority.md before ranking sources. Short version: **who owns the artifact** outranks university letterhead. Tag each finding `owner` | `spec` | `empirical` | `opinion`. Do not let opinion or a prestigious study override an owner observation. If an empirical paper’s corpus is not Rust, say so and do not imply it transfers.

## The note

One Markdown file, through `write` (Review-gated — do not treat it as landed until the tool result says so). Match the Project’s existing research/notes convention if there is one. Otherwise:

`docs/research/YYYY-MM-DD-<slug>.md`

```markdown
# <decision question>

**Status:** draft
**Pinned decision:** <one sentence>
**Would change the answer:**
- <fact 1>
- <fact 2>

## Findings
Each bullet: kind (`owner` | `spec` | `empirical` | `opinion`), claim, source (path or URL), short quote or precise location.

## Disagreements
Where sources conflict. Do not pick a winner unless the evidence forces it.

## Unknowns
What you did not search, and what would be needed to close it.

## Implication
2–5 sentences aimed at the pinned decision. No new unsourced facts here.
```

Do not paste the note into `HANDOFF.md`. Point at the file.

Redact secrets. Do not invent bibliography entries.

## Done

1. The decision was pinned
2. Every finding has a fetched source
3. The file exists at the path you reported and Review accepted the write
4. Unknowns are explicit

Tell the user the path and the implication. Do not start implementing unless they ask.
