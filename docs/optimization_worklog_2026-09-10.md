# Chromium profiling and execution model refactor (completed)

Baseline: binary `76e4f2e1b26c0342af155d46d4dd05f8f80d1fb7f1116d51907ba32e0f64cf6f`.
Raw artifacts: `/Users/user/workspace/flashgrep-optimization-20260910`.
Actual corpus: `/Users/user/workspace/chromium-src`; checkout is not modified.

## Evidence and design

* Presence queries used the full line-count verifier. Make presence a first-class verification objective; stop on the first matching line. In files-with-matches mode the `matched_lines` statistic now represents verified first hits, not a complete line count; use count mode for full counts.
* File-backed count verification allocated/read the entire file before matching. Stream through the existing reusable grep searcher; presence can stop both scanning and reading early.
* Remove the duplicate multi-literal scan implementation (mmap then copy, independent error/limit behavior). In-memory counts retain the shared literal matcher; all sources use the shared verification plan.
* Full reconciliation compared worktree mtimes to immutable Git snapshot metadata (mtime zero). Introduce separate, disposable worktree observations. Reuse content hashes only for identical Unix device/inode/size/mtime/ctime; explicit watcher scopes force content verification. Re-read on other platforms.
* Observations are independent of base/HEAD identity: a stored content hash is compared to the current base. Root-bound JSON checkpoints accelerate restart; corrupt/missing data causes re-observation. Events only update memory; full scans prune and checkpoint observations. Cache checkpoint errors cannot falsely publish freshness.
* Extend durable dirty-file versions with the same identity, preserving reads of older catalog formats. This detects same-size rewrites with restored mtime even for already-dirty files.

## Validation status

Presence-only first change: existing 302 library tests passed. Broader refactor verification and measurements are ongoing. Profiling runs are diagnostic and may overlap compilation/tests; final speed comparisons must run without them.

Remaining assessment: per-query correctness/counts, real-tree restart/reconcile/cache cost and RSS, change storms/recovery, indexing/update throughput; follow measured regressions before concluding.

## Further structural changes

Presence alone did not remove per-file open costs on the actual tree. Git base documents therefore now embed immutable content just like filesystem base documents, with a new cache fingerprint (`index-engine-postings-v4-inline-snapshots`). Existing incompatible base caches must rebuild; historical Git-backed segment decoding remains supported by the low-level reader.

Large inline payloads use the existing Zstd format (level 3, minimum 4 KiB, only when smaller). Encoding now returns the written size so builders and overlay writers compress once. Base queries decode one candidate at a time from mapped segment storage without retaining all decoded documents. The final overlay implementation admits decoded query payloads under one shared 512 MiB budget; payloads retire with their segment reader.

Discovery uses a separate shared pool of at most four workers, leaving the query pool independent. Initial reads reserve known file size to avoid repeated small probe reads. Observation updates are accumulated and applied only after successful classification; visibility/publication fences remain unchanged.

Git base inputs now read enumerated object IDs, avoiding both path lookup per blob and worktree/commit byte ambiguity (CRLF and clean filters). A regression checks that a clean CRLF checkout embeds LF commit content, while dirty files still embed the committed version. The previous status-based source-selection path is removed.

The first full suite after object-backed inputs exposed a timing assumption in `branch_switch_advances_base_without_automatic_full_rebuild`: advancement had already completed when refresh returned, so the assertion demanding `ReadyDirty` failed. The test now accepts either valid return phase and verifies the eventual manifest, no full rebuild, and exact added/modified/deleted query results. The failure log and isolated passing rerun are retained; a full rerun is required.

## Frozen implementation validation

Release SHA-256: `f145358d3d2354857a18a42f1774b051eb7e08d0cd630a50d87965ad7721e8db`. The recorded source identity matches the tested tree. `validated-tests.log`: 309 library, 34 end-to-end, 5 managed-daemon and 6 MCP tests passed. `validated-check.log`: all-target check passed without warnings.

First actual Chromium run: 140 exact query/count samples; fresh ensure 271.71 s, followed by 2.53 s readiness observation; index 3.262 GiB including the 77 MiB observation checkpoint. The desktop's available-memory estimate triggered conservative build behavior; this cold build is not a controlled before/after build-speed comparison. Subsequent paired query and restart results remain to be reported.


## Bounded overlay query cache

The uncached compressed-content variant reduced memory but regressed overlay queries. The final implementation shares a 512 MiB decoded-payload admission budget across overlay readers. Base readers retain no query payload cache. Concurrent admission cannot exceed this budget; losing a cache insertion refunds its reservation, and final reader destruction releases the retained payload budget. Generic merge buffers, metadata and outstanding query-owned content are outside this budget, so it is not an RSS limit or an LRU guarantee.

Final release SHA-256: `7dd76b629083f7b85535328f90e5c54afae81ccd9d1da649a9294d84914620c7`. `hot-cache-tests.log`: 313 library, 34 end-to-end, 5 managed-daemon, 6 MCP tests passed. `hot-cache-check.log`: all-target check passed. `hot-cache-identity.json` records source hashes. The final actual-tree run validated all 140 path/count results, with geometric mean speedups of 2.42× for files and 1.50× for counts against the paired old version. This does not imply that every query improved.

The completed uncached large matrix is retained under `large/`; the final bounded-cache matrix completed all six lifecycles under `large-hot-cache/`. Final conclusions and all query timings belong in `docs/benchmarks/chromium_optimization_2026-09-10.md`, including build, RSS and background-maintenance tradeoffs.

Final large results: 50k update medians 44.84 s (plain) / 39.17 s (Git); whole-observation peaks 1465.55 / 1534.89 MiB. All-match overlay query medians 90.20 / 84.08 ms, still slower than the old 75.16 / 74.22 ms. All restart first queries and restoration checks passed. Full tables and limitations are in the final benchmark report.
