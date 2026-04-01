# Workspace Index Architecture

## Goal

`codgrep` 的终态需要从“单层只读索引 + 手动 stale 检查”演进到一个可直接作为外部库使用的两层检索架构：

1. `base snapshot`
   对应某个 Git commit 的只读稳定索引。
2. `worktree overlay`
   对应磁盘工作树相对 base 的增量索引。

查询时永远针对一个统一的 `EffectiveIndexView`：

`worktree overlay > base snapshot`

## Layering

### Base Snapshot

Base snapshot 继续复用当前工程已有的 generation 目录布局：

- `docs.bin`
- `doc_terms.bin`
- `lookup.bin`
- `postings.bin`

它是只读、可复用、按 `snapshot_key = HEAD commit + config fingerprint` 缓存的主索引层。
在非 Git 目录下，会退化为稳定的 repo-scoped fallback snapshot key。

### Worktree Overlay

Worktree overlay 和 base 尽量同构，但只覆盖 dirty subset：

- `docs.bin`
- `doc_terms.bin`
- `lookup.bin`
- `postings.bin`
- `tombstones.bin`

语义：

- `Modified(path)` 等价于 `shadow base[path] + add overlay[path]`
- `Deleted(path)` 只记录 tombstone
- `Added(path)` 只存在于 overlay
- `Renamed(old, new)` 拆成 `delete old + add new`

当前实现里，overlay 也已经有稳定身份：

- `overlay_key = base snapshot + dirty worktree fingerprint`
- 同一个 dirty state 直接命中已有 overlay cache
- 不同 dirty state 可以在同一个 base 下并存多份 overlay cache
- 新 overlay 构建会尝试复用旧 overlay cache 中 `size + mtime` 未变化文件的 `doc_terms`
- 连续 overlay 之间会尽量保持未变化 dirty 文件的文档顺序和 `doc_id` 稳定
- 新 overlay 构建会基于上一份 overlay 只重写受影响 token 的 postings，其余 posting bytes 直接复用

## Effective View

逻辑主键使用规范化路径，而不是 `doc_id == docs slot`。

对任一路径 `path`：

1. 如果 worktree overlay 有 `Upsert(path)`，使用 overlay 内容。
2. 如果 worktree overlay 有 `Delete(path)`，路径不存在。
3. 否则回退到 base snapshot。

查询时的候选集合：

```text
EffectiveCandidates =
    (BaseCandidates - ShadowedByOverlay)
    UNION OverlayCandidates
```

最终 verify 永远读取“当前生效内容”，而不是 base commit 中的旧内容。

## Runtime Modules

建议的终态模块划分：

```text
src/index/
  base/
    builder.rs
    reader.rs
    registry.rs

  overlay/
    detector.rs
    builder.rs
    reader.rs
    tombstone.rs

  runtime/
    effective_view.rs
    candidate_merge.rs
    resolver.rs
    searcher.rs
    verifier.rs

src/workspace.rs
```

职责：

- `base/*`
  构建和复用 commit-addressed base snapshot。
- `overlay/*`
  发现 worktree diff，构建并复用 overlay cache。
- `runtime/*`
  合并 base / overlay，并执行查询。
- `workspace.rs`
  对外暴露稳定的库 API。

## Public Library API

对外建议暴露两层 facade。

### WorkspaceIndex

```rust
pub struct WorkspaceIndexOptions {
    pub build_config: BuildConfig,
    pub auto_refresh_overlay: bool,
    pub overlay_cache_limit: Option<usize>,
}

pub struct WorkspaceIndex;

impl WorkspaceIndex {
    pub fn open(options: WorkspaceIndexOptions) -> Result<Self>;
    pub fn ensure_base_snapshot(&self) -> Result<BaseSnapshotInfo>;
    pub fn refresh_overlay(&self) -> Result<Option<OverlaySnapshotInfo>>;
    pub fn prune_overlay_cache(&self, keep_per_base: usize) -> Result<usize>;
    pub fn status(&self) -> Result<IndexStatus>;
    pub fn search(&self, query: &QueryConfig) -> Result<SearchResults>;
    pub fn search_json(&self, query: &QueryConfig) -> Result<JsonSearchReport>;
}

pub struct BaseSnapshotInfo {
    pub snapshot_key: String,
    pub snapshot_kind: BaseSnapshotKind,
    pub head_commit: Option<String>,
    pub config_fingerprint: Option<String>,
    pub index_path: PathBuf,
    pub repo_path: PathBuf,
    pub tokenizer: TokenizerMode,
    pub doc_count: usize,
}

pub struct OverlaySnapshotInfo {
    pub base_snapshot_key: String,
    pub overlay_key: String,
    pub modified_files: usize,
    pub deleted_files: usize,
    pub new_files: usize,
}
```

## Current Implementation Status

当前代码已经具备：

- commit-addressed base snapshot
- key-addressed overlay cache
- `doc_terms` 级缓存复用
- 跨 overlay generation 的 `doc_terms` 复用
- 受影响 token 级 postings 重写
- overlay cache lifecycle management
- workspace 查询路径上的 base/overlay 候选合并
- stale / worktree diff 检测

当前缺口：

- overlay 还没有演进到更细粒度的字节级 patch 和跨 base 复用
- overlay manifest / 目录层级内部仍可沿用 generation 命名，但对外库 API 已统一为 `snapshot_key` / `base_snapshot_key` / `overlay_key`

因此当前仓库的改造原则是：

1. 先把外部库 facade 和 runtime 视图抽象稳定下来。
2. 查询结果优先保证最新视图正确。
3. 在此基础上逐步把 worktree 从 scan fallback 替换为真正的 overlay postings。
