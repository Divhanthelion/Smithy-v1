# Snowball

Read this when you have a seed set and are about to traverse. Do not read it to go looking for seeds — that is ordinary search.

## Seeds

A seed is a document you would defend to the user: artifact owner, original paper, spec, first-party source. A blog, a survey, or a secondary "deep research" report is not a seed. If the seeds are poisoned, the graph is poisoned — restart.

Write the seed list in the open before the first hop. Three to seven is enough. One is a point, not a set.

## Traverse

Each generation:

1. **Backward** — fetch the seed's bibliography / references. Keep items that could discriminate the frozen hypotheses or answer the pinned question. Drop the rest, in Dropped.
2. **Forward** — find what cites the seed (publisher "cited by", Semantic Scholar, OpenAlex, the paper's official page). Same keep/drop rule.
3. Fetch the kept documents. Skim titles is not reading. Quote or locate precisely, same as Findings.
4. New keepers become seeds for the next generation.

Do not treat a citation as evidence of the cited claim. The citation is a pointer. Read the target.

## Stop

Stop at the first of:

- The stop rule from the pin
- A full generation that added **no diagnostic** evidence (consistent-with-everyone does not count)
- The graph leaving the pinned question (drift) — cut, don't follow it "one more hop"

Saturation is "nothing new that changes hypothesis ranking," not "I have many PDFs."

## Failures

- **Bad seeds** — keyword-popular papers that don't own the artifact. Replace the set; don't hop further.
- **Terminology drift** — forward cites that only share a word. That's a new search, not a snowball.
- **Unfetched cites** — if you cannot fetch it this session, it is Unknown, not a finding.
