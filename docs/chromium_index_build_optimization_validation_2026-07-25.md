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
4. The explicit refresh path remains the branch-safe full rebuild path. The incremental planner was
   evaluated against Chromium but is not enabled there yet because branch-advance task ordering
   requires a separate base-manifest transition contract.

## Chromium Results

| Scenario | Before | After | Result |
| --- | ---: | ---: | --- |
| Published overlay restart before manifest-only recovery | 9.43 s | 4.90 s | Correct, one dirty path |
| No-change restart, 2,000-file overlay, before catalog version short-circuit | 10.75 s | 5.36 s | 3 active segments retained |
| Append 1,000 files to an existing 1,000-file overlay during downtime | full-overlay path historically grows with all dirty files | 11.77 s in the evaluated incremental prototype | 2,000 dirty files, one new segment |
| First marker query after optimized restart | - | 0.10 s | 1,000 matching lines |

The append measurement includes base reader prewarm, Git status, classification of the new paths,
tokenization, segment publication, and CLI round trip. The unchanged 1,000 already-published paths
were not rebuilt in that prototype measurement. The final ready daemon used 817,072 KiB RSS and
sampled at 0.0% CPU after the query.

## Correctness

The incremental overlay contained `FLASHGREP_OPT_2` in exactly 1,000 `.cc` files:

| Runner | Matching lines |
| --- | ---: |
| flashgrep indexed workspace view | 1,000 |
| rg worktree scan | 1,000 |

The earlier diverse Chromium corpus remained covered by `benchmarks/chromium_queries.tsv`. A rare
C++ symbol check after manifest-only recovery also matched exactly: both flashgrep and rg returned
1,644 lines for `ScopedAllowBlockingForTesting` under `*.{cc,h}`.

## Test Gates

- `cargo check -p flashgrep --all-targets`: passed.
- `cargo test -p flashgrep --lib`: 227 passed, 0 failed, 1 pre-existing ignored test.
- overlay-focused suite: 45 passed, 0 failed, 1 ignored.
- manifest-only published-overlay reopen test: passed.
- dirty-catalog ready-version invalidation test: passed.

## Remaining Work

Base construction still has independent costs in Git tree materialization, per-file metadata calls,
document preparation, and posting spill/merge I/O. Those should be instrumented before changing the
base builder. Overlay catalog persistence and a branch-aware large-delta planner remain the next
overlay-specific phases from the optimization plan.
