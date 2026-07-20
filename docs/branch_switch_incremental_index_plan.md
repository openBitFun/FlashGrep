# Branch Switch Incremental Index Plan

## Status

This document is an implementation plan for replacing branch-switch base snapshot rebuilds with
incremental manifest updates. It is intentionally specific to the daemon + MCP + distributed binary
runtime. The general workspace overlay contract remains documented in
[incremental_index_design.md](./incremental_index_design.md).

The target behavior is:

- opening an already-open repository is idempotent;
- changing Git `HEAD` never invalidates a readable base snapshot;
- search does not wait for reconciliation: it uses one bounded transition backend until a
  current-HEAD overlay is published, then returns to the indexed `base + overlay` view;
- advancing a base snapshot indexes only paths changed between two commits;
- switching back to a recently indexed commit reactivates a cached manifest instead of rebuilding;
- a full corpus build is reserved for the first build, an incompatible configuration/schema change,
  corruption recovery, or an explicit user request.

The old branch-switch path must be replaced, not retained as a compatibility or heuristic fallback.

## Problem Statement

The current runtime has two different incremental mechanisms, but neither prevents a large branch
switch from behaving like a full rebuild.

### MCP Reopens The Repository For Every Tool Call

`mcp/server.js` calls `open_repo` before status, build, glob, and search operations. When the daemon
already owns the runtime, `RepoManager::open_repo` calls
`RepoRuntime::refresh_base_snapshot_identity_state`. If the current Git `HEAD` differs from the base
snapshot metadata, that method:

1. changes the runtime phase to `missing_base_snapshot`;
2. drops the cached base reader;
3. clears the base path catalog;
4. clears the workspace overlay.

The MCP layer then observes a missing snapshot and starts `base_snapshot/build`. The published old
base was still a valid anchor for workspace search, but it is discarded before the search request is
sent.

### Base Rebuild Re-enumerates The Entire Commit

`WorkspaceBaseSnapshotIndex::rebuild_for_snapshot_identity_locked` obtains the complete tracked file
set using `git ls-files` or `git ls-tree -r`. It computes fingerprints for every base shard and builds
a new manifest. Unchanged shards can be hard-linked, but repository enumeration and shard
fingerprinting still scale with the complete repository.

### Reuse Is Too Coarse

Base files are assigned to 16 CRC32 path shards. One changed file invalidates its entire shard. With
256 paths distributed over 16 shards, the expected number of untouched shards is approximately:

```text
16 * (15 / 16) ^ 256 ~= 0.000001
```

The default `rebuild_dirty_threshold` is 256, so the threshold that starts background rebuild is also
large enough to make all 16 shards almost certainly dirty. A normal cross-branch change therefore
retokenizes the whole corpus.

### Previous HEADs Are Not Addressable Snapshots

The storage directory may still contain older segment directories, but only the current metadata's
shard map is considered reusable. There is no persistent `HEAD -> ManifestId` registry. Switching
back to a previously indexed branch cannot atomically reactivate its old manifest.

### Existing Tests Miss The MCP Failure Mode

The branch-switch daemon tests reuse one runtime and issue searches directly. They do not repeat
`open_repo` before each search, so they do not exercise the MCP lifecycle. The shard reuse test
modifies one of two populated shards and proves hard-link reuse, but does not cover a realistic
branch diff or assert the number of retokenized documents.

## Goals

1. Branch-switch work is proportional to changed paths and changed bytes.
2. Search never waits for branch promotion and never enters the missing-index path merely because
   `HEAD` changed.
3. `open_repo` is idempotent for an existing compatible runtime.
4. A daemon restart can reopen a base whose anchor commit differs from the current worktree `HEAD`.
5. Uncommitted worktree changes remain exclusively in the workspace overlay.
6. Base promotion is atomic and crash recoverable.
7. Recently used HEADs can be reactivated without indexing.
8. Merge and GC cannot turn an incremental promotion into an implicit full rewrite.
9. Existing compatible v3 base snapshots can enter the new model without retokenization.

## Non-Goals

- Avoiding work when every file really changed. Such a transition still has to index every changed
  corpus document.
- Preserving arbitrary historical commits indefinitely. The HEAD cache is bounded.
- Making the dirty catalog a query truth source. Queries continue to read published snapshots only.
- Retaining the 16-shard rebuild as a branch-switch fallback.
- Promoting uncommitted worktree content into a Git base snapshot.

## Required Invariants

### Query Availability

At every point in a branch transition, a query uses one valid published view or the explicit
transition backend. Once the overlay is aligned, the indexed view is:

```text
effective workspace =
    (published base - overlay shadowed paths)
    + published overlay live documents
```

Neither dirty catalog state nor an in-progress base candidate participates in query correctness.

The published indexed view may temporarily describe the previous HEAD while the new overlay is
being built. MCP searches must not return that stale branch view. Use one explicit transition state:

```text
base HEAD == observed HEAD, overlay aligned
  -> indexed_clean or indexed_workspace_view

base HEAD != observed HEAD, aligned overlay not published
  -> bounded transition backend against current worktree

base HEAD != observed HEAD, aligned overlay published
  -> indexed_workspace_view

base promotion running
  -> keep using the already-correct indexed_workspace_view
```

This is a clear external constraint rather than a general fallback chain. It reuses the existing
streaming result cap and ends as soon as the published overlay declares itself aligned. Query threads
still do not build index artifacts. Transition results are labeled with a distinct backend and do not
change the indexed query truth contract.

### Base Is An Anchor, Not A Validity Check

A base snapshot anchored at `H0` remains readable when the worktree moves to `H1`. The commit
mismatch means an overlay reconciliation is required; it does not mean the base is missing.

### Git Commit And Worktree Content Stay Separate

- Base delta documents come from Git blobs in the target commit.
- Worktree-only modifications and untracked files remain in the workspace overlay.
- A base candidate is identified by a concrete target commit captured at task start.

### Immutable Artifacts, Atomic Activation

Segments, delete segments, and manifest files are immutable. A candidate becomes active only through
an atomic current-manifest pointer update. Readers pinned to the previous manifest remain valid.

### No Query-Time Build

Search may schedule reconciliation, but it never enumerates Git diffs, tokenizes documents, writes
segments, or waits for promotion.

## Target Architecture

```text
                         Git HEAD H1
                              |
                              v
                    +-------------------+
                    | Git Diff Planner  |
                    | H0 -> H1 only     |
                    +---------+---------+
                              |
                    changed paths/blobs
                              |
                              v
+----------------+   +-------------------+   +-------------------+
| Base Manifest  |-->| Delta Publisher   |-->| Candidate H1      |
| active H0      |   | add/delete seqs   |   | immutable manifest|
+-------+--------+   +-------------------+   +---------+---------+
        |                                                |
        | search remains available                       | atomic activate
        v                                                v
+---------------------------+                 +---------------------------+
| Workspace Query View      |                 | Active Base Manifest H1   |
| base H0 + overlay         |                 | old segments + H1 deltas  |
+---------------------------+                 +-------------+-------------+
                                                              |
                                                              v
                                                   overlay = H1 -> worktree
```

The base index becomes an LSM-style manifest chain:

```text
initial foundation segments
  + delta segment(s) for added/modified documents
  + delete segment(s) for deleted/replaced documents
  + bounded delta compaction
```

The initial bulk builder can still create multiple foundation segments. Normal branch and commit
changes no longer call that builder.

## Runtime State Model

The daemon should distinguish these identities explicitly:

```rust
struct ActiveBaseGeneration {
    manifest_id: ManifestId,
    head_commit: String,
    config_fingerprint: String,
    repo_root: PathBuf,
    document_count: usize,
    delta_depth: u32,
    delta_bytes: u64,
}

struct WorkspaceRevisionState {
    base: ActiveBaseGeneration,
    observed_head: String,
    overlay_manifest_id: Option<ManifestId>,
    dirty_stats: DirtyStats,
}
```

`base.head_commit != observed_head` is a normal indexed state. It should be represented as
`ready_dirty`, not `missing_base_snapshot`.

Recommended task kinds:

- `build_base_snapshot`: first build or incompatible-state recovery;
- `advance_base_snapshot`: incremental commit-to-commit promotion;
- `refresh_workspace`: worktree overlay reconciliation;
- `compact_base_deltas`: bounded maintenance that does not rewrite foundation segments.

Automatic maintenance must not use `rebuild_base_snapshot` for HEAD changes.

## Manifest Identity

### Put Base Identity In The Manifest

The current base identity is split between `CURRENT_MANIFEST`, `BASE_SNAPSHOT_META.json`, and
`BASE_SNAPSHOT_KEY`. Updating those independently can publish mismatched content and identity.

Add an optional source identity to the generic manifest model:

```rust
struct Manifest {
    // existing fields
    manifest_id: ManifestId,
    generation: u64,
    committed_seq_no: SeqNo,
    active_segments: Vec<SegmentRef>,
    active_delete_segments: Vec<DeleteSegmentRef>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<ManifestSource>,
}

enum ManifestSource {
    GitBase {
        head_commit: String,
        config_fingerprint: String,
        repo_root: String,
        document_count: usize,
        parent_manifest_id: Option<ManifestId>,
        foundation_segment_ids: Vec<SegmentId>,
        delta_depth: u32,
        delta_bytes: u64,
    },
    WorkspaceOverlay {
        base_manifest_id: ManifestId,
        observed_head: String,
        probe_epoch: u64,
    },
}
```

The active base HEAD becomes part of the same immutable file that lists its segments. Overlay source
identity proves which base manifest and observed HEAD the published delta covers. Switching
`CURRENT_MANIFEST` atomically switches both content and identity.

`BASE_SNAPSHOT_META.json` and `BASE_SNAPSHOT_KEY` should leave the primary read path after migration.
They must not remain as a second identity source.

### Base HEAD Cache

Maintain a bounded, non-authoritative registry:

```json
{
  "version": 1,
  "entries": [
    {
      "head_commit": "<oid>",
      "manifest_id": "manifest-...",
      "config_fingerprint": "...",
      "last_used_unix_secs": 0
    }
  ]
}
```

Properties:

- default capacity: 4 HEADs;
- keyed by `head_commit + config_fingerprint + repo_root`;
- written atomically;
- loss or corruption only loses cache performance, not active-index correctness;
- every cached manifest is pinned for GC;
- eviction removes the pin and lets normal reachability GC reclaim artifacts.

### IDs Must Be Unique Across Divergent Histories

Generation-only IDs are insufficient after manifests can branch. Two candidates built from the same
parent have the same next generation even when they target different HEADs. Manifest, segment, and
delete-segment IDs must include a collision-resistant candidate identity, for example:

```text
manifest-base-<target-head-prefix>-<candidate-nonce>
seg-delta-<target-head-prefix>-<chunk>-<content-hash>
del-delta-<target-head-prefix>-<chunk>-<content-hash>
```

`generation` remains useful for ordering within one manifest lineage, but it must not be the unique
artifact key. Merge output IDs need the same property because cached manifests can be compacted on
different branches concurrently.

## Storage Layout

```text
.flashgrep-index-engine/
  base-snapshot/
    CURRENT_MANIFEST
    HEAD_MANIFESTS.json
    manifests/
      manifest-....json
    segments/
      seg-foundation-.../
      seg-delta-.../
    deletes/
      del-delta-....bin
  workspace-overlay/
    CURRENT_MANIFEST
    manifests/
    segments/
    deletes/
  dirty-catalog.json
```

Foundation, delta, and delete artifacts share the existing reader format. The distinction affects
promotion, merge, and GC policy, not query semantics.

## Open And Restart Semantics

### Existing Compatible Runtime

`RepoManager::open_repo` should:

1. normalize and validate the request;
2. find the compatible runtime;
3. return its current status without revalidating base HEAD equality;
4. if observed Git HEAD changed, mark one explicit head-reconciling state and schedule a workspace
   reconcile;
5. never clear the base reader or published overlay from this path.

`refresh_base_snapshot_identity_state` should be removed. HEAD equality is not a base-readability
condition in the new model.

### New Runtime Or Daemon Restart

Bootstrap should:

1. load the active base manifest;
2. validate repo root, config fingerprint, segment schema, and referenced artifacts;
3. open it even when its HEAD differs from the current worktree HEAD;
4. publish the base reader into the runtime;
5. recover or rebuild the workspace overlay relative to the base HEAD;
6. schedule an incremental base advance only after the overlay is query-visible.

Only an incompatible or unreadable active base enters `missing_base_snapshot`.

### Ensure Semantics

`ensure_repo` should mean "the repository has a searchable indexed base", not "the base manifest is
already aligned with the current HEAD". A compatible old base plus a published overlay satisfies the
ensure contract.

## Branch Transition Algorithm

Assume active base commit `H0`, observed target commit `H1`, and current worktree `W`.

### Step 1: Keep H0 Active

Do not clear or replace any published reader. Reconcile and publish overlay `H0 -> W`. Searches use
the bounded transition backend until this overlay's source identity confirms `observed_head = H1`.
After that publication, searches use `H0 base + overlay` throughout base promotion.

### Step 2: Try Cached Activation

Look up `H1` in `HEAD_MANIFESTS.json`.

If a compatible cached manifest exists:

1. validate referenced artifacts;
2. pre-open its reader;
3. atomically activate its manifest pointer;
4. update the runtime base generation;
5. reconcile overlay from `H1` to `W`;
6. mark the cache entry recently used.

No document tokenization occurs.

### Step 3: Plan A Commit Delta

If `H1` is not cached, compute only `H0 -> H1`:

```text
git diff-tree --raw -r -z --no-renames H0 H1 -- .
```

The raw diff provides status, old object ID, new object ID, and path. It avoids `git ls-tree -r` over
the complete target commit.

Normalize the records into:

```rust
struct BaseAdvancePlan {
    from_head: String,
    to_head: String,
    added: Vec<GitPathChange>,
    modified: Vec<GitPathChange>,
    deleted: Vec<GitPathChange>,
}
```

Apply corpus rules to the target object:

- hidden path filtering;
- maximum file size;
- binary detection;
- supported text decoding behavior;
- internal storage exclusion.

A modified path whose target object no longer belongs to the corpus is treated as a delete.

Renames remain delete + add. Rename detection is unnecessary for index correctness.

Use the current base path catalog to determine whether the old side was indexed. The planner should
produce operations using this matrix:

| Old side indexed | Target side indexable | Operation | Document count delta |
| --- | --- | --- | --- |
| no | no | none | 0 |
| no | yes | add | +1 |
| yes | no | delete | -1 |
| yes | yes, same object | none | 0 |
| yes | yes, changed object | delete + add | 0 |

Git status alone is not enough to update `document_count`, because hidden, oversized, binary, and
otherwise filtered files may exist in the Git tree without existing in the index.

### Step 4: Read Only Changed Git Blobs

Use one persistent `git cat-file --batch` process for target object IDs. Do not invoke one Git process
per path and do not read clean commit content from the worktree.

Build `IndexDocument`s only for target-side added and modified corpus files. Delta documents use
inline compressed source bytes. Existing foundation segments may continue using `GitHeadBlob`; an
unchanged path has the same object content at `H0` and `H1`.

### Step 5: Allocate Sequences

Sequence ordering must make modified documents replace old versions:

```text
delete sequence = current committed sequence + 1
add sequence    = current committed sequence + 2
```

For a modified path:

1. write a delete record at the delete sequence;
2. write the replacement document in a segment whose `max_seq_no` is the add sequence.

For a deleted path, only the delete record is written. For an added path, only the new document is
written.

### Step 6: Build A Candidate Manifest

The candidate references:

- all active segments from the parent manifest;
- all active delete segments from the parent manifest;
- new delta segments;
- new delete segments.

It records `H1`, its parent manifest, updated document count, delta depth, and delta bytes.

The candidate retains the parent's segment schema fingerprint. The delta publisher must not replace
it with the generic writer's default schema string. Segment format compatibility is validated before
writing any candidate artifacts.

Changed documents should be chunked by both document count and source bytes, for example:

- at most 4,096 documents per delta segment;
- at most 256 MiB uncompressed source bytes per delta segment.

These are tuning defaults, not correctness boundaries.

### Step 7: Validate Before Activation

Before making the candidate current:

1. open its reader;
2. verify all referenced artifacts;
3. verify the target commit still exists;
4. re-read current worktree HEAD;
5. check that the parent manifest is still the active base generation.

If HEAD moved again, retain the candidate in the bounded HEAD cache but do not activate it. If the
parent changed concurrently, discard the candidate reference and let GC reclaim unreferenced output.

### Step 8: Activate And Reconcile Overlay

Use compare-and-swap manifest publication against the parent manifest ID. After activation:

1. install the pre-opened base reader;
2. change runtime base HEAD to `H1`;
3. recompute dirty state as `H1 -> W`;
4. publish the new overlay;
5. update status and notifications.

The old overlay may remain temporarily visible while the new base reader is installed because it
already represents the same or newer worktree paths and shadows duplicates. It must not be cleared
until the replacement overlay is published. Its source identity does not match the new base manifest,
so MCP searches use the transition backend during this short rebase window rather than trusting an
unproven combination.

## Manifest-Only Append Writer

The existing `IndexEngine::open` calls `IndexWriter::restore_from_manifest`, which scans all durable
documents and reconstructs in-memory history. Reusing that path would make an incremental advance
perform full index I/O.

Add a dedicated append mode that restores only metadata:

```rust
fn restore_manifest_refs_only(&self, manifest: &Manifest) {
    state.generation = manifest.generation;
    state.committed_seq_no = manifest.committed_seq_no;
    state.last_flushed_seq_no = manifest.committed_seq_no;
    state.last_seq_no = manifest.committed_seq_no;
    state.active_segments = manifest.active_segments.clone();
    state.active_delete_segments = manifest.active_delete_segments.clone();
    state.pending_docs.clear();
    state.pending_delete_seq_by_key.clear();
    state.history.clear();
}
```

The base advance writer only needs new path history. Existing path visibility comes from immutable
manifest references and the reader's sequence/delete semantics.

Do not add a conditional full-history restore fallback to this path. If manifest-only append cannot
validate its preconditions, the advance task fails while the previous base + overlay remains active.

## Merge Policy

The current tiered policy selects all sufficiently old live segments when any delete segment exists.
For a base manifest, that can merge the complete foundation after every incremental update and
reintroduce the original problem.

Create a base-specific merge policy with these rules:

1. Interactive maintenance never selects foundation segments.
2. Small delta segments may merge with other delta segments.
3. Delete segments that still shadow foundation documents remain active.
4. Delete segments can be compacted by key while preserving the highest sequence per key.
5. Full foundation compaction is explicit or separately budgeted idle maintenance.
6. Cached HEAD manifests are immutable; compaction creates a new manifest for one HEAD and updates
   that cache entry only after successful publication.

The manifest needs enough information to identify foundation and delta segments. Prefer recording
the foundation segment IDs in `ManifestSource::GitBase` over adding behavior to path-name parsing.

Suggested automatic compaction triggers:

- more than 8 delta segments;
- more than 8 delete segments;
- delta bytes exceed 10% of active foundation bytes;
- never based solely on dirty file count.

Compaction must be cancellable and must not block search or manifest activation.

## Garbage Collection

GC reachability roots become:

1. current active manifest;
2. manifests pinned by live readers;
3. manifests in the bounded HEAD cache;
4. a small configured current-manifest history window.

Segments and delete segments are reclaimable only when unreachable from all roots. Candidate
manifests written by cancelled or superseded tasks are reclaimed normally.

The current `Reclaimer::plan_with_pins` already accepts pinned manifest IDs. Extend the caller to
union reader leases with HEAD cache pins. Do not create a separate segment-retention mechanism.

## Concurrency And Cancellation

### One Base Mutation At A Time

The existing cross-process base build lock should protect initial build, base advance activation,
cache activation, and base compaction. Candidate blob reads and segment construction may happen
outside the activation critical section, but parent-manifest CAS is mandatory.

### Coalesce HEAD Changes

Track the latest requested target HEAD. If `H1` promotion is running and HEAD moves to `H2`:

- allow the current task to finish immutable candidate artifacts;
- do not activate `H1` unless it is still current;
- cache `H1` if capacity allows;
- schedule only the latest `H2` target.

Do not queue one promotion task for every observed intermediate commit.

### Search And Activation

Search pins its base and overlay snapshots before querying. Activation swaps runtime references but
does not wait for active searches to drain. Reader leases keep old artifacts reachable until the last
query releases them.

### Cancellation

Cancellation before activation deletes or abandons only unpublished candidate artifacts. The active
base and overlay are unchanged. Cancellation after successful manifest CAS is treated as completed
activation followed by a separately cancellable overlay reconcile.

## Crash Consistency

| Crash point | Recovery behavior |
| --- | --- |
| Before segment completion | Delete unreferenced staging/artifacts. Active base is unchanged. |
| After segments, before candidate manifest | GC removes unreferenced artifacts. |
| After candidate manifest, before activation | Candidate may be restored into HEAD cache or reclaimed. |
| During current pointer update | Atomic pointer exposes either parent or candidate manifest. |
| After activation, before cache registry update | Active manifest remains authoritative; rebuild cache registry from retained manifests if desired. |
| After activation, before overlay reconcile | Existing overlay continues shadowing changed paths; startup/workspace reconcile publishes the minimal new overlay. |

No recovery case starts the old full branch rebuild. A failed advance keeps the previous indexed
view and reports an advance error.

## Configuration And Protocol Changes

Remove `rebuild_dirty_threshold` as the trigger for automatic full base rebuild. Replace it with
controls that describe actual incremental maintenance:

- `base_delta_max_segments` (default 8);
- `base_delta_max_delete_segments` (default 8);
- `base_delta_max_bytes_ratio` (default 0.10);
- `base_head_cache_entries` (default 4).

Add status fields:

- `base_head_commit`;
- `workspace_head_commit`;
- `overlay_head_commit`;
- `overlay_base_manifest_id`;
- `base_manifest_id`;
- `base_delta_depth`;
- `base_delta_bytes`;
- `base_advance_target_head`;
- `cached_head_count`;
- `base_compaction_recommended`.

Add task kinds:

- `advance_base_snapshot`;
- `compact_base_deltas`.

Keep protocol additions optional/additive for clients, but remove the old automatic rebuild behavior
from daemon execution. The MCP server should not implement its own branch policy; it only reports
daemon state and starts the initial build when no compatible base exists.

## Migration From Current Base Snapshots

Existing compatible v3 base snapshots already contain readable segments and a current manifest.
Migration should not tokenize documents:

1. read current manifest and base metadata;
2. validate repo root, config fingerprint, current marker, and referenced artifacts;
3. publish an enriched manifest containing `ManifestSource::GitBase` and the same segment refs;
4. atomically make it current;
5. seed the HEAD cache with the migrated manifest;
6. stop reading identity from the legacy metadata files.

If tokenizer, corpus configuration, or segment schema is incompatible, perform one initial full
build. This is a format/configuration boundary, not a branch-switch fallback.

After the migration release, new code should have one base identity path. Do not keep both legacy
metadata identity and manifest identity active indefinitely.

## Implementation Phases

### Phase 0: Reproduction And Observability

Add failing tests before changing runtime behavior.

Files:

- `tests/mcp_server.rs`
- `src/daemon/service/tests/branch_switch.rs`
- `src/index_engine/workspace/base_snapshot/tests.rs`

Required coverage:

- every MCP search repeats `open_repo` after branch checkout;
- search does not report `missing_base_snapshot` and does not wait for overlay publication;
- transition search reads current worktree contents rather than the previous branch snapshot;
- search returns to `indexed_workspace_view` once overlay source identity matches current HEAD;
- no `build_base_snapshot` or `rebuild_base_snapshot` task starts;
- progress records distinguish enumerated diff paths from tokenized documents;
- test hooks expose built/reused document counts without parsing logs.

### Phase 1: Stable Anchor Lifecycle

Files:

- `src/daemon/repo/mod.rs`
- `src/daemon/repo/base.rs`
- `src/daemon/repo/status.rs`
- `src/daemon/repo/maintenance.rs`
- `mcp/server.js`

Changes:

- make existing-runtime `open_repo` idempotent;
- bootstrap compatible stale-HEAD bases;
- remove `refresh_base_snapshot_identity_state` invalidation;
- stop scheduling automatic full base rebuild from dirty count;
- keep old base + overlay visible across branch changes and daemon restart;
- leave initial missing-index build behavior unchanged.

Exit criteria:

- branch switch never causes a missing phase when a compatible base exists;
- MCP search uses the bounded transition backend while the new-HEAD overlay is unpublished;
- MCP search returns `indexed_workspace_view` after overlay publication;
- daemon restart on another branch reconstructs overlay without rebuilding base.

### Phase 2: Manifest Identity And Append Mode

Files:

- `src/index_engine/manifest/model.rs`
- `src/index_engine/manifest/store.rs`
- `src/index_engine/writer/core.rs`
- `src/index_engine/workspace/base_snapshot.rs`
- `src/index_engine/workspace/base_snapshot/metadata.rs`

Changes:

- add manifest source identity;
- add candidate-manifest write and CAS activation APIs;
- add manifest-only writer restore;
- migrate existing base manifests without retokenization;
- make active manifest the single base identity source.

Exit criteria:

- opening an append writer does not open every segment or load every document;
- identity and segment references activate atomically;
- existing compatible indexes migrate without a base build task.

### Phase 3: Git Delta Planner And Publisher

Files:

- `src/git_snapshot.rs`
- new `src/index_engine/workspace/base_snapshot/delta.rs`
- `src/index_engine/workspace/base_snapshot/input.rs`
- `src/daemon/repo/tasks.rs`

Changes:

- parse commit-to-commit raw diff;
- batch-read changed Git objects;
- build delta and delete segments with correct sequence ordering;
- publish candidate manifests;
- add progress and cancellation;
- remove branch changes from `rebuild_with_options` callers.

Exit criteria:

- tokenized document count equals target-side changed corpus document count;
- no full `ls-tree -r` occurs during normal advance;
- deleted and modified paths correctly shadow all parent versions.

### Phase 4: Runtime Activation And Overlay Rebase

Files:

- `src/daemon/repo/base.rs`
- `src/daemon/repo/workspace.rs`
- `src/daemon/repo/catalog.rs`
- `src/daemon/repo/query.rs`
- `src/daemon/repo/tasks.rs`

Changes:

- keep queries on parent base + overlay while building;
- pre-open and CAS-activate target manifests;
- coalesce rapid HEAD changes;
- reconcile overlay against the activated HEAD;
- expose advance status and task notifications.

Exit criteria:

- active searches continue across activation;
- HEAD changing during build cannot activate a stale target;
- uncommitted files remain visible and are never stored in base deltas.

### Phase 5: HEAD Cache, Merge, And GC

Files:

- new `src/index_engine/workspace/base_snapshot/head_cache.rs`
- `src/index_engine/gc/reclaimer.rs`
- `src/index_engine/merge/policy.rs`
- `src/index_engine/merge/executor.rs`
- `src/index_engine/writer/core/admin.rs`

Changes:

- persist bounded HEAD manifest registry;
- pin cached manifests in GC;
- implement delta-only live-segment merge;
- implement delete-segment compaction that preserves foundation tombstones;
- make full foundation compaction explicit/budgeted.

Exit criteria:

- switching back to a cached HEAD performs zero tokenization;
- automatic maintenance cannot select all foundation segments;
- evicting a cached HEAD eventually reclaims only unreachable artifacts.

### Phase 6: Protocol, Documentation, And Packaging

Files:

- `src/daemon/protocol.rs`
- `src/daemon/convert.rs`
- `src/cli/args.rs`
- `mcp/server.js`
- `mcp/README.md`
- `mcp/CHANGELOG.md`
- `docs/daemon_protocol.md`
- `docs/incremental_index_design.md`

Changes:

- expose new task/status fields;
- remove obsolete rebuild threshold wiring;
- document branch transition semantics;
- ensure packaged binaries and MCP bridge use the same daemon policy.

## Test Plan

### Unit Tests

- raw Git diff parsing for add, modify, delete, type change, and rename-as-delete-add;
- corpus filtering against target Git objects;
- delete/add sequence ordering for replacements;
- manifest-only writer restore performs no segment document reads;
- candidate CAS rejects a stale parent manifest;
- HEAD cache LRU and atomic registry recovery;
- GC preserves active, reader-pinned, and cached manifests;
- delta merge never selects foundation segments;
- compacted delete segments retain maximum sequence per key.

### Daemon Integration Tests

- branch switch with fewer than, equal to, and greater than 256 changed files;
- branch switch while searches are active;
- branch switch with uncommitted modifications;
- branch switch followed by daemon restart;
- rapid `H0 -> H1 -> H2` while H1 advance is running;
- cancellation before activation;
- target commit disappearing after task start;
- config change still enters initial/full build path;
- corrupted candidate leaves parent view searchable.

### MCP Integration Tests

- `search_code` calls `open_repo` on every request without invalidating base;
- subdirectory MCP sessions share the same branch-aware runtime;
- multiple MCP processes coalesce one target HEAD advance;
- search uses the bounded transition backend only until the aligned overlay is published;
- search remains indexed during base promotion after overlay publication;
- no branch transition starts `base_snapshot/build`;
- switching back to cached HEAD starts no indexing task.

### Performance Tests

Synthetic repository:

- at least 100,000 tracked files;
- initial base at H0;
- branches changing 1, 256, 1,000, and 10,000 files;
- assert enumerated and tokenized counts scale with Git diff size;
- measure search latency during build and activation;
- assert resident memory does not scale with total durable document count when opening append mode.

Chromium repository:

- use detached temporary worktrees; never mutate the user's active branch;
- build at one commit and advance to another;
- record Git diff file count, tokenized file count, delta bytes, elapsed time, and peak RSS;
- switch back and verify manifest-cache activation with zero tokenization;
- compare against initial full build time only as a benchmark baseline, not as a fallback.

## Acceptance Criteria

The implementation is complete when all of the following hold:

1. Repeated MCP `open_repo` calls do not invalidate a compatible base after checkout.
2. Branch switch never enters missing-index state: it uses one bounded transition backend until the
   aligned overlay is published, then returns to the indexed view.
3. Normal base advance does not invoke full tracked-file enumeration.
4. Tokenized files are limited to added/modified target corpus paths.
5. Unchanged foundation segments are referenced directly without being read or rewritten.
6. Modified and deleted paths have correct visibility before, during, and after activation.
7. Daemon restart on a different branch uses old base + reconstructed overlay.
8. Cached HEAD activation performs zero document tokenization.
9. Automatic merge does not rewrite foundation segments.
10. GC preserves every active, pinned, and cached manifest and reclaims evicted unreachable data.
11. Initial build, incompatible configuration, corruption recovery, and explicit rebuild are the only
    full-build entry points.
12. Full Rust tests, MCP integration tests, strict Clippy, and Chromium performance validation pass.

## Metrics And Diagnostics

Expose task progress and optional structured logs for:

- `from_head` and `to_head`;
- diff paths by add/modify/delete;
- filtered-out paths;
- Git blobs read;
- documents tokenized;
- delta and delete segment count;
- delta bytes written;
- parent and candidate manifest IDs;
- cache hit/miss/eviction;
- activation CAS retries;
- overlay reconciliation duration;
- base reader open/prewarm duration;
- compaction input/output bytes;
- peak task RSS where available.

These metrics are required to prove that a branch advance is incremental. A task message that only
says "rebuilding base snapshot" is insufficient.

## Risks And Mitigations

### Delete Visibility Across Foundation And Delta Segments

Risk: consuming a delete segment during delta-only merge can resurrect a foundation document.

Mitigation: preserve tombstones until every segment containing an older version is part of the same
compaction, or emit an equivalent compacted delete segment with the maximum sequence.

### Base Reader Uses One Git HEAD For Git-Backed Sources

Risk: reused foundation segments read `GitHeadBlob` content using the active manifest HEAD.

Mitigation: only reuse a foundation document when its path/object content is unchanged. Changed paths
are shadowed by a higher-sequence delete/add pair. Delta segments store inline source bytes.

### Append Writer Accidentally Loads Full History

Risk: using normal `IndexEngine::open` scans all durable documents.

Mitigation: enforce a separate manifest-only append constructor and test segment stores that fail if
document reads occur during open.

### Merge Reintroduces Full Rebuild Cost

Risk: current delete-triggered merge selects the foundation.

Mitigation: use a dedicated base delta merge policy and make foundation compaction explicit.

### Rapid Branch Changes Publish A Stale Target

Risk: H1 completes after worktree has moved to H2.

Mitigation: validate current HEAD and parent manifest immediately before CAS activation. Cache but do
not activate stale targets.

### Cache Growth

Risk: manifests share large foundations but changed branch content can still accumulate.

Mitigation: bounded LRU entries, GC pins derived from the registry, and metrics for per-entry unique
reachable bytes.

## Explicit Removal List

The implementation should remove or disconnect these old execution paths from branch changes:

- HEAD mismatch changing a compatible runtime to `missing_base_snapshot`;
- `open_repo` clearing base reader/catalog/overlay on checkout;
- dirty-count maintenance scheduling `RebuildBaseSnapshot`;
- branch transitions calling `rebuild_with_options`;
- normal branch advance using `git ls-tree -r` for the complete target commit;
- 16-shard fingerprint reuse as the primary branch incremental mechanism;
- current delete-triggered merge selecting all foundation segments;
- legacy base metadata acting as a second active identity source.

The initial bulk builder remains for the explicit full-build boundaries listed above. It is not a
fallback for a failed or large branch advance.
