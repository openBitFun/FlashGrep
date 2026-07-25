# Chromium Index And Overlay Optimization Validation

Validation date: 2026-07-25

Repository commit: `606acc70f6320a025c719d7eee8a4583c9205252`

Binary: release `flashgrep` from the current worktree. Measurements are process-cold; the macOS
filesystem cache was not purged. Chromium ran from a detached worktree. The existing 445,491
document base index was copied with APFS clone-on-write and its repository identity was updated to
the detached worktree. The main Chromium worktree was not modified.

## Changes Validated

1. Workspace overlays reopen with `IndexEngine::open_manifest_only`; active segment documents are
   not loaded into writer history during daemon startup.
2. Startup validates the overlay source against the active base manifest and current Git HEAD.
3. Git supplies the complete tracked/untracked candidate path set. A ready dirty catalog with
   matching size/mtime versions bypasses content reads and hashing.
4. The successful catalog path keeps the transient `building` state in memory and persists the
   final ready state once after manifest activation. Catalog generations prevent stale concurrent
   plans from marking newer tracked or untracked states ready/failed.
5. Same-HEAD explicit refresh consumes watcher candidate paths through the incremental planner.
   HEAD changes, base-advance completion, and invalid artifacts use the explicit full-rebuild path.
6. Overlay document preparation uses a bounded eight-worker pool; segment construction keeps its
   existing deterministic ordering.

## Chromium Results

| Scenario | Before | After | Result |
| --- | ---: | ---: | --- |
| Published overlay restart before manifest-only recovery | 9.43 s | 4.90 s | Correct, one dirty path |
| No-change restart, 2,000-file overlay, before catalog version short-circuit | 10.75 s | 5.36 s | 3 active segments retained |
| Append 1,000 files to an existing 1,000-file overlay during downtime | full-overlay path historically grows with all dirty files | 11.77 s in the evaluated incremental prototype | 2,000 dirty files, one new segment |
| First marker query after optimized restart | - | 0.10 s | 1,000 matching lines |
| Dirty catalog, 2,000 entries | 695,544 B pretty v1 | 321,532 B compact v2 | 53.8% smaller |
| Same-HEAD no-change refresh | 4.3 s full Git diff | 0.14 s watcher fast path | no segment/catalog change |
| Same-HEAD 100-path in-place refresh | 4.4 s full Git diff | 0.17 s event delta | one delta segment |

The append measurement includes base reader prewarm, Git status, classification of the new paths,
tokenization, segment publication, and CLI round trip. The unchanged 1,000 already-published paths
were not rebuilt in that prototype measurement. The final ready daemon used 817,072 KiB RSS and
sampled at 0.0% CPU after the query.

The compact-catalog migration was exercised with one additional already-dirty file. The new binary
loaded the previous v1 object catalog, published the delta, and wrote the v2 array catalog. That run
took 10.56 seconds including process-cold base prewarm; the ready daemon used 736,912 KiB RSS and
the first marker query took 0.10 seconds.

The final transaction smoke showed zero catalog writes for no-change refresh and one catalog write
for a 100-path delta. The delta completed in 0.13 seconds and its marker count was exactly 100 in
both flashgrep and rg.

The 2,000-file full rebuild was profiled by stage: document preparation was 9 ms for 23.6 MB and
segment publication was 1.77 s before parallel term collection. After the change, segment publication
was 0.58 s. A one-second process monitor during a second rebuild recorded peak RSS 2,794,624 KiB
(2.66 GiB) and peak CPU 151.5%; the index directory remained approximately 5.23 GiB. The remaining
roughly 4.3 s was worktree/Git reconciliation, which
is now avoided for same-HEAD watcher refreshes but remains the full-probe cost for watcher overflow
or disabled watchers. A 100-file event delta prepared 100 documents in 6 ms and published in under
1 ms.

## Correctness

The incremental overlay contained `FLASHGREP_OPT_2` in exactly 1,000 `.cc` files:

| Runner | Matching lines |
| --- | ---: |
| flashgrep indexed workspace view | 1,000 |
| rg worktree scan | 1,000 |

The earlier diverse Chromium corpus remained covered by `benchmarks/chromium_queries.tsv`. A rare
C++ symbol check after manifest-only recovery also matched exactly: both flashgrep and rg returned
1,644 lines for `ScopedAllowBlockingForTesting` under `*.{cc,h}`.

The complete 14-case query corpus was rerun after the event fast path. Every matching-line count was
exact against `rg`: rare symbols 1,644, common short word 895,521, path-scoped `#include` 416,998,
and the no-match case 0, with all intermediate regex, glob, case, and word cases also exact.

## Test Gates

- `cargo check -p flashgrep --all-targets`: passed.
- `cargo test -p flashgrep --lib`: 230 passed, 0 failed, 1 pre-existing ignored test.
- overlay-focused suite: 45 passed, 0 failed, 1 ignored.
- manifest-only published-overlay reopen test: passed.
- dirty-catalog ready-version invalidation test: passed.
- dirty-catalog compact v2 round-trip and v1 migration path: passed.
- no-change refresh artifact-preservation regression: passed.
- repeated tracked/untracked watcher permutations with catalog-generation CAS: passed.

## Remaining Work

Base construction still has independent costs in Git tree materialization, per-file metadata calls,
document preparation, and posting spill/merge I/O. Those should be instrumented before changing the
base builder. The remaining Phase 3 transaction-boundary matrix and a branch-aware large-delta
planner remain the next overlay-specific phases from the optimization plan.
