---
name: handoff
description: Write a Project-owned note for a later Session. History stays. Use only when the user types /handoff.
argument-hint: "What the next Session is for"
---

Write a handoff the next Session can run from. This Session's History stays as it is — do not summarize in-context to free tokens; that is `/compact`.

If they passed arguments, that is what the next Session is for.

Update the Project's existing `HANDOFF.md` or `docs/HANDOFF.md` if either exists. Otherwise `write` `HANDOFF.md` at the Project root. Review-gated. Never write this to a temp directory. The Project owns memory.

```markdown
# Handoff

**Next session:** <one sentence>

## Already right
Decisions that look like bugs. Do not "fix" these.

## State
What is true now. Pointers to files, branches, commits — not pasted diffs.

## Open
Unresolved decisions. Questions, not tasks, if they are still decisions.

## Next
Ordered next moves for the stated session purpose.
```

Do not duplicate specs, plans, or research notes — reference them by path. Redact secrets. Do not start the next Session's work here unless they ask.
