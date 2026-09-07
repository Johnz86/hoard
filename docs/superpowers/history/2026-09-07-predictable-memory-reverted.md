# The predictable-memory run, and why it was reverted

Written on 2026-09-07 after the run was halted by the project owner at the end of wave 4.
The code it produced is reverted; the commits stay reachable at the tag
`reverted/2026-09-07-predictable-memory` if a measurement ever says they were worth it.
This document exists so the knowledge outlives the code, and so the next run does not
repeat the process that produced it.

## What happened

The goal document, deleted with the run and readable at the tag as
`docs/superpowers/specs/2026-09-07-predictable-memory-goal.md`, set five waves. Four landed:
a continuous hashing dispatch with a `hash_threads` setting; a byte-metered index writer; a
usage log keyed by repository with a minute-grained reindex and a bounded sink; and a summary
fold with a grouped walk and a `cache_mib` setting. The fifth, the measurement, was stopped
before it finished.

The run added about 1,900 lines to production code and 1,296 to one test file, bumped
`FORMAT_VERSION` from 7 to 8, and changed the shape of a stored record.

**It measured nothing.** Not one commit of the run carries a number.

## Why it was reverted rather than cleaned up

The baseline at `9979f7b` meets every standing gate at a million artifacts. The head did not
demonstrably beat it on anything, because nothing was measured, and it carried nine defects
that this run introduced and that review caught. How many it introduced and review missed is
unknown, which is the point.

Cleaning up would have cost more than reverting and would have left the same unanswered
question. Measuring first is likely to show that most of these four waves were not worth
doing at all: the reviews already established that wave 2's budget cannot bind on any fixture
that exists and that wave 4's grouping is invisible on the gate tree. The cheapest path to a
correct codebase is to stand on the baseline and let evidence ask for the next change.

## What the process got wrong

These are causes, not symptoms, and each one has a rule attached to it now.

1. **A goal was written where the owner's own analysis said little was left.** The owner's
   ranked list of 2026-09-07 named eleven items; at the head, its top priorities were already
   implemented, including the two the owner said their first implementation wave would
   combine. That was found and reported, and a five-wave goal was written anyway.
2. **"Measure once, at the end" was inherited from the previous run and applied to a goal
   that was entirely about memory.** That made every wave speculative. The rule worked before
   because that run was about correctness and scope; it is exactly wrong for optimisation.
3. **The review loop turned into a bug factory and the trend was read as health.** Findings
   per wave rose rather than fell, because each fix round added code the next review then
   faulted. Wave 4 took eight commits and three rounds. Nine of the defects found were
   introduced by this run, in code that replaced code which passed every gate.
4. **Every wave grew during execution.** Wave 4 was about scan memory and ended up deleting
   two pre-existing functions and changing the mirror's ingest and pull write paths. Each
   expansion was justified on its own and none was justified against the wave's purpose.
5. **Process rules were written and edited while being used, on no evidence.** One of them,
   "a fix a review asks for is a task appended to the same wave for the same implementer",
   is what produced a single agent transcript that writes something and then deletes it.
6. **The specification needed four corrections in four waves.** Six of the ten commits on
   `master` were documents. A spec that trails the work is not guiding it.
7. **The owner got no checkpoint and the reports concealed the trend.** Each wave was
   summarised as a success, because "review found things and we fixed them" reads as a
   healthy loop. No report said "this wave introduced a data-loss path that did not exist
   before", which was true twice.
8. **The application was never run.** Every claim was validated by a test suite the run was
   simultaneously growing.

## What the reviews established, which is worth keeping

These are facts about this codebase and its dependencies, verified against source during the
run. They are the run's real output.

- **`TantivyDocument::new()` reserves 1,024 bytes** (`CompactDoc::with_capacity(1024)`) plus
  eight bytes per value in `field_values`, and neither tantivy nor hoard ever calls
  `shrink_to_fit`. Any estimate of what a queued document holds that counts only its text
  reads about a fifth of the truth.
- **Tantivy's operation channel is bounded at 10,000 groups**, and hoard sends one group per
  artifact so its documents land in one segment. A byte budget therefore only becomes the
  binding constraint above roughly 13 KiB of documents per artifact; below that the channel
  is the tighter cap and a budget changes nothing.
- **One artifact's documents cannot be bounded while a revision's card is indexed into every
  entry document of that revision**, and a card may reach 4 MiB. Bounding it means not
  repeating the card, which changes what a non-primary entry document matches.
- **A native_db secondary scan materialises every matching primary key before yielding**, so
  `take(n)` on one pays for the whole match; a primary scan is a lazy redb range that streams
  and exits early. This is why keying the usage log by repository was worth considering, and
  it also makes forgetting a repository's log quadratic in its length under a secondary scan.
- **The summary fold telescopes.** `SummaryPart::of` reads only the record and
  `shift_summary` moves the stored total by a difference, so shifting `a -> b` then `b -> c`
  equals `a -> c`. That is what would make a per-transaction fold safe.
- **Two clones of one remote share a repository row**, because a clone's identity comes from
  its remote URL. Any change that processes a source's repositories in batches must keep a
  repository's rows in one batch, or one batch retracts what the next restores, which sweeps
  the artifact and orphans the `Override` keyed to its id.
- **A delete-only writer group estimates zero bytes**, so any commit predicate based on bytes
  rather than on whether anything was written will skip a real pending deletion and then
  advance a cursor past it.
- **The gate tree cannot show most memory work.** It is a loose source of a hundred
  repositories with one artifact per file and no card, so it takes every single-group path,
  never crosses a per-artifact byte budget, and runs at the default cache size.
- **The scan's per-artifact structures are `Counters.before` at 56 bytes each and
  `Counters.touched` at 8.** At a million artifacts the first is about 136 MB resident and
  200 MB across its final rehash. `touched` cannot be scoped without giving the hashing
  pass's artifact merges an in-transaction sweep.
- **A retirement killed before its recount leaves artifact path counts stale with nothing
  able to find them**, because what it owes is only in memory and `hoard doctor` compares two
  reads of the same stored value. This predates the run and is still true.

## What the next run must do differently

Recorded as rules in `.claude/skills/`, not as intentions here:

- Establish that a problem exists, with a number, before planning a change to it.
- A change that no available measurement can show does not ship; build the workload first or
  do not build the change.
- A review finding that needs more than a small correction means the wave was wrong. Stop and
  re-plan rather than appending fix tasks to the same agent.
- Halt the run and ask the owner when findings per wave rise, or when a wave introduces a
  defect that did not exist before.
- Report the trend, not the events.
