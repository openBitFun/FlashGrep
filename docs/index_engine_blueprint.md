# Index Engine Blueprint

## Goal

这份文档描述当前 `index_engine` 的工程拆分方式，围绕三件事展开：

1. base snapshot 如何构建和打开
2. overlay 如何发布和查询
3. daemon runtime 如何把 dirty catalog、overlay build、search view 串起来

它对应的运行时模型见 [incremental_index_design.md](./incremental_index_design.md)。

## Top-Level Modules

```text
src/index_engine/
  mod.rs

  types.rs
  error.rs
  config.rs

  writer/
    mod.rs
    writer.rs

  manifest/
    mod.rs
    manifest.rs
    snapshot.rs
    store.rs

  segment/
    mod.rs
    format.rs
    writer.rs
    reader.rs
    postings.rs
    term_dict.rs
    path_map.rs
    live_docs.rs

  delete/
    mod.rs
    format.rs
    writer.rs
    reader.rs
    resolver.rs

  merge/
    mod.rs
    planner.rs
    executor.rs
    policy.rs

  reader/
    mod.rs
    index_reader.rs
    query_session.rs
    state.rs
    visibility.rs

  query/
    mod.rs
    planner.rs
    execution.rs

  workspace/
    mod.rs
    base_snapshot.rs
    git_snapshot.rs
    overlay.rs
    search_view.rs

  gc/
    mod.rs
    reclaimer.rs
```

daemon 侧和 workspace runtime 侧还会再叠一层：

```text
src/daemon/repo/
  base.rs
  catalog.rs
  dirty_catalog.rs
  maintenance.rs
  overlay.rs
  query.rs
  status.rs
  tasks.rs
  workspace.rs
```

## Module Responsibilities

## `types.rs`

统一定义稳定领域类型：

- `RepoId`
- `ShardId`
- `DocKey`
- `SegmentId`
- `DeleteSegmentId`
- `SeqNo`
- `ManifestId`
- `CommitStamp`
- `RefreshStamp`

## `writer/`

负责单个 engine shard 的写入和发布。

- 接收 add / update / delete
- 维护 pending document/delete state
- 生成新的 segment / delete segment
- 写入 manifest 并原子 publish
- 暴露 writer/runtime stats

这里的 writer 不是单独的 durable log 层；它的 durable 可见性边界就是 manifest publish。

## `manifest/`

负责已发布真相：

- `manifest.rs`
  manifest 结构和 segment 引用
- `store.rs`
  manifest load/save/atomic switch
- `snapshot.rs`
  reader 持有的不可变 manifest handle

manifest 是 reader 看到的唯一已发布入口。

## `segment/`

负责 immutable data segment 的读写与格式：

- posting list
- term dictionary
- stored fields
- path map
- live docs

base snapshot 和 overlay 都复用同一套 segment 结构。

## `delete/`

负责 delete tombstone 的持久化与解析：

- 写 delete shard
- 读 delete shard
- 根据 snapshot 构造可见性遮罩

## `merge/`

负责 overlay / shard 的后台整理：

- 选择 merge 候选
- 生成新的合并产物
- 替换旧 segment / delete segment 引用

merge 是后台维护行为，不改变“只有 published manifest 可见”这个约束。

## `reader/`

负责 snapshot-scoped reader：

- 打开 manifest snapshot
- 构造 query session
- 应用 delete / replacement visibility
- 汇总 segment 查询结果

## `query/`

负责查询规划和执行：

- query token planning
- 执行倒排召回
- 交给上层做 regex verify 或结果合并

## `workspace/`

把 engine 和 workspace 语义连起来：

- `base_snapshot.rs`
  构建和打开 commit-addressed base snapshot
- `git_snapshot.rs`
  Git `HEAD` snapshot 辅助；repo fingerprint fallback 仅保留为 daemon 外低层能力
- `overlay.rs`
  workspace overlay shard、发布策略、运行时统计
- `search_view.rs`
  合并 base snapshot 和 published overlay

## `daemon/repo/*`

这是实际的 repo runtime 编排层：

- `catalog.rs`
  计算 dirty set 和 overlay 写计划
- `dirty_catalog.rs`
  持久化 `dirty/building/ready_overlay/failed`
- `overlay.rs`
  执行 rebuild/apply-delta/publish
- `query.rs`
  选择 `indexed_clean / indexed_workspace_view / fallback`
- `status.rs`
  生成 repo 对外状态
- `tasks.rs`
  build/rebuild/refresh 任务生命周期
- `workspace.rs`
  workspace probe / reconcile / refresh 编排

## Core Data Structures

### Manifest

```rust
pub struct Manifest {
    pub manifest_id: ManifestId,
    pub generation: u64,
    pub created_at_unix_ms: u64,
    pub committed_seq_no: SeqNo,
    pub refreshed_seq_no: SeqNo,
    pub writer_epoch: u64,
    pub schema_fingerprint: String,
    pub active_segments: Vec<SegmentRef>,
    pub active_delete_segments: Vec<DeleteSegmentRef>,
}
```

`active_segments` 和 `active_delete_segments` 定义了查询线程可见的完整 snapshot。

### Dirty Catalog Entry

```rust
pub struct DirtyCatalogEntry {
    pub kind: DirtyPathKind,
    pub observed_version: ObservedFileVersion,
    pub state: DirtyCatalogState,
    pub last_attempt_at: Option<SystemTime>,
}
```

### Workspace Overlay Runtime Stats

```rust
pub struct WorkspaceOverlayStatus {
    pub committed_seq_no: u64,
    pub last_seq_no: u64,
    pub uncommitted_ops: u64,
    pub pending_docs: usize,
    pub active_segments: usize,
    pub active_delete_segments: usize,
    pub merge_requested: bool,
    pub merge_running: bool,
    pub merge_attempts: u64,
    pub merge_completed: u64,
    pub merge_failed: u64,
    pub last_merge_error: Option<String>,
}
```

这些字段是 overlay runtime 的观测指标，不是查询线程的额外真相源。

## Storage Layout

单个 shard 的磁盘布局：

```text
<index-root>/
  CURRENT_MANIFEST
  manifests/
    manifest-0000000000000000.json
    manifest-0000000000000001.json
  segments/
    segment-.../
  deletes/
    delete-.../
```

workspace repo 的完整布局：

```text
.flashgrep-index-engine/
  base-snapshot/
    <snapshot-key>/
      ...
  workspace-overlay/
    CURRENT_MANIFEST
    manifests/
    segments/
    deletes/
  dirty-catalog.json
```

## Write Flow

### Base Snapshot Build

1. 解析 snapshot key
2. 扫描 repo corpus
3. 直接 bulk-build 新的 immutable shard
4. 发布为新的 base snapshot root

### Overlay Delta Publish

1. dirty catalog 找出待处理路径
2. 读取文件内容或删除状态
3. writer 形成新的 segment / delete segment
4. 生成新 manifest
5. 原子 publish
6. dirty catalog 标记为 `ready_overlay`

## Read Flow

1. 打开 base snapshot reader
2. 打开 published overlay snapshot reader
3. 应用 delete / replacement visibility
4. 合并候选和结果

对单个 path：

- overlay live doc 优先于 base
- overlay delete tombstone 会屏蔽 base
- 没有 overlay 记录时回退到 base

## Recovery Rules

- `dirty_catalog.json` 在启动时把 `building / failed` 重置成 `dirty`
- 未发布 staging 可以直接丢弃
- 已发布 manifest 保证查询可继续
- 当前工作区通过 rescan 补回未发布变化

## Public API Boundary

对外稳定 contract 是 daemon protocol。

`index_engine` 是内部引擎层，供：

- daemon runtime
- workspace/search facades
- benchmark/test harness

它不是推荐给宿主产品直接依赖的稳定 SDK 面。
