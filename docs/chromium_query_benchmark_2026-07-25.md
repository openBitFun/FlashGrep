# Chromium Query And Branch Benchmark

Validation date: 2026-07-25

Repository: `/Users/user/workspace/chromium-src`, measured in a detached worktree at commit
`606acc70f6320a025c719d7eee8a4583c9205252`. The main Chromium worktree was not modified.

Binary: release `flashgrep` at the current worktree. `rg` was ripgrep 15.1.0. Flashgrep and rg
queries used fixed-string `#include`, counted matching lines, and included CLI process startup. Each
reported query average uses five runs except the largest 68,044-file overlay, which uses three.

## Query Latency

The Chromium checkout contains 493,140 tracked files and 445,491 indexable documents.

| State | Flashgrep | rg | Overlay/sync detail |
| --- | ---: | ---: | --- |
| Clean H0 | 326 ms | 7,812 ms | Base already indexed |
| 1,000 uncommitted files | 226 ms | 8,738 ms | Overlay refresh 5.13 s |
| 10,000 uncommitted files | 266 ms | 8,498 ms | Overlay refresh 15.26 s |
| 50,000 uncommitted files | 210 ms | 8,572 ms | Overlay refresh 60.06 s |
| 68,044 uncommitted files | 250 ms | 8,227 ms | Checkout `.cc` file limit; refresh 93.98 s |
| Committed H1, 1,000 changed files | 332 ms | 7,764 ms | Base delta depth 1 |
| Committed H2, cumulative 10,000 changed files | 240 ms | 8,652 ms | Base delta depth 2 |

The rg command directly scans the current worktree on every run. Flashgrep queries use the base
snapshot plus the published workspace overlay or base delta.

## Branch Synchronization

The base build at H0 indexed 445,491 documents in 130.31 s in this run. The machine was also running
an unrelated MCP repository-index build, so this full-build time is a conservative measurement.

| Transition | Changed files | Refresh request wall time | Base delta bytes | Result |
| --- | ---: | ---: | ---: | --- |
| H0 -> H1 | 1,000 | 4.30 s | 13,357,702 | Ready clean, depth 1 |
| H1 -> H2 | 9,000 additional | 13.25 s request; task completed shortly after | 119,037,663 cumulative | Ready clean, depth 2 |
| H2 -> H0 | 10,000 reverted | 13.17 s request; cache activation completed | 0 | Exact initial Manifest, depth 0 |

The H2 -> H0 transition reported `cached_head_count=3`, restored
`manifest-0000000000000001`, and did not publish a new base candidate. The worktree reset itself took
10.46 s because Git had to restore 10,000 files; that is separate from index synchronization.

## Resource Usage

The daemon was sampled once per second with `scripts/monitor_process_and_dir.sh` throughout the full
build, overlay refreshes, branch advances, searches, and cache activation:

- peak daemon RSS: 6,024,480 KiB (approximately 5.75 GiB);
- peak daemon CPU: 522.3% on the multi-core host;
- peak index directory size: 6,774,652 KiB (approximately 6.46 GiB);
- final index directory size after returning to H0: 5,758,184 KiB (approximately 5.49 GiB).

CPU percentages are process percentages and can exceed 100% when multiple cores are active. The
resource peak includes the initial full build and the largest overlay ingest, not just steady-state
querying.

## Method Notes

- Overlay changes appended one marker comment to tracked `.cc` files; no Chromium code was compiled.
- Overlay sizes were cumulative: 1,000, 10,000, 50,000, then the checkout maximum of 68,044 files.
- Branch commits modified 1,000 files, then 9,000 additional files, using the same detached worktree.
- The main Chromium worktree remained on its original commit with its pre-existing `.gitignore`
  modification only.
