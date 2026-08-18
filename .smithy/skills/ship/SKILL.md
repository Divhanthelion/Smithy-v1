---
name: ship
description: Land the current Rust work on GitHub. Creates a private repo if none exists, runs tests, commits if needed, then git push. Use only when the user types /ship.
argument-hint: "Optional commit message or repo name"
include: rust-first.md
tools: [read, ls, glob, grep, bash]
---

Land this Project on GitHub.

`/ship` is the user saying push. Do not wait for a second "please commit/push." Do not force-push. Do not run `git config`. Do not `git push --force`. `bash` waits for approval — that is expected.

If arguments were passed, treat them as the commit message, or as the GitHub repo name when creating a remote.

## 0. Rust

If `Cargo.toml` exists, run `cargo test` (Cargo workspace: `cargo test --workspace`). Tests failing → stop. Do not commit or push.

If there is no `Cargo.toml`, say so and continue only with git/GitHub setup. Do not introduce another language to "just get it on GitHub."

## 1. Git repo

If `.git` is missing: `git init`.

`git status` and `git remote -v`.

## 2. Commit if dirty

If the work tree is dirty:

- Stage what belongs to this ship. Do **not** stage `.env`, credentials, `target/`, or secret files.
- Commit with a HEREDOC message (1–2 sentences, why not what). If the user passed a message, use it.
- Never add a co-author to a commit message.

If there are no commits at all after that, stop — GitHub has nothing to receive.

## 3. Remote — none yet (new Project)

No `origin` (or no remotes):

1. Check `gh` is on PATH and authenticated (`gh auth status`). If not: tell them to `gh auth login` and stop.
2. Default **private**. Default name: user argument, else the directory name.
3. Ask once only if the name would be surprising or they might want public. Otherwise create:
   `gh repo create <name> --private --source=. --remote=origin`
   Do **not** pass `--push`. The next step must be a real `git push`.
4. If `gh` refuses (name taken, no network): report the error, do not invent a remote URL.

## 4. Push

`git push -u origin HEAD` (or the existing upstream if set).

Never `--force` / `--force-with-lease` from this Skill.

## 5. Done

Report: test result, commit SHA if you created one, remote URL, whether the repo was created in this run. Stop. Do not open a PR unless they ask.
