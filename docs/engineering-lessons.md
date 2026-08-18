# Engineering Lessons

Distilled practices, extracted from defects this project actually shipped.

**This file exists so `facts/` does not have to carry them.** A contract states
what a conforming implementation must do; it is a description of current
reality, not an argument for it. When a rule in `facts/` was learned the hard
way, the rule stays in the contract and the lesson comes here.

**Entry criteria.** A lesson lands here only if it generalises past the single
site that produced it — if it would change how you write unrelated code. A
finding that only says "component X does Y" belongs in X's contract, not here.
Each entry names the concrete instance, because a rule without its failure is
unmemorable and gets optimised away by the next reader.

**What is deliberately not here.** The blow-by-blow: dates, commit hashes, who
found it, what the previous draft said. `git log` and `CHANGELOG.md` hold that,
and it is only ever needed by someone reconstructing a decision, not by someone
implementing against it.

---

## Traits and interfaces

### A defaulted trait method behind a delegating wrapper is a silent no-op

`BlockValidator` carried four methods with do-nothing default bodies. The
`Validator` enum wrapper in `src/main.rs` had to forward each one to the active
variant. When `resize_cache` was added, the wrapper was not updated — and it
compiled, because the trait supplied a default returning `Ok(())`. The at-tip
cache resize called into the wrapper, hit the default, did nothing, returned
`Ok`, and **logged success**. The real implementation was correct throughout
and simply unreachable. It never once reached `state.redb`.

`flush` had the same shape and would have been far worse: a forgotten forward
means state is never persisted while every caller is told it was.

**Detecting the mistake is weaker than removing it.** "Declare every method, no
defaults, let the compiler catch the next wrapper that forgets" works, but it
still depends on someone reading the compiler errors correctly next time. The
fix that holds was splitting the trait so that the methods with one consumer
each live on narrow traits that the wrapper does not implement at all — there is
no forwarding left to forget.

**Applies to:** any enum-dispatch or newtype wrapper over a trait. The default
body is the hazard, not the wrapper.

### One `Option` must not carry two facts

`emission_box_id` returned `None` for *either* "this is digest mode" *or* "all
ERG has been emitted" — two unrelated facts wearing one value. The single
consumer early-returned on both identically, so a mode question and an emission
question produced the same behaviour, and neither was visible at the call site.

**Applies to:** any sentinel return. If two callers would want to distinguish
the cases, the type must distinguish them. Absence of a capability and absence
of a value are different kinds of nothing.

---

## Watermarks and derived state

### Verification must not lag application

Deferred script evaluation let `apply_state` return before the scripts had run,
which bought sync throughput and created a second source of truth for how far
the chain was actually verified. Three separate bugs came out of that one root:

- a reorder buffer that was drain-local, so any result overtaking its
  predecessor was found non-contiguous and discarded. The watermark froze for
  190,000+ blocks while every consumer read it as "validated up to here".
- an in-flight counter zeroed on failure while its rayon tasks were still
  running and still holding their heap, which opened the dispatch gate early.
- an unbounded backlog that killed a 4-thread 1.8 GHz host at anon-rss
  10.62 GiB mid-catch-up.

**A second watermark that trails the first can only ever disagree with it.**
Removing the lag removed the whole class. Treat "make it async for throughput"
against a verification boundary as a proposal to reopen all three.

**Applies to:** any pair of watermarks where one is supposed to trail the other.
Ask what a consumer does when they disagree; if the answer is "nothing sensible",
there should be one watermark.

---

## Measurement and observability

### A bytes-per-unit constant outlives the structure it describes

`/debug/memory` computed a header-cache figure from a local
`AVG_HEADER_BYTES = 800` describing a `Vec<Header>` in another crate. When that
crate retired the `Vec`, the constant kept multiplying it by the header count
and reported ~1.48 GB for a structure that did not exist — a figure exceeding
total process RSS. Nothing cross-checked the two crates, so it survived four
months.

**Compute from the real structures or report nothing.** If only a count is
obtainable, return the count and omit the derived bytes. And the crate that owns
a structure computes its size; a consumer that transports the number cannot
notice when the structure changes shape.

**Applies to:** every gauge. A derived number with no link to the thing it
describes is not an underestimate or an overestimate — it is unfalsifiable.

### Call order between a shared buffer's producer and consumer is load-bearing

`generate_proof()` clears the changed-node buffers that `update_internal` reads
via `removed_nodes()`. Called in the wrong order the per-block delete list comes
back empty, every superseded node is orphaned, and nothing errors: the tree
stays consistent and correct, the database grows two orders of magnitude past
the live tree (235 GB against a 4–8 GB tree). Both calls succeed. Both are
individually correct.

**Applies to:** any pair where one call fills a buffer and another drains it.
The dependency is invisible at both call sites, so it belongs in the contract as
an ordering rule, not in a comment at one of them.

### A crate function with no caller outside its own tests is a second implementation waiting to diverge

`mining::generate_candidate` was the documented sole entry point for candidate
assembly. `src/main.rs` built the `CandidateBlock` inline instead — including
its own copy of the `max(now, parent.timestamp + 1)` timestamp rule — and never
called it. So `select_transactions` and `build_fee_tx` had no production caller
either: the path that would have called them was not the path that ran. Mined
blocks carried the emission transaction alone and miners collected no fees, for
months, with every test passing.

**"Not yet wired" and "duplicated" look identical from inside the crate.**
Check that the entry point has a caller outside its own tests, and that the rule
you just wrote does not already exist somewhere upstream.

---

## Tests

### A test that recomputes the value the way the implementation does proves consistency, not correctness

The mining API served `b` (the PoW target) as the *difficulty* — `q/difficulty`
is the target, and the code returned the divisor. The serve path advertised a
bound tens of orders of magnitude harder than `check_pow` enforced, so no share
was ever submitted at any hashrate. It shipped in 42 consecutive tagged
releases. Three tests covered `b` for the life of the project and all three
passed, because each asserted the served value against the same formula the
implementation used.

The same shape produced a header-window bug: a test asserting that two windows
*agree*, where the code controls both, passes while both are equally wrong.

**Pin regression tests to values observed from an independent source** — a
capture from the reference implementation, a known-good vector — not to your own
definition. And when two sides must agree, assert each against an absolute
value, not against each other.

### A miner that accepts your candidate and reports nothing is not idle

Byte-perfect `msg`, well-formed JVM-shaped JSON, parsed and accepted — and
unmineable. "Verified at runtime against mainnet tip" only ever meant assembly
was correct. Compare **every** served field against the reference, not the ones
that a client visibly rejects.

**Applies to:** any protocol where the failure mode of a wrong value is silence
rather than an error.

---

## Defaults and inherited values

### A library default is a number nobody chose

`RedbModifierStore` constructed with a bare `Database::create` inherited redb's
default page cache of 1 GiB *per handle*. Nobody picked that figure, nothing
reported it (`/debug/memory` shows redb's logical cache occupancy, not its
allocations), and it was the dominant allocation of the header phase during a
genesis sync.

**Applies to:** every resource-sizing constructor you call without an explicit
argument. An inherited default is a decision made by someone who did not know
your workload.

---

## Documentation

### Correcting a claim in one contract leaves it standing in its sibling

`facts/validation.md` retracted "the prover gauge publishes at flush cadence"
and named the real mechanism. `facts/api.md`, which transports the same figures,
still said flush cadence — the correction had been scoped to the file where the
argument happened. Both files were internally coherent and the pair disagreed.

**When a rule changes, grep the other contracts for the old wording before
closing.** And prefer stating the rule once in the owning contract with a
cross-reference from the consumer, so there is only one place to correct.
