# Chromium Overlay Cold Query Validation

Validation date: 2026-07-27

Source repository: `/Users/user/workspace/chromium-src` at
`606acc70f6320a025c719d7eee8a4583c9205252`. Overlay mutations were made only in the detached
benchmark worktree `/tmp/flashgrep-chromium-opt`; the source worktree's existing `.gitignore`
change was not modified.

Binary: `target/release/flashgrep`. Query corpus:
`benchmarks/chromium_queries.tsv`. Every flashgrep query used a newly started daemon and one timed
search after `ensure`; every rg query used a new process. macOS page cache was not purged, so this is
process-cold rather than machine-cold. Flashgrep search time below excludes `ensure/open`, which is
reported separately. rg used `--no-ignore`, excluded `.git`, and used the same fixed/regex, glob,
and root semantics.

## Summary

| Overlay state | Cold open median | Flashgrep query median | rg median | Median speedup | Exact query cases |
| --- | ---: | ---: | ---: | ---: | ---: |
| O0, clean | 8.896 s | 0.498 s | 2.389 s | 4.8x | 14/14 |
| O1, 1,000 files | 4.802 s | 0.534 s | 2.624 s | 4.9x | 14/14 |
| O2, 10,000 files | 6.075 s | 0.592 s | 2.495 s | 4.2x | 14/14 |
| O3, approximately 50,000 files | 11.348 s | 0.625 s | 3.733 s | 6.0x | 14/14 |

The O0 open samples immediately followed restoration of about 50,000 files and therefore had a
colder filesystem cache than O1. The open medians must not be interpreted as a strictly monotonic
overlay scaling curve. O2 also had one 11.681 s open outlier. The stable conclusion is that query
latency itself remained broadly flat, while process-cold open remained expensive and O3 repeatedly
took about 11-12 seconds.

## Query Types

Times are seconds and include CLI startup and count-result serialization, but exclude daemon open.
The rg column is the O3 sample.

| Query type | O0 | O1 | O2 | O3 | rg at O3 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Rare C++ literal | 0.219 | 0.212 | 0.251 | 0.243 | 2.904 |
| Whole-word `scoped_refptr` | 0.478 | 0.523 | 0.578 | 0.619 | 3.501 |
| Four common types, alternation | 1.666 | 1.706 | 1.718 | 1.594 | 3.850 |
| Case-insensitive `chromium` | 2.474 | 2.484 | 2.485 | 2.151 | 5.016 |
| Common short word `if` | 2.357 | 2.532 | 2.459 | 2.143 | 4.942 |
| No-match symbol | 0.131 | 0.152 | 0.177 | 0.165 | 7.402 |
| `chrome/browser` path scope | 0.755 | 0.761 | 0.734 | 0.630 | 1.060 |
| Comment-tag regex | 0.721 | 0.689 | 0.908 | 1.772 | 3.988 |

The remaining cases were feature literals, browser-test macros, export-macro regexes,
browser-test-name regexes, header strings, and anchored namespace regexes. All 56 settled
state/query comparisons returned the same matching-line count as rg. The benchmark marker did not
overlap the 14 normal query patterns.

## Overlay Behavior

- O3 no-change explicit refresh completed in 0.08-0.13 seconds and wrote no new visible content.
- O1 publication reported 1,000 dirty paths and one active overlay segment.
- Cumulative O1 to O2 publication reported 10,000 dirty paths and two active segments.
- A settled O3 restart reopened one active segment, 50,000 dirty paths, no pending documents, and
  returned the 50,000-file benchmark marker exactly.

## Correctness Findings

### Resolved: Large watcher batches exceeded ARG_MAX

Growing O2 to O3 by adding 40,000 modified paths caused the initial refresh to fail with:

```text
initial workspace refresh failed: io error: Argument list too long (os error 7)
```

The repository temporarily remained at the prior 10,000-path published overlay with
`probe_healthy=false`. A later watcher retry recovered to 50,000 dirty paths and one published
segment. During that transition, one search for the new O3 marker returned zero while rg returned
50,000; after settlement both returned 50,000.

The immediate cause was `git_output_with_pathspec`, which appended every candidate path as one Git
command argument. A 40,000-path event exceeded macOS `ARG_MAX`.

The implementation now calculates a conservative chunk budget from the host's `ARG_MAX`, inherited
environment size, and fixed headroom, then runs every candidate probe through bounded chunks. Apple
Git 2.39 does not support `--pathspec-from-file` for `diff` or `ls-files`, so bounded argv chunks are
the single portable path across the Git versions supported here.

### Resolved: Refreshed overlay updates were lost across restart

At settled O3, an already-dirty file was edited twice with unique markers. Both markers were
query-visible in the running daemon. Runtime status showed:

```text
committed_seq_no: 50000
last_seq_no: 50004
uncommitted_ops: 4
pending_docs: 1
```

After a normal `shutdown_ack` and restart, both markers returned zero in flashgrep and one in rg.
Both ordinary refresh and `refresh --force` returned quickly but did not recover them.

The code path explains the result:

1. Below the default 256-operation threshold, `WorkspaceOverlayIndex::publish_after_write` calls
   `writer.refresh()` instead of committing.
2. The dirty catalog is nevertheless marked `ReadyOverlay` and persisted with the new file
   version.
3. `RepoRuntime::shutdown` joins workers but does not checkpoint the overlay writer.
4. On restart the durable manifest contains the old document, while the catalog version already
   matches the new worktree file, so startup reconciliation selects `NoChange`.

The fix makes overlay publication report whether it was only refreshed in memory or committed to a
durable manifest. A volatile refresh marks catalog entries ready only in memory. A commit persists
the matching ready catalog, and graceful shutdown checkpoints pending overlay operations before it
persists the catalog. After a crash, the older durable catalog version causes startup to apply the
missing delta.

Startup also now reuses the already-computed Git dirty set when its paths and kinds equal the
durable catalog. This removes a redundant second 50,000-path Git classification. A follow-up full
probe no longer rebuilds a readable overlay merely because the watcher has not yet marked itself
active.

## Fix Validation

Chromium was rerun with the release binary after the fixes:

- O10k to O50k completed without `E2BIG`; `probe_healthy` stayed true throughout. The marker
  converged to 50,000 in both flashgrep and `rg --no-ignore`. File mutation took 7.60 seconds and
  overlay convergence completed about 5.30 seconds after mutation ended while watcher publication
  overlapped the writes.
- A same-path dirty edit was visible in the running daemon with `uncommitted_ops=2` and
  `pending_docs=1`. Graceful shutdown/restart preserved the marker exactly.
- A second same-path edit followed by `SIGKILL` recovered through one delta. Cold recovery took
  12.08 seconds, retained the existing 50,000-document overlay, and returned the crash marker 1/1
  against rg. It did not rebuild the 50,000-document overlay.
- Post-fix process-cold O50 query samples were: rare literal 219 ms, broad alternation 1.565 s,
  no-match 165 ms, path-scoped literal 602 ms, and comment regex 790 ms. Corresponding rg times
  were 3.031 s, 3.746 s, 7.495 s, 1.076 s, and 3.830 s. All counts were exact and no query
  regression was observed against the pre-fix samples.
- Post-fix O50 cold opens were 11.98-12.04 seconds for the sampled normal-query daemons, consistent
  with the pre-fix approximately 11-12 second range.

Local verification after the fixes completed with `cargo clippy --all-targets -- -D warnings` and
the full `cargo test` matrix: 236 library tests passed with one pre-existing ignored test, plus all
34 e2e, 5 managed-daemon, and 5 MCP tests.

## Repository State

The main Chromium worktree remains at the original detached HEAD with only its pre-existing
`.gitignore` modification. The detached benchmark worktree is intentionally left at the
50,000-file overlay so the fixed paths remain reproducible; no benchmark daemon remains running.
