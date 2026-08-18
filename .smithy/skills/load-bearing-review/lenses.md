# Lenses

Apply only against the utility scenarios from S2. Each Lens has one load-bearing question. If you cannot answer it, write a Question, not a Finding.

## L1 Purpose

Load-bearing question: Is this the right job, at the right time, with honest non-goals?

### WIP

- What user or operator outcome changes if this ships?
- What is explicitly not being solved? Is that honest, or avoidance?
- Is now the right time to add this (Google design check)?
- Could a smaller Subject teach the same thing cheaper?
- What would falsify this bet in 90 days?
- Are the load-bearing requirements verifiable, or adjectives?

### Completed

- Does the running system still match the job it claims?
- Which features are unused load, and who pays for them?
- Are success metrics about user outcome or internal activity?

### Evidence

A stated job + a user/operator path + a non-goal. Product copy is not evidence. A spec that disagrees with the code is a Correctness Finding, not a Purpose Finding.

### False positives

- "I would have built a different product"
- Taste disputes about UX that do not touch a utility scenario

## L2 Shape

Load-bearing question: Are decisions that will change hidden behind small interfaces, or complected across the Subject?

Parnas: start from decisions that are difficult or likely to change; hide each in a module; reveal as little as possible. Hickey: simple is unbraided; easy is familiar. Moseley & Marks: state, control, and volume are the usual complexity sources. Essential complexity is the user's problem. The rest is accidental.

### WIP

- What will change, and is that change confined?
- Is the decomposition a flowchart of steps (usually wrong) or a hiding of decisions?
- Where is accidental state (caches, duplicated facts, derived data stored as source)?
- Is this more generic than the current problem requires?
- Can we rename this in one PR, or is the interface already published?
- If this process dies between two writes, what is true?

### Completed

- Which changes in the last six months required edits in many modules?
- Where do names lie (service named X that also does Y)? Does the same word mean two things?
- Is there a second source of truth for the same fact?
- After delete, restore, or merge, what still uniquely identifies this?

### Evidence

A change that would ripple, a type or table that is used as a bus, a module whose interface leaks its storage. "Looks messy" is not evidence.

### False positives

- "Too many files" (simple often has more pieces, unbraided)
- Layer purism (hexagonal/clean/onion as identity)
- Microservices as a Shape improvement without a hidden decision

## L3 Correctness

Load-bearing question: What must remain true, and would today's tests fail if it stopped being true?

### WIP

- What are the invariants (money, tenancy, uniqueness, ordering, idempotency)?
- Are they enforced in one place or hoped for in many?
- Do tests fail when the invariant is violated, or only when the happy path breaks?
- Is there spec vs implementation drift already?
- Is there a deny-path test (adjacent user gets 403)?
- What if this call hangs, succeeds on the server and fails on the client, or runs twice?

### Completed

- Which production bugs were invariant failures missed by tests?
- Are migrations and backfills in the same correctness story as the code?
- What happens to in-flight work during deploy?

### Evidence

An invariant stated in code or docs, a test that would miss it, a race, a non-idempotent retry. Test count and coverage percent are not evidence.

### False positives

- Missing tests for code that cannot fail a utility scenario
- Demanding tests of tests
- Type-system religion as a substitute for the invariant

## L4 Failure

Load-bearing question: What is the steady state, and what happens when a dependency, disk, or datacenter dies?

Chaos principle: hypothesize about measurable output, not internals. SRE launch checklist: machine/rack/cluster death, backend death, timeout/retry/load-shed, backup/restore. Deutsch: the network is not reliable, latency is not zero, the topology will change, the other party is not automatically trustworthy.

Cook: catastrophe needs multiple latent failures. Systems already run degraded. Safety is in the system of defenses, not in a component. Do not attribute to a single root cause.

### WIP

- What is the steady state in user terms (success rate, freshness, "money is correct")?
- For each dependency: timeout, retry, idempotency, load-shed, what the user sees
- Is degraded mode designed, or is the only mode "works / pages everyone"?
- What is the blast radius of a retry storm?
- What happens when the call returns unknown, not just success or failure?

### Completed

- Last incidents: what combined, which defense was missing, what was silent?
- Are alerts on symptoms of SLO burn, or on causes that flap?
- Can you restore data, and have you?
- What is the actual failover, not the diagram?

### Evidence

A dependency with no timeout. Retries without idempotency. No backup restore drill. Alerts that do not map to user steady state. Incident notes.

### False positives

- Demanding multi-region for a system whose utility scenario is "internal weekly job"
- Chaos experiments as virtue without a hypothesis
- Uptime as a substitute for a steady-state definition

## L5 Adversary

Load-bearing question: Which Assumptions are load-bearing, and who can violate them?

STRIDE is a prompt over data flows and trust boundaries, not a score. Spoof, tamper, repudiate, disclose, deny, elevate. Add abuse of the product, incentive hacking, and "nobody owns this." For agent-using systems: tools and data the agent can reach are in the trust model.

### Assumption kill list (time-box)

Run in order. Stop when you have 1–3 real paths (Diff/Design) or 7 (System), or a clean miss. Skip an item only with a written reason.

1. Caller vs object: authz on this entity in this tenant, or only "is authenticated"?
2. Adjacent role: what still works for the next-lower role or the other tenant?
3. Trust label: what enforces "internal," "VPN," "localhost," or "service-to-service" besides topology?
4. Control plane: admin, impersonate, flags, migrations, support, break-glass — same authz, audited?
5. Secret gravity: tokens, PII, prompts, embeddings, dumps in logs, traces, tickets, model context
6. Deny-path tests: does CI prove the adjacent user gets 403?
7. Irreversible actions: delete, pay, email, deploy, shell, export — approval, idempotency, rate limit?
8. Agent/tool authority (skip if none): whose credentials, what network, what approval, what log. The model is an untrusted user of tools.
9. Retrieved content (skip if no RAG/web/mail): untrusted text can steer tools. Is tool policy independent of model output?
10. Failure coupling: one dependency timeout — retries, queues, who is paged, what is the steady-state metric?
11. "Later": monitor, TODO, temporary, skip auth, trust — accepted risk with no owner
12. Nobody owns X: backups, key rotation, deletion/TTL, rate-limit exceptions, pager, tool policy
13. Incentive inversion: profit by lying, duplicate accounts, refunds, scrape, trap another user
14. Conway seam: interface between teams that do not share a standup
15. Detection: if 1–8 happened at 2am, which log line exists, who sees it, how is it distinct from normal?

If nothing fires: write "K1–K15 inspected; no cheap path found; residual: …" and stop. That is a successful pass.

### WIP

- Draw trust boundaries on the C4-lite map. STRIDE each crossing.
- Premortem: it failed in production because an Assumption was false. Which one?
- Abuse cases: what a rational attacker or a confused user does with the happy path

### Completed

- Authz bugs and data leaks in history
- Privilege that accumulated (admin everyone, shared tokens)
- Controls that exist on the main path and not on the export/job/support path
- Incentive and consent paths (cancellation, refunds, impersonation in UX)

### How to write the Finding

Goal. Preconditions. Boundary crossed. Effect. Blast radius. Detection gap. Fix class (check, reduce trust, isolate, make fail-closed, make attributable). What would kill this path. Confidence. No payload. No exploit steps.

### False positives

- Infinite hypotheticals ("nation state with your HSM")
- Security theater (scanner clean, authz wrong)
- Demanding a pentest to complete a Diff review
- System prompt as a trust boundary
- Mapping every note to ATT&CK

## L6 Change

Load-bearing question: Can this be reversed, deployed, owned, and recovered without heroics?

DORA throughput and instability are lagging symptoms of how you deliver. They are not goals and not a grade for a Diff. Small batches, fast recovery, and low rework show up when Change is healthy. Goodhart applies if you target the number.

### WIP

- What is the rollback or forward-fix path?
- Is this reversible if the bet is wrong in two weeks?
- Who owns it after merge, including pager and data?
- Are we locking a decision that should wait (last responsible moment vs. fake YAGNI)?
- Which one-way doors does this walk (schema, published interface, identity)?

### Completed

- Deploy: repeatable, canaried, reversible?
- Failed-deploy recovery: how long, how often, how manual?
- Bus factor: one human for a load-bearing path?
- Conway: does the org match the actual coupling, or fight it?
- Can a reviewer actually see the invariant at this diff size, or is volume the defect?

### Evidence

A deploy that cannot be undone. A migration without rollback. An OWNERS gap. A change that needs a coordination meeting across five teams because Shape leaked. Recovery time from the last failed deploy.

### False positives

- "Must deploy N times a day" as a moral rule
- Rewriting to microservices to improve DORA
- Demanding more process when the problem is coupling
