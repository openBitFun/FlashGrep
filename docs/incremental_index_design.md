# Incremental Index Design For Flashgrep

## Goal

`flashgrep` 的增量索引运行时有三块状态，但查询真相源只有两层只读 snapshot：

1. `base snapshot`
   只读、稳定；daemon runtime 中锚定某个 Git `HEAD + config fingerprint`。
2. `dirty catalog`
   记录工作区里哪些路径变了、当前处于 `dirty / building / ready_overlay / failed` 哪个状态。
3. `published overlay snapshot`
   只包含已经构建完成并原子发布、对查询可见的 workspace 增量层。

目标不是让查询线程参与构建，而是把“变化发现”“后台构建”“查询可见”拆成三条边界清晰的流程；查询线程始终只读 `base snapshot + published overlay snapshot`。

## Design Principles

### 1. Query Truth Is Base Snapshot + Published Overlay Snapshot

查询线程只读取：

- base snapshot
- 当前 published overlay snapshot
- published overlay snapshot 所引用的 manifest / segment / delete segment

查询不会读取 dirty catalog、中间 staging，或任何 runtime cache，也不会等待后台构建完成。

### 2. Dirty Detection Is Separate From Query Visibility

后台 workspace probe、定时 reconcile、手动 refresh 都只负责：

- 发现 dirty path
- 记录 observed file version
- 安排后台 overlay build

它们不直接改变查询结果。

### 3. Publish Must Be Atomic

overlay 必须先在 staging 或临时产物中构建完整，再通过 manifest pointer swap 一次性发布。

查询线程只能看到：

- 旧 manifest
- 或新 manifest

不能看到半写入状态。

### 4. Startup Recovery Relies On Rescan

进程重启后的恢复策略是：

- 清理未发布 staging
- 把 `building / failed` 重新视为 `dirty`
- 重扫当前工作区
- 重新排队并发布缺失的 overlay

系统不依赖单独的日志重放恢复。

### 5. Base And Overlay Have Different Roles

- base snapshot 负责稳定、低抖动、可复用的大索引基线
- overlay 负责工作区最新变化的快速可见层

base rebuild 是独立流程，不阻塞 overlay 的局部更新。

## High-Level Architecture

```text
                 +----------------------------+
                 | Search Request             |
                 +-------------+--------------+
                               |
                               v
                 +----------------------------+
                 | Workspace Search View      |
                 | base + published overlay   |
                 +------+------+--------------+
                        |      |
                        |      v
                        |   +----------------------+
                        |   | Overlay Manifest     |
                        |   | published shards     |
                        |   +----------+-----------+
                        |              |
                        v              v
             +----------------+   +------------------+
             | Base Snapshot  |   | Overlay Segments |
             | immutable      |   | + delete shards  |
             +----------------+   +------------------+

Filesystem / Git changes
          |
          v
+------------------+
| Dirty Catalog    |
| dirty/building/  |
| ready/failed     |
+--------+---------+
         |
         v
+------------------+
| Background Build |
| stage -> publish |
+------------------+
```

## Write Path

### 1. Mark Dirty

变化来源可以是：

- background workspace probe
- Git diff / repo reconcile
- 启动时全量 rescan
- 显式 refresh

动作统一成：

```text
mark_dirty(path, observed_version, kind)
```

这一步只更新 dirty catalog。

### 2. Build Overlay Artifact

后台 worker 取出 dirty path 后：

1. 读取文件当前内容
2. 重新生成该路径对应的文档或删除记录
3. 写入 overlay staging 产物
4. 形成可发布的 segment / delete segment 元数据

如果文件已经不存在，则产出 delete tombstone。

### 3. Publish Overlay

当 staging 构建完成：

1. 生成新的 overlay manifest
2. 原子切换 manifest pointer
3. 把相关 dirty entry 标为 `ready_overlay`

失败时把 entry 标为 `failed`，等待下次 rescan 或重试。

## Read Path

查询时的有效视图遵循：

1. 先查 base snapshot
2. 再查当前 published overlay
3. 如果 overlay 中某个 path 有 live document，覆盖 base
4. 如果 overlay 中某个 path 有 delete tombstone，从 base 中扣除

逻辑上等价于：

```text
EffectiveCandidates =
    (BaseCandidates - OverlayShadowedPaths)
    UNION OverlayLiveCandidates
```

dirty 但尚未 `ready_overlay` 的路径不会进入查询结果。

## Dirty Catalog

每个路径记录：

- `path`
- `kind`: `modified | deleted | new`
- `observed_version`
- `state`: `dirty | building | ready_overlay | failed`
- `last_attempt_at`

dirty catalog 的职责只有两个：

- 记住哪些路径需要重建
- 让后台 worker 可以恢复和重试

它不是查询层，也不是额外的可见性真相源。

## Overlay Manifest

published overlay manifest 至少要表达：

- 当前可见 segment 列表
- 当前可见 delete segment 列表
- 覆盖了哪些路径
- 这些路径对应的最新 sequence/version 边界

查询线程只读 manifest，不扫描 staging。

## Crash Recovery

### Staging 阶段崩溃

- 未发布 staging 可以直接清理
- 对应路径重新标 dirty

### Publish 前崩溃

- 旧 manifest 仍然可读
- 查询继续看到旧 overlay 视图
- 重启后 rescan 会补回遗漏更新

### Publish 后崩溃

- 新 manifest 已经生效
- 查询直接使用新 overlay

核心约束是：

- publish 原子
- startup 可重扫
- 未发布更新允许回退到上一个已发布 overlay

## Minimal State Machine

```text
Clean@Base
  -> Dirty
  -> Building
  -> ReadyOverlay
  -> Failed
```

状态跃迁：

- 文件变化: `Dirty`
- worker 开始构建: `Building`
- 发布成功: `ReadyOverlay`
- 构建失败: `Failed`
- base snapshot 重建并吸收这些变化后: 回到 `Clean@Base`

## Directory Layout

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

实现上允许使用临时 staging 目录，但它们不属于查询 contract。

## Operational Consequences

### 优点

- 查询延迟稳定，因为查询线程不参与构建
- 恢复逻辑简单，因为已发布 manifest 就是查询真相
- 适合 IDE / agent 场景，因为工作区变化可以快速进入 overlay

### 代价

- 未发布 overlay 更新在崩溃后可能丢失
- freshness 取决于后台 worker 的吞吐
- base snapshot 与 overlay 之间需要明确的合并边界

## Non-Negotiable Constraints

下面三条应该保持不变：

1. 查询线程绝不触发增量索引构建。
2. 只有 published overlay 才对查询可见。
3. 未发布更新允许丢失，重启后依赖 startup rescan 修复。

## Related Documents

- 当前运行时整体视图：[workspace_index_architecture.md](./workspace_index_architecture.md)
- 模块和目录蓝图：[index_engine_blueprint.md](./index_engine_blueprint.md)
