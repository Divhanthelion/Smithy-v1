---
name: authority
description: Source ranking for this user's research skills. Other skills point here; do not invoke on its own.
disable-model-invocation: true
---

# Who to listen to

This skill cannot tell fact from fiction. It can classify a source, prefer the owner of the artifact, and refuse to launder prestige into certainty. You still decide.

Calling skills may add a **source fence** (e.g. pointed-research stays in Rust docs). This file does not. It only ranks what that fence let through.

## Owner beats letterhead

Harvard vs an unknown university is the wrong question. Institutional rank is a **weak prior**, never a verdict. Excellent CS comes from everywhere; prestigious labs publish irreproducible work. Do not treat affiliation as accuracy.

Listen first to whoever **owns the artifact** the claim is about:

| Claim about | Owner (highest weight) |
| --- | --- |
| What `rustc` does | rustc source, The Reference, RFCs, rust-lang team writing on that RFC |
| Unsafe / aliasing | The Nomicon, Miri, compiler internals — not a blog "explaining" unsafe |
| `std` / a crate | That crate's source and docs on docs.rs |
| A bug in this repo | This repo |
| A protocol, spec, or paper's claims | That spec or paper, not a survey citing it |
| A product's behavior | First-party docs and the running system |
| An empirical "study of bugs" | The paper's **methods and corpus**, not the university name |

A Brazilian group that measured what they claim, released the corpus, and studied software you actually write outranks a Harvard affiliation on a Java bug-mining paper used to advise Rust.

## Rank (high to low)

1. **Artifact owner** — the implementation, spec, or RFC that *is* the thing
2. **Independent corroboration** — two primary sources that did not copy each other
3. **Methods** (empirical only) — what was measured, on what corpus, with what definition of the thing counted; whether the artifact (code, data) is available
4. **Incentives** — who funded it, what they need to be true; report, do not psychoanalyze
5. **Institutional prestige** — weak prior only; never decisive; never a substitute for (1)–(3)

Popularity, SEO, and "everyone knows" are anti-signals.

## Empirical studies

If the corpus is not the artifact under study, say so and do not imply it transfers. A study of Java, JavaScript, or "GitHub at large" does not settle a Rust question. A study is weaker if: no artifact, no definition of the thing counted, p-hacking tells, or the conclusion is bigger than the measurement.

## What to write in the note

For each finding, tag **kind**: `owner` | `spec` | `empirical` | `opinion`.

If kinds disagree, put it under Disagreements. Do not average them. Do not let an `opinion` or a prestigious `empirical` paper override an `owner` observation of the artifact.
