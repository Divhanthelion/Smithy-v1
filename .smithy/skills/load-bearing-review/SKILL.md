---
name: load-bearing-review
description: Cold-start a codebase you have not touched. Load-Bearing Review (System by default), verdict, first moves.
argument-hint: "Optional: decision this review serves"
include: lenses.md, template.md
tools: [read, write, ls, glob, grep, bash, explore, symbol]
---
# Load-Bearing Review

You are in the **current Project**, not the Software_review authoring repo. Do not ask for a tour. Read lenses.md before S4. Write the review with template.md.

Default decision if the user did not name one: **Can I change this safely, and what is the first move?**

## Senior rule

Favor Proceed once the Subject definitely improves system health, even if it is not perfect. Do not Proceed if it degrades health, except in a named emergency.

A comment that does not touch complection, coupling, failure, time, or identity is a Nit.

## Cold start (do this first)

Infer **Mode = System** unless the user pointed at a diff or RFC. Infer **Stage** from evidence: Completed if it ships or has users; WIP if it is a spike, unfinished tree, or never deployed.

Run, in this order. Cite paths. Do not cite vibes.

1. **Claim** — README, CONTEXT.md, AGENTS.md, package/Cargo/pyproject/go.mod. What it says it does.
2. **Pulse** — `git log -20 --oneline`, `git status`, default branch, last tag, CI config. How long it has been dead, and what the last real change was.
3. **Map** — entrypoints, deploy, data stores, who uses it, where trust changes. C4-lite. If you cannot draw this, that is a Shape Finding. Stop guessing.
4. **Invariants** — tests that would fail if money, tenancy, identity, or idempotency broke. Coverage % is not this.
5. **Failure** — timeouts, retries, dual writes, what happens if a process dies mid-write.
6. **Trust** — authz, control plane, secrets, agent/tool boundaries.
7. **Change** — how you ship, see, recover; who else can change this.

Then S1–S6. Time-box: do not boil the ocean. Cap Adversary paths at **7** for System, **3** for Diff/Design.

## Protocol

1. **S1 Frame** — Subject; decision; harm if wrong; out of scope; one-way doors. If there is no decision, stop and ask.
2. **S2 Utility scenarios** — 3–7 stimulus/response pairs. Rank (high importance, high difficulty). Those own the review.
3. **S3 Understand** — the map from cold start. If a strong reviewer cannot explain the request path, write a Question or Shape Finding.
4. **S4 Lenses** — Purpose, Shape, Correctness, Failure, Adversary, Change against the scenarios only. Skip a Lens and say so. Questions in lenses.md.
5. **S5 Adversary** — K1–K15 in lenses.md. Goal → preconditions → boundary → effect → blast radius → detection gap → fix class. No exploits, payloads, or reproduction procedures. Five-minute premortem: it failed in six months; what did go wrong. This is a threat model, not a pentest.
6. **S6 Verdict** — classify, severity, Confidence, one verdict.

## Classification

| Kind | Rule | Changes verdict? |
| --- | --- | --- |
| Finding | Claim + evidence + blast radius + fix class + Confidence | Yes |
| Risk | Plausible path, incomplete evidence | Yes, if dated or Stop-class harm |
| Question | Unknown that would flip the verdict | No until answered |
| Nit | Preference | Never |

Severity: **Stop** (harm / data loss / authz / irreversible) · **Path-block** (owner + date, or change path) · **Dated debt** (expiry required) · **Nit**.

Confidence: observed | demonstrated | inferred | speculative. Speculative cannot be a Stop Finding.

Verdict: **Proceed** · **Proceed-with-constraints** · **Redirect** · **Stop**. Redirect = wrong path. Stop = unsafe increment.

Match review weight to one-way doors (schema, published interface, identity, data in users' hands). Slow review of reversible work is a quality failure.

## Output

1. Put the review in the answer using template.md.
2. `write` the same text to `LBR.md` at the Project root (overwrite if a prior run exists). Review-gated.
3. After the verdict, give **First moves** — at most three actions, smallest reversible first.

Do not start a rewrite. If Verdict is Proceed or Proceed-with-constraints, the first move is the smallest change that teaches. If Stop, do not code. If Redirect, name the different bet and stop.

## Do not

- Ask the user to explain the repo if the files can answer
- Style nits crowding out design
- ISO 25010 or DORA as a score
- Coverage percent as Correctness
- Undated "we'll fix it later"
- Inventing exploits
- Reviewing every Lens equally
- Treating a scheduled pentest as this review
- Following rust-first or any stack religion. This skill is stack-agnostic.
