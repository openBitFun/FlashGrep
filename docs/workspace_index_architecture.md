# Workspace Index Architecture

## Goal

`codgrep` 当前 daemon/runtime 主线已经不是“base snapshot + query-time dirty repair”，也不是把 workspace 增量直接暴露给查询线程的可变写路径，而是：

1. `base snapshot`
   对应某个 Git `HEAD` 或 repo fingerprint 的稳定基线快照。
2. `dirty catalog`
   记录当前工作区相对 base 的 dirty / building / ready / failed 状态。
3. `published overlay snapshot`
   只表达已经构建完成并原子发布、对查询可见的 workspace 增量层。

这套架构的目标是把“工作区变化发现”和“查询可见性”彻底分离：

- 搜索读取 `base + overlay` 两层 snapshot。
- watcher / reconcile / refresh 只负责更新 dirty catalog，并驱动后台 overlay build。
- 只有 published overlay 才参与查询。

## External Contract

对产品和集成方，稳定外部 contract 仍然是 daemon 协议。

- daemon request / response / notification schema 是外部接口。
- `DaemonClient` / `ManagedDaemonClient` 是 Rust 侧推荐入口。
- `WorkspaceIndex` / `WorkspaceSnapshot` / `SearchEngine` 是库层 facade，主要服务于本地工具、测试和专家向集成。

## Current Runtime Model

### Base Snapshot

base snapshot 由 `WorkspaceBaseSnapshotIndex` 负责：

- snapshot key 由 `HEAD commit + config fingerprint` 或 repo fallback 指定。
- 数据落在 `index_engine` 的 base shard 路径，而不是旧 `src/index/*` generation layout。
- build / rebuild 直接产出新的 base snapshot shard。

### Dirty Catalog

dirty catalog 由 daemon runtime 负责维护：

- watcher / reconcile 从当前 worktree 归纳 modified / deleted / new paths。
- 对每个 path 记录 `dirty / building / ready / failed` 和 observed file version。
- catalog 只负责表达“哪些路径需要重建或已经发布”，不直接参与查询合并。

### Overlay Snapshot

overlay snapshot 也是 daemon runtime 负责维护，但它只承担已发布查询层职责：

- dirty catalog 驱动后台 build/rebuild overlay。
- overlay 先在后台构建，再一次性 publish。
- 查询时通过 `WorkspaceSearchView` 合并 `base snapshot + published overlay snapshot`。

`WorkspaceEventual` 模式下，daemon 可以复用当前 overlay 状态；
`WorkspaceStrict` / refresh 路径则要求当前 workspace 没有 pending dirty，且 overlay 已经发布到可读 snapshot。

### Library Facades

库层 facade 也已经采用 snapshot 语义：

- `WorkspaceIndex::search()` 走 `base + overlay` workspace view。
- `WorkspaceSnapshot` 固定一份 base snapshot + overlay snapshot 组合。
- `SearchEngine` 的 build / rebuild 直接委托给 base snapshot build。

仍然保留的 `IndexWorktreeDiff` / freshness probe 仅用于状态观察、诊断和调度，不再驱动旧 `IndexSearcher` 式查询修补。

## Effective View

任一路径 `path` 的有效视图遵循：

1. 如果 published overlay 中有 live document，使用 overlay 版本。
2. 如果 published overlay 中有 delete tombstone，该路径不存在。
3. 否则回退到 base snapshot。

查询候选集合：

```text
EffectiveCandidates =
    (BaseCandidates - OverlayShadowedPaths)
    UNION OverlayLiveCandidates
```

## Runtime Modules

当前实现建议关注下面这些模块：

```text
src/index_engine/workspace/base_snapshot.rs
src/index_engine/workspace/search_view.rs
src/daemon/repo/catalog.rs
src/daemon/repo/watcher.rs
src/workspace.rs
src/workspace/runtime.rs
src/search_engine.rs
src/git_snapshot.rs
src/worktree_diff.rs
```

职责：

- `base_snapshot.rs`
  构建、发布、打开 base snapshot shard。
- `search_view.rs`
  合并 base / overlay snapshot 并执行 workspace 查询。
- `daemon/repo/catalog.rs`
  计算 base 相对当前工作区的 dirty 集，并构造 rebuild / delta publish 计划。
- `daemon/repo/dirty_catalog.rs`
  维护持久化 dirty catalog 和最小状态机。
- `daemon/repo/watcher.rs`
  把 watcher / reconcile 结果写入 overlay runtime。
- `workspace.rs`
  对外暴露 workspace facade、snapshot view、freshness/status API。
- `workspace/runtime.rs`
  辅助 diff、overlay state 构造以及 workspace 级结果合并。
- `search_engine.rs`
  提供简化的 build / search facade。
- `git_snapshot.rs`
  提供 Git `HEAD`、dirty worktree、HEAD materialization 等辅助能力。
- `worktree_diff.rs`
  提供基于 snapshot catalog 的 diff 诊断类型和逻辑。

## Compatibility Boundary

当前主线搜索架构完全建立在 `index_engine` 的 snapshot / overlay 目录和 manifest 之上。

## Public Library API

```rust
pub struct WorkspaceIndexOptions {
    pub build_config: BuildConfig,
}

pub struct WorkspaceIndex;

pub struct WorkspaceSnapshot;

impl WorkspaceIndex {
    pub fn open(options: WorkspaceIndexOptions) -> Result<Self>;
    pub fn ensure_base_snapshot(&self) -> Result<BaseSnapshotInfo>;
    pub fn status(&self) -> Result<IndexStatus>;
    pub fn probe_freshness(&self) -> Result<WorkspaceFreshness>;
    pub fn probe_freshness_if_due(&self, ttl: Duration) -> Result<WorkspaceFreshness>;
    pub fn search(&self, query: &QueryConfig) -> Result<SearchResults>;
    pub fn snapshot(&self) -> Result<WorkspaceSnapshot>;
}

impl WorkspaceSnapshot {
    pub fn base_snapshot_key(&self) -> &str;
    pub fn search(&self, query: &QueryConfig) -> Result<SearchResults>;
}
```

## Current Limits

- daemon runtime 已经按 `base + dirty catalog + published overlay` 工作；库层 `WorkspaceIndex` 仍是偏本地工具语义的按需 workspace facade。
- dirty catalog 是轻量持久化状态；未发布 overlay 丢失依赖 startup rescan 恢复。
- query 执行层仍是第一版实现，reader 查询与 verify 还有继续优化空间。
