# Index Build And Overlay Recovery Optimization Plan

## Status

Proposed on 2026-07-25 and last reconciled on 2026-07-27.

| Phase | Status | Evidence / remaining work |
| --- | --- | --- |
| Phase 0 instrumentation | Partial | Overlay and base stage metrics exist; branch-delta metrics and the complete branch matrix remain. |
| Phase 1 manifest-only startup | Complete | Published overlays reopen without durable-history replay; Chromium restart and query checks passed. |
| Phase 2 incremental refresh | Complete | Watcher-driven no-change and event delta paths are proportional to changed paths. |
| Phase 3 catalog transaction | Partial | Compact v2 catalog, one-write success path, and generation CAS are complete; publication-boundary kill/restart matrix remains. |
| Phase 4 overlay bulk build | Partial | Bounded preparation, parallel term collection, and 10k/50k Chromium runs are complete; streaming writer and aggregate memory budget remain unimplemented. |
| Phase 5 base builder | Partial (core path accepted) | Git-object sources, 2M spill runs, and ten shards are accepted; aggregate memory bounding remains open, while bucket parallelism and 192 MiB posting residency were measured and rejected. |
| Phase 6 branch delta | Pending | H0 -> H1 -> H2 -> H0 Chromium matrix and any evidence-driven delta optimization remain. |
| Final gate | Blocked by known failures | Chromium query results are exact, but five watcher/rebuild timing tests still fail in the serial full Rust gate. |

## Next Execution Queue

1. Resolve the five reproducible watcher/overlay-rebuild timing failures and restore a green serial
   Rust gate.
2. Run the Phase 3 publication-boundary kill/restart matrix and fix any mixed manifest/catalog
   recovery behavior.
3. Run the Phase 6 Chromium H0 -> H1 -> H2 -> H0 branch matrix, including tokenized-object counts,
   synchronization wall time, CPU/RSS, index bytes, and queries during/after transition.
4. Implement the Phase 4 streaming overlay writer and aggregate memory budget only if the branch
   and O4 profiles still show material writer or peak-RSS cost.
5. Run the complete automated gate and publish the final consolidated validation result.

This document is the execution and acceptance contract for improving full base construction,
workspace overlay refresh, daemon restart recovery, and branch-delta construction. Every
performance change must be measured on `chromium-src` immediately after it lands; a final benchmark
alone is not sufficient.

Once a new primary path is accepted, it replaces the previous slow path. The implementation must
not retain the old implementation as a compatibility branch or accumulate speculative fallback
logic. A rebuild remains valid only for an explicit external constraint: missing/incompatible or
corrupt artifacts, an explicit rebuild request, or an overlay state that cannot be represented as a
delta with the current schema.

## Baseline

Measurements from `docs/chromium_query_benchmark_2026-07-25.md`:

- Chromium tracked files: 493,140;
- indexable base documents: 445,491;
- isolated base index size after returning to H0: approximately 5.49 GiB;
- full base build: 130.31 seconds, although that sample had unrelated indexing interference;
- overlay refresh: 7.91 seconds at 1,000 files, 16.29 seconds at 10,000, 59.59 seconds at
  50,000, and 94.01 seconds at 68,044;
- fresh-daemon `open_repo` with a 68,044-file published overlay: more than two minutes;
- ready-daemon query latency with the same overlay: generally 0.18-0.96 seconds.

The large gap between restart time and ready query time shows that overlay reconstruction is the
first target. Full base construction is the second target and must be profiled before its pipeline
is changed.

## Goals

1. Reopen a valid published overlay without reading and retokenizing all dirty documents.
2. Make overlay refresh proportional to newly changed paths when the visible dirty set only grows
   or changes in place.
3. Avoid repeated full dirty-catalog serialization for one publish operation.
4. Use bounded parallelism for large overlay document preparation and posting construction.
5. Reduce full base construction wall time without increasing peak RSS or index size materially.
6. Preserve branch-delta behavior: only changed Git objects are tokenized.
7. Preserve exact query results before, during, and after rebuild, restart, and branch transition.
8. Produce reproducible Chromium A/B evidence after every implementation phase.

## Non-Goals

- Optimizing warm repetitions of the same query.
- Removing base sharding. Stable shards remain useful for initial-build parallelism and explicit
  full-build reuse.
- Hiding synchronization time inside query latency.
- Treating the dirty catalog as query truth. Published manifests remain authoritative.
- Avoiding the cost of indexing content that genuinely changed.
- Changing index schema merely to improve benchmark presentation.

## Required Invariants

### Published Views Are Query Truth

Search reads only an activated base manifest plus an activated overlay manifest. Dirty-catalog
entries and in-progress build state never become query-visible by themselves.

### Startup Never Returns A Stale Overlay

A persisted overlay may be reopened only when all of the following match:

- overlay manifest `base_manifest_id` equals the active base manifest;
- overlay manifest `observed_head` equals the current Git HEAD;
- repository root and tokenizer/configuration identity match;
- the dirty catalog is structurally valid;
- catalog versions for paths that may have changed while the daemon was stopped are reconciled
  before the indexed view is reported ready.

Invalid identity takes one explicit rebuild path. Corrupt artifacts take one explicit quarantine
and rebuild path. There must not be a chain of progressively slower recovery branches.

### Append Reopen Does Not Materialize Durable History

Opening an immutable published overlay for querying and future delta appends must not call
`IndexWriter::load_durable_history`. Manifest-only writer restore must be sufficient for subsequent
delete/add delta publication.

### Performance Numbers Separate Phases

Every report must separately measure:

- worktree/Git reconciliation;
- document reading and tokenization;
- segment/posting write;
- manifest/catalog publication;
- daemon `open_repo`;
- the first query after readiness.

## Chromium Benchmark Protocol

All phases use `/Users/user/workspace/chromium-src` through a detached temporary worktree. The
user's active Chromium worktree and its existing changes must not be modified.

The benchmark binary is `target/release/flashgrep`. Each A/B pair uses the same Chromium commit,
configuration, query map, overlay file list, and storage layout. Raw CSV/JSON and process samples
are kept under `/tmp`; summarized results are appended to
`docs/chromium_index_build_optimization_validation_2026-07-25.md`.

### Environment Controls

- build release binaries before each A/B run;
- stop unrelated flashgrep benchmark daemons;
- do not run another base build concurrently;
- record commit, binary hash, macOS version, CPU count, and available memory;
- use a fixed detached-worktree path because base metadata binds artifacts to the canonical root;
- label measurements as process-cold; macOS page cache is not forcibly purged;
- alternate baseline/optimized order when a phase is sensitive to page-cache warming;
- monitor daemon RSS, CPU, elapsed time, and index-directory size once per second.

### Correctness Query Set

Use all cases in `benchmarks/chromium_queries.tsv`. Compare matching-line counts with `rg
--no-ignore` while excluding `.git`, using identical glob and path scopes. Every measured case must
be exact.

### Overlay Matrix

Create cumulative overlays by appending a deterministic marker to the same tracked `.cc` list:

| State | Modified files |
| --- | ---: |
| O0 | 0 |
| O1 | 1,000 |
| O2 | 10,000 |
| O3 | 50,000 |
| O4 | 68,044 |

For every state record refresh time, indexed documents, overlay segment count/bytes, catalog bytes,
fresh-daemon `open_repo`, first-query latency, peak RSS, peak CPU, and result correctness.

### Base And Branch Matrix

- empty-storage full base construction at H0;
- explicit rebuild at the same H0;
- H0 -> H1 with 1,000 committed file changes;
- H1 -> H2 with 9,000 additional committed file changes;
- H2 -> H0 cached-manifest activation;
- first query during and after each transition.

## Phase 0: Instrumentation And Reproducible Baseline

Add structured timing and counters before changing the algorithms.

Required measurements:

- Git enumeration/status and filesystem metadata time;
- worktree-diff scan files/bytes and hash time;
- files and bytes read for overlay/base preparation;
- tokenization time and posting-term count;
- posting in-memory, spilled, sorted-run, and merge bytes;
- segment document/posting write time;
- manifest and dirty-catalog serialization/write time;
- overlay durable-history documents read during open;
- per-shard elapsed time, documents, source bytes, and posting records.

Chromium gate:

1. Run the complete overlay and base/branch matrices.
2. Confirm instrumentation changes results by zero files and zero matching lines.
3. Save the isolated baseline used by every subsequent phase.

## Phase 1: Manifest-Only Overlay Startup

### Current Problem

`RepoManager::open_repo` bootstraps the base and then unconditionally calls
`refresh_workspace_overlay`. That method plans a full worktree rebuild. Separately,
`WorkspaceOverlayIndex::open_with_policy_and_hook` uses normal `IndexEngine::open`, which restores
all durable document history.

### Implementation

1. Add a manifest-only overlay constructor using `IndexEngine::open_manifest_only`.
2. Validate active overlay manifest source against active base manifest, current HEAD, repository
   root, and configuration.
3. Restore the overlay reader/writer state directly from its manifest.
4. Restore the published-overlay path cache from the validated dirty catalog.
5. Reconcile catalog file versions changed while the daemon was stopped.
6. Publish readiness after validation; schedule ordinary watcher reconciliation afterward.
7. Replace unconditional startup full rebuild with this path.
8. Use one rebuild path only when persisted identity or artifacts are invalid.

### Tests

- valid overlay restart performs zero segment-document reads and zero tokenization;
- delta update works after manifest-only reopen;
- file modified, deleted, added, or restored while daemon is stopped is reconciled correctly;
- base manifest or HEAD mismatch rejects the persisted overlay;
- interrupted manifest/catalog publication cannot expose a mixed view;
- corrupt manifest or catalog enters the single recovery path.

### Chromium Gate

Run fresh-daemon open at O0-O4 and all correctness queries. Acceptance:

- O4 open performs zero overlay tokenization;
- O4 open falls from more than two minutes to at most 15 seconds;
- first-query counts remain exact;
- O4 ready query latency does not regress by more than 10%;
- peak restart RSS is lower than the baseline.

### Phase 1 Result (2026-07-25)

The implementation is complete. A published overlay is reopened with manifest-only writer restore,
validated against the active base and current HEAD, and reconciled through Git candidates plus
dirty-catalog file versions. Chromium measurements are recorded in
`docs/chromium_index_build_optimization_validation_2026-07-25.md`.

## Phase 2: Incremental Explicit Refresh

### Current Problem

`refresh_workspace_overlay_with_progress` always calls
`plan_overlay_full_rebuild_from_worktree`, even though the event/incremental planner can already
choose `NoChange`, `ApplyDelta`, or `Rebuild`.

### Implementation

1. Make explicit refresh use the same reconciliation planner as an incremental probe.
2. Apply only dirty-catalog pending paths when the visible dirty set grows or changes in place.
3. Keep full overlay reconstruction only when an already-published path becomes clean and the
   current overlay schema cannot express "unshadow base".
4. Remove the unconditional full-rebuild refresh path after the incremental path passes tests.
5. Report total dirty paths separately from paths actually read/tokenized.

### Tests

- O1 -> O2 tokenizes exactly 9,000 newly modified files;
- O2 -> O3 tokenizes exactly 40,000 newly modified files;
- modifying an already-dirty file publishes only that file;
- deleting and adding dirty files preserves visibility;
- restoring a dirty file to base invokes the one required rebuild path;
- no-change refresh writes no segment or manifest.

### Chromium Gate

Run the cumulative O0-O4 matrix. Acceptance:

- tokenized file count equals the pending delta count for growing overlays;
- O1 -> O2 and O2 -> O3 are faster than Phase 1 baseline by at least 20%;
- no-change refresh completes without segment writes;
- query results remain exact at every state;
- segment count stays within merge-policy bounds.

### Phase 2 Result (2026-07-25)

Explicit refresh now uses the incremental planner for same-HEAD watcher events. HEAD changes and
base-advance completion use an explicit full-rebuild method, so branch task ordering is not inferred
from the dirty set. No-change refresh waits briefly for watcher delivery and does not run a whole
repository diff when a published snapshot is already current.

Chromium measurements: no-change refresh fell from 4.3 seconds to 0.14 seconds, and a 100-path
in-place delta fell from 4.4 seconds to 0.17 seconds. Both remained exact.

## Phase 3: Dirty-Catalog Transaction And Compact Persistence

### Current Problem

One overlay publication can serialize and atomically replace the complete pretty-printed dirty
catalog during reconcile, mark-building, and mark-ready transitions.

### Implementation

1. Treat overlay segment/manifest publication plus catalog-ready state as one recoverable
   transaction.
2. Keep building state in memory and persist the final catalog once after successful manifest
   activation.
3. Persist one failure record only when publication fails.
4. Replace pretty JSON with a compact versioned encoding while keeping deterministic ordering.
5. On startup, resolve interrupted state from the activated manifest and transaction identity.
6. Remove the three full-catalog writes from the normal success path.

### Chromium Gate

At O1-O4 record catalog bytes serialized and written. Acceptance:

- one successful catalog write per refresh;
- O4 catalog publication bytes fall by at least 50%;
- O4 refresh wall time improves or remains within 5% when tokenization dominates;
- kill/restart recovery at publication boundaries remains exact.

### Phase 3 Result (2026-07-25, partial)

The normal success path no longer persists the transient `building` state. The reconciled dirty
state remains the crash-recovery record, while the final `ready_overlay` state is persisted after
manifest publication. Catalog serialization is now version 2 compact array JSON with deterministic
BTree ordering; version 1 object catalogs are read once and migrate on the next successful write.
Each reconcile increments an in-memory catalog generation, and plans may mark entries ready/failed
only when both generation and dirty kind still match. This prevents stale concurrent watcher plans
from overwriting a newer path state without adding another durable write.

On a 2,000-entry Chromium catalog, bytes fell from 695,544 (pretty v1) to 321,532 (compact v2), a
53.8% reduction. The successful-path transaction consolidation is implemented; the kill-boundary
matrix remains open.

## Phase 4: Bounded Parallel Overlay Bulk Build

### Current Problem

Overlay document loading, `IndexDocument` conversion, tokenization, and generic
`build_posting_lists` are serial. The 50,000+ overlay experiments sustained approximately one CPU
core during large rebuild/reopen work.

### Implementation

1. Prepare overlay documents in a bounded worker pool outside the writer lock.
2. Preserve deterministic path/doc-id order after parallel preparation.
3. Replace the generic in-memory segment path for large batches with a streaming bulk segment
   writer that supports inline worktree sources and the required delete segment.
4. Give the worker pool a global memory budget based on prepared source bytes and posting records.
5. Parallelize independent posting buckets/runs, not individual writes to one shared writer.
6. Replace the serial large-batch path; do not retain it as a size-based compatibility branch.

### Chromium Gate

Run a first-time O4 build and O1 -> O4 cumulative growth. Acceptance:

- first-time O4 refresh is at least 2x faster than Phase 3;
- CPU utilization scales beyond one core during preparation;
- peak RSS does not exceed the original 5.75 GiB Chromium peak;
- index and catalog size do not grow by more than 5%;
- all 14 query cases remain exact.

### Phase 4 Result (2026-07-25, partial)

Overlay document preparation now uses a bounded eight-worker local pool while preserving the
existing deterministic path ordering at segment construction. On a 2,000-file Chromium rebuild,
preparation measured 9 ms for 23.6 MB; segment publication fell from 1.77 seconds to 0.58 seconds
after parallel per-document term collection. A monitored rebuild peaked at 2.66 GiB RSS and 151.5%
CPU, below the 5.75 GiB Chromium peak from the original full matrix. Cumulative Chromium runs then
measured a 10k delta at 41.30 s open / 2.70 s delta publish with 628.1% CPU, and a 50k rebuild at
12.92 s open / 12.79 s publish with 936.8% CPU and 3.52 GiB peak RSS. The 50k run had a warmer page
cache and is not a strict A/B comparison, but it confirms the path scales beyond one core. The
streaming writer and aggregate memory-budget work remain open.

## Phase 5: Full Base Builder Pipeline

This phase begins only after Phase 0 identifies the dominant base-build stages.

Candidate changes, applied only when supported by the profile:

1. Replace nested shard-thread plus Rayon scheduling with one bounded scheduler.
2. Allocate one global posting-memory budget across active shards instead of 128 MiB per shard.
3. Parallelize independent posting-bucket sorting and merging.
4. Reduce spill write amplification with larger adaptive runs or radix sorting of `(term, doc_id)`.
5. Stream Git tree enumeration instead of buffering the complete `ls-tree` output.
6. Avoid redundant metadata/content reads when Git object identity already proves a file unchanged.
7. Reuse prepared buffers and avoid repeated lowercasing, sorting, and allocation where possible.

The stable shard/fingerprint model remains. Chromium measurement selected ten shards; previous
sixteen-shard manifests remain readable and naturally rebuild into the new assignment.

### Phase 5 Result (2026-07-25, accepted slices)

Base construction now uses the Git tree input path whenever a HEAD exists, including a clean
worktree. Regular files are stored as `GitHeadBlob` sources rather than duplicating their bytes as
inline segment sources; symlinks retain the prior worktree-following behavior. This removes the
old clean-worktree-only input branch and makes shard fingerprints object-identity based.

The new instrumentation reports Git status/tree/classification and per-shard preparation, append,
and finish timings. On Chromium H0 the new path indexed the same 445,491 documents with all 14 query
counts exact. The optimized index was 2,522,784 KiB versus 5,253,040 KiB for the prior inline-source
build, a 52.0% reduction. A pre-symlink-correction optimized run measured 76.26 s wall time; the
final semantics-preserving run overlapped another benchmark daemon and is not a timing sample.
Earlier isolated runs were 84.38 s before the duplicate binary scan removal and 72.81 s after it. A four-shard-worker
run took 83.22 s, so the default available-parallelism setting remains preferable. The resource
sample reached 3,945,185 KiB RSS during a run with concurrent benchmark interference and is not used
as a clean peak comparison.

The second slice increased the normal spilled-bucket sort run from 512k to 2M records while
retaining the 64k low-memory setting. Adjacent Chromium H0 runs with low-memory auto-detection
disabled measured 70.83 s at 2M versus 80.12 s at 512k, an 11.6% improvement. The first wave's
per-shard finish time fell from repeated 9-12 s samples to roughly 5-8 s. Parallel sorting of all
buckets was also tested and rejected because nested shard/Rayon scheduling did not improve the
84.48 s full-build sample. Global posting-memory budgeting remains open.

The same-H0 explicit rebuild completed in 13.11 s with all 16 prior shards reused and no
segment-build event. Its remaining cost was Git input enumeration plus clean overlay probing. A
fresh H0 matrix then measured 63.84 s at 10 shards, 68.39 s at 8 shards, and 70.83 s at 16 shards
with the same 2M run setting. Ten shards is now the selected default because it matches the measured
worker count; old manifests remain readable and naturally rebuild into the new assignment when
explicitly rebuilt.

A 192 MiB per-shard posting threshold was rejected: the ten-shard H0 build regressed to 71.32 s
from 63.84 s at 128 MiB, with finish stages increasing to roughly 9-11 s. The global-budget work
therefore retains 128 MiB as the performance point and should focus on bounding aggregate memory,
not increasing per-shard residency.

### Chromium Gate

Run empty-storage H0 build and same-H0 explicit rebuild. Acceptance:

- full build improves by at least 20% over the isolated Phase 0 baseline;
- peak RSS does not increase;
- final index bytes stay within 5%;
- document count and all query counts remain exact;
- shard reuse on explicit same-H0 rebuild reads/tokenizes zero documents.

## Phase 6: Branch Delta Construction

Only proceed if Phase 0/5 profiling shows meaningful remaining cost in branch advance.

Candidate changes:

- batch Git old/new blob reads instead of serial request/flush cycles;
- prepare target-side changed documents in bounded parallel workers;
- use the bulk segment writer for large deltas;
- keep delete-before-add sequence ordering and candidate CAS activation unchanged.

Chromium acceptance:

- H0 -> H1 and H1 -> H2 improve without tokenizing unchanged files;
- H2 -> H0 remains a zero-tokenization cache activation;
- search stays available and exact during transition;
- foundation segments are neither read nor rewritten by normal advance/merge.

## Automated Gates After Every Phase

Run all of the following before Chromium performance validation:

```text
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
node --check mcp/server.js
```

Also run focused daemon/MCP tests for restart, overlay visibility, branch transition, corruption,
and interrupted publication. A performance win does not pass if any correctness backend, result
count, manifest identity, or recovery invariant changes unexpectedly.

## Stop And Rollback Rules

A phase is rejected or revised when any of these occurs:

- any Chromium query count differs from the tracked-corpus rg reference;
- query fallback remains active after the indexed view should be ready;
- a normal restart or growing overlay performs full document tokenization;
- peak RSS increases by more than the phase's allowed bound;
- index bytes grow by more than 5% without a measured latency justification;
- interruption can publish a manifest/catalog pair from different transactions;
- the improvement is within benchmark noise in two alternating A/B runs.

Rollback means reverting the incomplete phase, not retaining both old and new execution paths.

## Delivery Order

1. Complete: instrument overlay/base construction and capture Chromium baselines.
2. Complete: implement manifest-only overlay startup and incremental explicit refresh.
3. Partial: consolidate dirty-catalog persistence; finish its interruption matrix.
4. Partial: implement bounded overlay preparation; defer the streaming writer until profiling
   proves it remains worthwhile.
5. Complete for accepted slices: optimize Git-object base construction, posting runs, and shard
   scheduling.
6. Pending: profile and optimize branch delta construction.
7. Pending: restore the full automated gate and publish the final validation report with
   correctness checks, resource peaks, and before/after tables.

The expected largest user-visible gain is Phase 1: eliminating minutes of unnecessary overlay work
after daemon restart. Phase 2 and Phase 4 target large-change refresh latency. Phase 5 targets the
less frequent but still expensive initial full build.
