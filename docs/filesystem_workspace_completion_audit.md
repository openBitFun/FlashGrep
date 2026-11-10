# Filesystem workspace completion audit

Status: **complete — code, tests, final-binary matrices and artifacts verified**.
The requirements below retain the original scope of
[the architecture work record](filesystem_workspace_refactor.md).

Validation logs: `/Users/user/workspace/flashgrep-refactor-validation-20260909`.
Production experiments: `/Users/user/workspace/flashgrep-production-benchmark-20260910`.

| Requirement | Authoritative implementation and inspected evidence | Status |
|---|---|---|
| Plain directories and unborn Git: open/build/query/watch/refresh/reopen | `WorkspaceLocation`, optional HEAD and filesystem base source. Production lifecycle test and actual Node MCP lifecycle cover edit/add/rename/delete/restore/offline restart. Final library 302/302; MCP 6/6. | Verified |
| Git optional, including absence of the executable | `GitMode::{Auto,Filesystem}`, CLI/managed/MCP `--no-git`, cached optional membership. No-Git-program test covers Filesystem mode and Auto on a plain directory; subtree selection preserves explicit filesystem scope. | Verified by final library and integration suites |
| Shared eligibility and scoped discovery | `files::workspace_path_is_eligible`, shared scoped walker and `workspace_file_metadata`; Git modes carried through commit diff. Regression covers ordinary/unborn/Git/forced-filesystem symlinks, symlinked directories, broken links, restoration, and Git base initial/advance filtering of custom storage and internal paths. Ignore/binary/size/hidden/rename tests are included in final library suite. | Verified |
| Bounded notification transport and missed-event recovery | Independent count/byte limits for watcher transport and candidates, overflow/rescan/full-reconcile paths; separate periodic deadline avoids empty event wake scans and periodic starvation. Deterministic maintenance tests and watcher lifecycle sequences passed. | Verified |
| Incremental publication, deletion masks and restoration | Streamed overlay preparation and segment flushes, tombstones, per-path base retractions, v1/v2 delete decoding, base manifest identity checks and reader leases. Full engine suite is part of the passing 302-test run. | Verified |
| Finite, cooperative compaction and durable checkpoints | Segment deletion watermark prevents repeated merging of retained base masks. Delete selection respects source budget; production overlay uses 64 MiB merge input budget and one task per maintenance turn. Tests cover subsequent deletes, held readers, reopen, budgets and continuation. | Verified by final tests and large-final: all six lifecycles, including maintenance idle and RSS |
| Explicit freshness and exact progress | Published/Reconciled/Filesystem; consumed-event revisions are carried through publication, later events are not credited, failures do not declare successful publication. Runtime and MCP tests passed. Revisions are session-local and are not proof against missed notifications. | Verified |
| Branch/worktree/membership behavior | Bulk checkout, cached-head activation, rapid HEAD changes, queued target capture, concurrent queries and staged ignored membership/linked worktrees. Final audit added base/discovery serialization; all 302 library tests passed, including the earlier failing checkout test and a regression holding an in-flight discovery. | Verified |
| Recovery, failure propagation and concurrent readers | Manifest-load/vacuum coordination, process-local reader pins, failure injection and retry paths. Final daemon/managed/MCP restart suites passed. Historical recovery reclaimed a prior buggy run's approximately 50k artifacts; its failed first bulk-delete timing remains separately recorded. | Verified within the documented single-process GC scope |
| Complete steady/small-update speed comparison | Cooperative binary completed 36 distinct cases per tool, all converged; exact full path-set oracles and 3 trials per operation/size. Revalidated all 72 records. Original Chromium source remains outside fixture mutation paths. | Verified: final workspace binary 36/36 flashgrep cases plus unchanged tgrep 36/36 baseline |
| Large update/query/resource/restart comparison | Baseline 12/12 lifecycles; cooperative variant 6/6, all first restart queries exact. Reports retain build/open boundaries, source bytes, RPC output cost, RSS sampling and post-query compaction costs. Revalidated complete records. | Verified: large-final completed all six plain/Git lifecycles at 1k/10k/50k; session 13672 exited 0 |

## Current source verification

- `final-workspace-library.log`: 302 passed, 0 failed, 0 ignored (86.97 s).
- `final-workspace-integration.log`: e2e 34, managed 5, MCP 6, all passed.
- `final-workspace-clean-check.log`: all-target check passed without warnings.
- `final-workspace-clean-release.log`: release build passed without warnings.
- After the full test suite, the only code edit moved an unused production import
  into the test module. The all-target check and release build cover that edit.
- `git diff --check` passed. Final documentation links, binary/source hashes, all 72 dynamic records,
  six large lifecycles and fixture restoration were checked after completion.

## Preserved failures and measurement boundaries

Earlier library/integration failures and launch stalls remain in the chronological
work record; they are not described as passing invocations. The first no-follow
full suite had 299 passes and one real base-publication/discovery race failure;
the final 302-test run follows the serialization fix. A missing test-only import
caused a separate compile failure before the successful 53-test base rerun.

The original production merge loop repeatedly rewrote retained deletion masks,
exhausting file handles. Its interrupted run and slower cleanup/recovery timings
are retained. The bounded-only variant reduced RSS but delayed 50k publication to
158 s and was deliberately stopped; it is not a completed comparison. Cooperative
scheduling replaced it and completed the full variant experiment.

Historical tables describe their recorded binary hashes. The cooperative release
binary is archived under `pre-no-follow-binary`; its dynamic artifacts and index
are retained under `pre-final-dynamic`. Final results are in `large-final` and the root dynamic tables.
`final-matrix-complete.json` records successful completion and the unchanged release
SHA-256; `final-source-identity.json` records the 171 source/test/example file hashes.
The running source identity was rechecked after the matrices completed.

Source/operation budgets are not hard RSS limits. Single documents can exceed a
flush threshold, posting expansion and long-lived readers add memory, and Git
commit snapshots retain their own build/advance machinery. Result visibility and
background compaction idle are measured separately. tgrep's unchanged baseline
was measured in a separate run, not interleaved with the final flashgrep build.

## Final gate

Completed: final binary/source identities, every required matrix row, exact-path
checks, restart/restoration records, generated reports and source checks were
inspected. All 50,000 large fixture files match their backup byte-for-byte; the
1,000 controlled Chromium files match fixture HEAD and no controlled additions
remain. The original source is outside the scripts' mutation paths.

[Final results and all speed tables](filesystem_workspace_results.md) disclose
update-latency regressions, small-query regressions, measured memory improvement,
separate maintenance cost and baseline/version boundaries. Performance completion
means the required experiments are complete and reported, not that every metric
beats tgrep or an earlier flashgrep build.
