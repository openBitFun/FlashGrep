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

### Large Overlay Runs

These runs use the same detached Chromium checkout and cumulative marker list. The 10k run added
10,000 files to an existing 1,998-file overlay during daemon downtime. The 50k run added the next
38,000 files and intentionally included two restored paths, forcing the schema-required full
rebuild. Process-cold here means a new daemon process; the macOS page cache was not purged, so the
two open times are not a strict A/B pair.

| State | Dirty files | `open_repo` | Prepare stage | Publish stage | Peak CPU | Peak RSS | Catalog | First query |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Cumulative 10k delta | 11,998 | 41.30 s | 77 ms / 118 MB | delta 2.70 s | 628.1% | 1,685,344 KiB | 2,021,277 B | 0.23 s |
| Cumulative 50k rebuild | 49,998 | 12.92 s | 1.06 s / 608 MB | rebuild 12.79 s | 936.8% | 3,695,472 KiB | 8,597,916 B | 0.55 s |

The 10k marker matched exactly 10,000 lines in flashgrep and `rg --no-ignore`. The 50k marker was
present in exactly 38,000 files in both runners. All 14 diverse query cases remained exact at the
50k state; the indexed result set included the tracked `chrome/chrome_elf/dll_hash` files, matching
the explicit `--no-ignore` reference corpus.

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

## Earlier Test Gate

The following gate was captured before the later base-builder slices:

- `cargo check -p flashgrep --all-targets`: passed.
- `cargo test -p flashgrep --lib`: 230 passed, 0 failed, 1 pre-existing ignored test.
- overlay-focused suite: 45 passed, 0 failed, 1 ignored.
- manifest-only published-overlay reopen test: passed.
- dirty-catalog ready-version invalidation test: passed.
- dirty-catalog compact v2 round-trip and v1 migration path: passed.
- no-change refresh artifact-preservation regression: passed.
- repeated tracked/untracked watcher permutations with catalog-generation CAS: passed.

## Base Builder Results

The base builder now reports input enumeration and per-shard prepare/append/finish metrics. An
instrumented prior-path empty-storage H0 build indexed 445,491 documents in 84.38 s. Per-shard
preparation took 21-32 s, append took 2-4 s, and posting finish took 4-10 s while shards ran in
parallel. Removing a duplicate binary-content probe produced a 72.81 s run. Restricting shard
workers to four regressed to 83.22 s, so the default available-parallelism setting was retained.

Clean HEAD builds now use the same Git-object input model as dirty HEAD builds. Regular base files
are referenced as `GitHeadBlob`; symlinks keep the worktree-following behavior, preserving the prior
445,491-document corpus. A representative run completed in 76.26 s and the index shrank from
5,253,040 KiB to 2,522,784 KiB (52.0%). The 76.26 s wall sample was captured before the final
symlink semantic correction; the final correction run overlapped another daemon and is excluded
from timing comparison. All 14 query cases remained exact against `rg --no-ignore`,
including 895,521 common-word lines and 416,998 path-scoped include lines. The macOS page cache was
not purged, so the wall-time samples show direction and stage attribution rather than a strict cold
cache comparison.

An adjacent spill-run A/B fixed `FLASHGREP_BASE_SNAPSHOT_LOW_MEMORY=0` so current system pressure
could not change shard scheduling. A 2,097,152-record run completed in 70.83 s versus 80.12 s at
the previous 524,288-record default, an 11.6% improvement. The normal default is now 2M records;
the 64k low-memory default is unchanged. Parallel bucket sorting was rejected after an 84.48 s
sample showed no stable gain over the existing shard-level parallelism. Both experimental builds
indexed 445,491 documents, and the 14-query corpus remained exact.

An explicit same-H0 rebuild reused every prior stable shard: it completed in 13.11 s with no
`stage=build_segment` event and retained 445,491 documents. The remaining time was 8.82 s of Git
status/tree/classification plus 3.17 s of clean overlay probing. The subsequent shard matrix
tested 8/10/16 shard assignments; the production assignment remains 16 because shard count must
not be selected from one host's worker topology.

The shard-count matrix used the same Chromium H0 and `FLASHGREP_BASE_SNAPSHOT_LOW_MEMORY=0`:

| Shards | H0 build | Documents | Result |
| ---: | ---: | ---: | --- |
| 8 | 68.39 s | 445,491 | exact |
| 10 | 63.84 s | 445,491 | fastest on this host only |
| 16 | 70.83 s | 445,491 | portable default |

Ten shards matches this host's measured worker count, but that is not a portable optimization.
The fixed 16-shard assignment remains so indexes can be reused across machines; runtime worker
scheduling and memory controls are the portable tuning points.

Raising the in-memory posting threshold from 128 MiB to 192 MiB was rejected. With ten shards the
same H0 build regressed from 63.84 s to 71.32 s, and per-shard finish times rose to 9-11 s. The
existing 128 MiB threshold remains the measured performance point.

A later input-source A/B tested removing regular-file worktree metadata probes and reading all
regular HEAD content through persistent `git cat-file --batch` processes. With 16 shards and
`FLASHGREP_BASE_SNAPSHOT_LOW_MEMORY=0`, the existing worktree-read path completed Chromium H0 in
71.75 s; the Git-batch path took 87.06 s, a 21.3% regression. Both indexed 444,146 documents, and
storage was effectively unchanged (2,674,324 KiB versus 2,676,004 KiB). The experiment was rejected
and removed: object IDs remain fingerprints, but clean source bytes stay on the parallel filesystem
path because Git-pack reads are not a generally faster source across storage layouts or machines.

The accepted metadata-elision follow-up retained the parallel filesystem source for clean regular
files, but stopped calling `fs::metadata` for each path after Git status had proved it clean. Dirty
regular paths still use HEAD blobs and symlinks still use worktree metadata. On the same Chromium H0,
wall time fell from 71.75 s to 64.75 s (9.8%), with 444,146 documents and 2,674,956 KiB of index;
all 14 query counts stayed exact. A same-H0 explicit rebuild completed in 9.18 s, reused all 16
shards, and emitted no `stage=build_segment` event; the input enumeration stage was 4.08 s.

A separate dirty-worktree A/B isolated the remaining process-per-file Git blob cost. The test
modified 10,000 tracked `.cc` files while constructing the H0 base from HEAD. On the same
clean-metadata revision, the old source reader took 115.99 s and the shard-local persistent
`git cat-file --batch` reader took 69.29 s, a 40.3% reduction; both produced 444,146 documents.
Storage changed from 2,857,204 KiB to 2,858,476 KiB (+0.04%). The batch process is now created only
for shards that contain dirty `GitHeadBlob` inputs, so the accepted 64.75 s clean-worktree path is
unchanged. After opening the resulting 10,000-file overlay, all 14 recorded query counts matched
`rg --no-ignore`, as did a dedicated dirty marker query at 10,000 lines. The superseded
process-per-file reader was removed rather than retained as a compatibility branch.

Preparation chunks of 2,048 documents were tested and rejected. Although two alternating managed
CLI runs averaged 63.07 s at 2,048 versus 71.52 s at the 1,024 default, those runs did not sample
the detached daemon and were too variable to justify a default change. A fixed-daemon resource
rerun measured 63 s at 2,048 versus 64 s at 1,024, while peak RSS increased from 3,421,168 KiB to
3,924,128 KiB (+14.7%). Peak sampled CPU was nearly identical (822.2% versus 816.5%). The 1.6%
latency difference is within noise, so the 1,024 normal and 128 low-memory defaults remain unchanged.

## Branch Delta Results

The detached Chromium branch matrix used cumulative 1,000-file commits. H1 (1,000 files) and H2
(10,000 cumulative files) advanced the base without tokenizing unchanged documents; H2 reached
`base_delta_depth=2` and `base_delta_bytes=120,735,023`. The H3/H4 measurements showed that the
old post-transition overlay path still spent about 3.6-3.8 s in a clean full probe after the
delta was published.

The accepted path now reclassifies only the prior dirty-catalog paths against the new base catalog.
On Chromium H8 (1,000 changed files, cumulative depth 3), base delta construction took 795 ms
(`diff=60 ms`, old blob reads `100 ms`, new blob reads `170 ms`, writes `401 ms`) and the final
catalog-based overlay rebuild took 366 ms for 1,000 candidates, with zero remaining dirty paths.
The same query marker returned exactly 1,000 matching lines from flashgrep and `rg`. This removes
roughly 90% of the measured post-transition scan time without changing shard assignment or worker
defaults; it is therefore a path-count-based optimization rather than a machine-topology tuning.

Multi-request Git blob batching was then tested on a fresh pair of cloned H0 indexes advancing
directly to H2 (10,000 modified files). The existing persistent reader measured 537 ms for old
blobs, 486 ms for new blobs, and 3,895 ms for the complete base delta. Grouping up to 256 requests
measured 563 ms, 455 ms, and 3,907 ms respectively. Both wrote 120,735,023 delta bytes and returned
the same 10,000 marker lines as `rg --no-ignore`. With no end-to-end gain, the grouped implementation
was removed; segment writing at roughly 2.76 s, not Git request flushing, is the dominant stage.

The accepted replacement uses the already-preloaded active snapshot path catalog to determine
whether modified/deleted old-side paths were indexed. It therefore reads only target-side blobs and
shares the 444k-entry daemon catalog through `Arc` instead of deep-cloning it. On the same H0 -> H2
transition, old-side classification fell from 537 ms to 1 ms, Git blob reads fell from 20,000 to
10,000, and complete base-delta time fell from 3,895 ms to 3,252 ms (16.5%). The output remained
120,735,023 delta bytes. All 14 diverse H2 queries matched `rg --no-ignore` exactly, as did the
combined H1/H2 marker at 10,000 lines.

Two attempts to replace the normal delta segment writer were rejected. On H2 -> H12, reusing the
foundation bulk writer reduced the new segment from 182,432 KiB to 66,436 KiB, but write time rose
from 2,737 ms to 3,602 ms and complete base-delta time rose from 3,364 ms to 4,223 ms (+25.5%). The
bulk implementation was removed.

Keeping the normal posting writer while marking delta documents as `GitHeadBlob` reduced the
segment further to 64,676 KiB, but did not improve latency: H2 -> H12 took 3,449 ms versus 3,364 ms,
and H12 -> H11 took 3,406 ms versus 3,381 ms. The latter transition also exposed a contract failure:
background compaction opens a generic segment store without the active manifest HEAD and therefore
could not resolve the Git-backed delta source. The daemon remained searchable but reported the
compaction error. The source-kind experiment was removed rather than expanding compaction and
recovery behavior for a storage reduction with no measured build-speed benefit.

Finally, a streaming rewrite of the generic document writer was tested on the same H0 -> H2
Chromium transition. It used separate record/source file cursors to avoid constructing the complete
`docs.bin` buffer in memory, but write time changed only from 2,756 ms to 2,743 ms and complete
base-delta time from 3,399 ms to 3,390 ms. The sub-percent difference is noise, so the prototype
was removed. Aggregate memory residency was not changed by default; streaming remains an open
Phase 4 memory investigation rather than an accepted speed optimization.

## Background Compaction Results

The Chromium H9 -> H10 run used a one-delta test threshold to make background maintenance visible.
The first implementation exposed a real correctness failure: repeated compaction candidate IDs
embedded the complete parent ID, eventually exceeding the filesystem filename limit. Candidate IDs
are now bounded by generation plus a monotonic nonce.

After restarting the daemon on the same H10 storage, startup detected the over-limit compatible
base and scheduled compaction automatically. It merged 29.66 MB of live delta input in 756 ms,
left `ReadyClean` searchable, reduced `base_delta_depth` from 2 to 1, and cleared
`base_compaction_recommended`. H10 marker results remained exact at 1,000 lines.

The larger H12 transition changed 10,000 files. Base delta construction took 4.01 s and the
catalog-based overlay finish took 968 ms. During the resulting 2.84 s background compaction window,
five of twenty process-cold search requests observed `active_task_id=auto:compact:*` while the repo
phase stayed `ready_clean`; every request returned exactly 10,000 H12 marker lines. Final flashgrep
and `rg` counts were both 10,000, and the manifest ended at depth 1 with no recommendation pending.
Compaction logs now report merge input/output bytes, segment counts, and elapsed time.

## Open Work

The maintenance contract is explicit: `ensure` reuses a compatible base, HEAD changes use base
deltas, and only the user-requested rebuild reconstructs a foundation. Delta compaction is
background work that keeps the active manifest searchable while candidate merges are written and
atomically activated. Compaction now reports merge input/output bytes and elapsed time, and opening
a repository with an already-over-limit compatible base schedules the same background task.

The full Rust gate before the base slice was 230 passed, 0 failed, 1 ignored. A post-change full
run had six watcher timing/recovery failures under concurrent daemon load; the base-focused tests,
Chromium query corpus, and individual rebuild test passed. These failures remain a watcher-race
follow-up rather than being claimed as a clean gate. The latest serial rerun after catalog-based
old-side branch classification completed with 225 passed, 6 failed, and 1 ignored; an earlier
rerun completed 226/5/1. The remaining failures are the same watcher event/rebuild timing cases,
while base input, branch transition, lifecycle, and background compaction coverage passed.

The authoritative execution order is maintained in
`docs/index_build_and_overlay_recovery_optimization_plan.md`. The remaining work is:

1. fix the five reproducible watcher/overlay-rebuild timing failures;
2. complete the Phase 3 publication-boundary kill/restart matrix;
3. complete the Chromium H0 -> H1 -> H2 -> H0 cache-switch and interruption matrix;
4. implement the overlay streaming writer and aggregate memory budget only if profiling still
   justifies them;
5. rerun the complete automated gate and publish the final report.
