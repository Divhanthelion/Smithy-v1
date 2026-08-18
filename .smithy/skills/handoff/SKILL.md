---
name: handoff
description: Write a Project HANDOFF.md so a later Session can continue. History stays.
argument-hint: "What the next Session is for"
tools: [read, write, edit, ls, glob, grep]
---

Write a handoff the next Session can run from. This Session's History stays as it is — do not summarize in-context to free tokens; that is `/compact`.

If they passed arguments, that is what the next Session is for.

## Where

Update the Project's existing `HANDOFF.md` or `docs/HANDOFF.md` if either exists. Otherwise `write` `HANDOFF.md` at the Project root. Review-gated — do not treat it as landed until the tool result says so. Never write this to a temp directory. The Project owns memory.

## What to put in

```markdown
# Handoff

**Next session:** <one sentence>
**Rust:** stayed | left (where, why it was impossible to stay)

## Already right
Decisions that look like bugs. Do not "fix" these.

## State
What is true now. Pointers to files, branches, commits — not pasted diffs.

## Open
Unresolved decisions. Questions, not tasks, if they are still decisions.

## Next
Ordered next moves for the stated session purpose.

## Suggested skills
Slash commands the next Session should type, if any (`/ship`, `/code-review`, `/pointed-research`, `/grill-me`, …).
```

Do not duplicate specs, plans, ADRs, research notes, issues, or diffs. Reference them by path. Redact secrets.

Do not start the next Session's work here unless they ask.
