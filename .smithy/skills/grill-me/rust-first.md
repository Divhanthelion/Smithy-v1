---
name: rust-first
description: Rust-only default. Other skills point here; do not invoke on its own.
---

everything stays focused exclusively on rust, we only leave rust when we absolutely have to and then only begrudgingly

Leaving Rust means introducing another language, runtime, or package ecosystem (Python, Node, C/C++, Go, a separate JS frontend stack, etc.) as something we *write or depend on*, not merely something we call through a thin FFI.

Allowed only when the pinned decision is impossible in Rust (a host platform API with no Rust binding, a mandated toolchain, a kernel/firmware boundary). Say so in the open, treat it as a defect to contain, and keep the foreign surface as small as possible.

Never recommend leaving Rust because a tutorial, a study, or a team preference is easier in another language.
