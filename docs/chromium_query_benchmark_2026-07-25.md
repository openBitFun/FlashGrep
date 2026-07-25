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

## Cold Query Checks

The reproducible query set is stored in `benchmarks/chromium_queries.tsv`. It covers rare and common
symbols, word boundaries, feature names, test and export macros, alternation, anchored regexes,
case folding, header strings, comment tags, no-match behavior, globs, and a path scope.

For each row, a new flashgrep daemon was started and `open_repo` completed before the one timed
search. Each rg measurement is also a new process. This is process-cold: daemon/query state is not
reused, but macOS page cache was not forcibly purged. Index construction and `open_repo` are not in
the timed interval. The table reports one clean-H0 sample and matching-line counts.

| Query kind | Flashgrep | rg | Speedup | Matching lines |
| --- | ---: | ---: | ---: | ---: |
| Rare C++ symbol | 0.22 s | 2.29 s | 10.4x | 1,644 |
| Whole-word `scoped_refptr` | 0.46 s | 6.66 s | 14.5x | 66,003 |
| Feature symbol | 0.17 s | 6.63 s | 39.0x | 7,571 |
| Browser-test macro | 0.18 s | 4.06 s | 22.6x | 33,678 |
| Four common core types, alternation | 4.89 s | 6.59 s | 1.3x | 370,020 |
| Export-macro regex | 0.22 s | 6.53 s | 29.7x | 42,007 |
| Browser-test name regex | 0.21 s | 4.16 s | 19.8x | 13,601 |
| Header string | 0.35 s | 6.59 s | 18.8x | 275,548 |
| Case-insensitive `chromium` | 0.46 s | 6.28 s | 13.7x | 132,427 |
| Common short word `if` | 0.84 s | 6.55 s | 7.8x | 895,521 |
| Anchored namespace regex | 0.19 s | 6.50 s | 34.2x | 5,129 |
| No-match symbol | 0.13 s | 8.83 s | 67.9x | 0 |
| `chrome/browser` path scope | 0.28 s | 0.58 s | 2.1x | 416,998 |
| Comment-tag regex | 0.59 s | 3.43 s | 5.8x | 51,510 |

The broad alternation is the weakest indexed case: its four common alternatives create a very
large candidate and result set, reducing the advantage to 1.3x. The path-scoped query is similarly
close at 2.1x because rg scans a much smaller subtree. A separate process-cold check on the
10,000-file overlay measured 0.36 s for flashgrep and 7.40 s for rg using fixed `#include`.

## Result Correctness

Results were compared on H0 using tracked-file references. The comparison covered fixed strings,
regular expressions, case-insensitive search, word boundaries, line-regexp, occurrence counts,
`*.cc` globs, and the `chrome/browser` path scope.

All 14 diverse cold-query cases produced exactly the same matching-line counts as rg when rg used
`--no-ignore` and excluded `.git`, matching flashgrep's Git-tracked base corpus. Without
`--no-ignore`, rg omits tracked files under `chrome/chrome_elf/dll_hash` because Chromium's root
`.gitignore` contains `**/*_hash`; that is a corpus mismatch rather than a search mismatch.

- `std::(unique_ptr|shared_ptr)` regex: exact, 43,893 files.
- `*.cc` fixed `#include`: exact, 67,927 files.
- `*.cc` case-insensitive `chromium`: exact, 63,100 files.
- `*.cc` word-regexp `class`: exact, 25,020 files.
- `chrome/browser` fixed `#include`: exact, 30,222 files.
- Whole-repository fixed `#include` and occurrence counts differ only on
  `third_party/liblouis/wasm/liblouis_wasm.data`, which is binary/non-text handling rather than a
  C++ source mismatch. Whole-repository case/word checks also differ only in non-code artifacts
  such as `.pdf`, `.mht`, `.json`, `.data`, and generated scripts; source-only checks are exact.

The previous performance table therefore remains valid for throughput, while these checks establish
that source-code search semantics and path scoping agree with the tracked-file reference.

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
