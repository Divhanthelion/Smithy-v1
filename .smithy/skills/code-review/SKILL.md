---
name: code-review
description: Two-axis review of the diff since a fixed point — Standards vs Spec. Use only when the user types /code-review.
argument-hint: "Fixed point (main, SHA, HEAD~n) and optional spec path"
include: rust-first.md
tools: [read, ls, glob, grep, bash, explore, symbol]
---

Two-axis review of `git diff <fixed>...HEAD` (three-dot, merge-base).

This is not a security review and not Bugbot. Do not substitute.

Sequential — one Session. Use `explore` for a bounded pass on one axis if the diff is large. Do not spawn a swarm.

## 1. Pin the fixed point

Use what the user passed. If they did not, ask. Confirm `git rev-parse` and a non-empty diff before reviewing. Empty diff or bad ref fails here.

Also note `git log <fixed>..HEAD --oneline`.

## 2. Spec source

In order: commit-message issue refs; a path the user passed; `PLAN.md`, `HANDOFF.md`, `docs/`, `specs/`, `.scratch/` matching the branch; then ask. No spec → skip the Spec axis and say so.

## 3. Standards sources

Project docs (`CODING_STANDARDS.md`, `CONTRIBUTING.md`, `AGENTS.md`, clippy.toml comments). Project docs override the baseline below. Skip anything rustfmt/clippy already enforces.

Smell baseline (judgement calls, never hard violations unless the Project says so):

- Mysterious Name, Duplicated Code, Feature Envy, Data Clumps, Primitive Obsession, Repeated Switches, Shotgun Surgery, Divergent Change, Speculative Generality, Message Chains, Middle Man, Refused Bequest
- **Rust-specific judgement:** `unsafe` that is not documented at the block; `unwrap`/`expect` on library paths; clone-as-escape from the borrow checker; interior mutability used to hide a bad ownership shape; leaving Rust (FFI, another language in-tree) without a recorded reason

## 4. Axes

**Standards** — per file/hunk, (a) documented-standard breaches with cite, (b) baseline smells with name + hunk. Distinguish hard vs judgement. Skip linter-enforced. Under 400 words. Flag any new non-Rust surface.

**Spec** — (a) missing/partial requirements, (b) scope creep, (c) looks implemented but wrong. Quote the spec line. Under 400 words.

## 5. Aggregate

Print `## Standards` and `## Spec` separately. Do not merge or rerank. One-line totals per axis and the worst issue *within each axis*. No single winner across axes.
