# Flashgrep Daemon Protocol v1

## 目标

`flashgrep` 对外稳定接口以 daemon 进程协议为准。

对接入方式的建议是：

- 跨语言 / 跨进程 / 自定义 transport 封装：以本协议为准。
- Rust 调用方：优先使用 `DaemonClient` / `ManagedDaemonClient` 这类基础客户端封装，避免直接拼 protocol 请求。

这个协议面向三类调用方：

- IDE / 编辑器插件
- agent / 桌面应用 / 本地服务编排器
- 命令行调试工具

v1 采用 `JSON-RPC 2.0` 风格消息：

- 请求 / 响应走 `jsonrpc: "2.0"`
- 长任务通过 `task` 对象建模
- 进度和状态变化通过 notification 推送
- 所有关键状态都能通过显式 query 重新拉取

## 非目标

- 不暴露底层索引文件格式
- 不承诺 Rust crate root 导出的 API 稳定
- 不暴露内部 workspace probe / rescan 的原始事件流
- 不在 v1 中提供通用订阅 DSL

## 传输

协议语义与具体 framing 无关，当前支持两类本地传输：

- `stdio`：推荐给编辑器 / agent 集成，framing 为 LSP 风格 `Content-Length`
- `tcp` / `unix socket`：推荐给常驻本地 daemon

Rust 侧当前提供的是围绕本地 daemon 的基础客户端封装；其中 `ManagedDaemonClient` 会按 `storage_root/daemon-state.json` 发现或拉起 daemon，并通过同目录下的 `daemon-state.lock` 协调并发启动。

## 顶层消息

请求：

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "open_repo",
  "params": {
    "repo_path": "/path/to/repo"
  }
}
```

成功响应：

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "kind": "repo_opened",
    "repo_id": "/path/to/repo",
    "status": {}
  }
}
```

错误响应：

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32010,
    "message": "workspace is busy rebuilding index"
  }
}
```

通知：

```json
{
  "jsonrpc": "2.0",
  "method": "$/progress",
  "params": {}
}
```

## 核心对象

### `repo_id`

daemon indexed workspace runtime 只支持非 bare Git worktree。`open_repo` / `ensure_repo`
会先执行 Git discovery，并把输入路径归一化到 canonical Git top-level。

`repo_id` 使用 canonical Git top-level 路径字符串。调用方传入 repo 子目录时，runtime
仍以 Git top-level 作为 repo identity；子目录搜索应通过 query `scope.roots` 表达。

如果调用方没有显式传入 `storage_root`，默认值基于 canonical Git top-level 派生：

```text
<canonical-git-top-level>/.flashgrep-index-engine
```

非 Git 目录、bare repository、没有 `HEAD` commit 的 Git repo 都会返回协议错误：

```text
indexed daemon workspace requires a Git worktree with a HEAD commit: <path>
```

### 内部路径保留

协议层不预留宿主产品私有目录名。

当前实现只把下面这些路径视为 `flashgrep` 自己的内部产物：

- `.flashgrep-index-engine`
- `.flashgrep-index`（legacy 兼容保留，仅用于忽略旧产物）
- `.flashgrep-bench`

此外，调用方显式传入的 `storage_root` 也会被视为内部路径并从扫描/搜索候选中排除。

### `RepoStatus`

```json
{
  "repo_id": "/path/to/repo",
  "repo_path": "/path/to/repo",
  "storage_root": "/path/to/repo/.flashgrep-index-engine",
  "base_snapshot_root": "/path/to/repo/.flashgrep-index-engine/base-snapshot",
  "workspace_overlay_root": "/path/to/repo/.flashgrep-index-engine/workspace-overlay",
  "phase": "ready_dirty",
  "snapshot_key": "base-git:abc123+cfg:deadbeef",
  "base_head_commit": "abc123",
  "workspace_head_commit": "def456",
  "overlay_head_commit": "def456",
  "overlay_base_manifest_id": "manifest-base-abc123-def456",
  "base_manifest_id": "manifest-base-abc123-def456",
  "base_delta_depth": 1,
  "base_delta_bytes": 8192,
  "base_advance_target_head": null,
  "cached_head_count": 2,
  "base_compaction_recommended": false,
  "last_probe_unix_secs": 1710000000,
  "last_rebuild_unix_secs": 1710000100,
  "dirty_files": {
    "modified": 2,
    "deleted": 1,
    "new": 3
  },
  "active_task_id": null,
  "probe_healthy": true,
  "workspace_probe_pending": false,
  "last_error": null,
  "last_maintenance_error": null
}
```

`phase` 枚举：

- `opening`
- `missing_base_snapshot`
- `building_base_snapshot`
- `ready_clean`
- `ready_dirty`
- `rebuilding_base_snapshot`
- `degraded`

说明：

- `phase` 表示 repo 当前对外可见状态
- 长任务是否仍在运行，由 `active_task_id` 和 `task/status` 表达
- `ready_dirty` 表示 daemon 已经通过 published overlay snapshot 表达当前 Git worktree
  相对 base snapshot 的增量视图
- `base_head_commit` 是当前 base manifest 对应的 commit；`workspace_head_commit` 是当前 worktree HEAD
- `overlay_head_commit` 和 `overlay_base_manifest_id` 表示已发布 overlay 的源身份
- `base_delta_depth`、`base_delta_bytes`、`cached_head_count` 和
  `base_compaction_recommended` 描述增量 base 的维护状态
- 分支切换不会把兼容 base 置为 missing，也不会自动触发 full rebuild；daemon 先发布当前
  worktree overlay，再以 commit diff 增量推进 base manifest
- `overlay` 存在时表示 daemon 已打开 workspace overlay engine，并暴露其当前已发布 overlay 与后台维护状态
- `probe_healthy=false` 表示最近一次 Git dirty probe 失败；daemon 会保留上一份成功发布的查询视图，并由 maintenance worker 重试
- `workspace_probe_pending=true` 表示 daemon 仍欠一次 worktree reconcile：此时
  `dirty_files`、`phase` 和已发布 overlay 描述的是**最近一次观测到**的 worktree 状态，
  而不是当前状态。`open_repo` / `ensure_repo` 只恢复上次持久化的 overlay（不走 worktree
  遍历，超大仓上可省数秒），因此打开后该字段为 `true`，直到 maintenance worker 完成
  authoritative probe 后自动清零。需要 authoritative 结果的调用方应显式调用
  `refresh_repo`；daemon 不会因为该字段为 `true` 就把查询降级到 `rg` 全量扫描
- `last_error` 由 worktree probe 和后台任务共用，且**每一次成功的 probe 都会把它清零**。
  probe 以秒级频率运行，所以只落在这里的后台维护失败会在轮询者看到之前被抹掉
  （实测 v0.2.14 上约 3.5 s）
- `last_maintenance_error` 是后台 base 维护（`advance_base_snapshot` /
  `compact_base_deltas`）失败的**独立**插槽：probe 不碰它，只有同类维护任务下一次成功
  才清零。以人的时间尺度轮询 `get_repo_status` 的调用方应当读这个字段，而不是 `last_error`。
  早于 v0.2.15 的 daemon 不发送该字段

`overlay` 字段：

- `base_manifest_id`
- `committed_seq_no`
- `last_seq_no`
- `uncommitted_ops`
- `pending_docs`
- `active_segments`
- `active_delete_segments`
- `merge_requested`
- `merge_running`
- `merge_attempts`
- `merge_completed`
- `merge_failed`
- `last_merge_error`

### `TaskStatus`

```json
{
  "task_id": "task-7",
  "workspace_id": "/path/to/repo",
  "kind": "build_base_snapshot",
  "state": "running",
  "phase": "tokenizing",
  "message": "Tokenizing repository files",
  "processed": 120,
  "total": 400,
  "started_unix_secs": 1710000000,
  "updated_unix_secs": 1710000004,
  "finished_unix_secs": null,
  "cancellable": true,
  "error": null
}
```

`kind` 枚举：

- `build_base_snapshot`
- `rebuild_base_snapshot`
- `advance_base_snapshot`
- `compact_base_deltas`
- `refresh_workspace`

`advance_base_snapshot` 的 progress message 会分别报告 from/to HEAD、add/modify/delete 路径数、
过滤路径数、Git blob 读取数、tokenized/deleted 文档数、delta/delete segment 数、delta bytes、
parent/candidate manifest ID 和 cache hit。

`state` 枚举：

- `queued`
- `running`
- `completed`
- `failed`
- `cancelled`

`phase` 枚举：

- `scanning`
- `tokenizing`
- `writing`
- `finalizing`
- `refreshing_overlay`

说明：

- 当前 base snapshot build 主路径通常只会发出 `scanning` 和 `finalizing`。
- `tokenizing` / `writing` 仍保留在协议里，作为更细粒度构建阶段的前向兼容枚举。

### `SearchCompleted`

```json
{
  "kind": "search_completed",
  "repo_id": "/path/to/repo",
  "backend": "indexed_workspace_view",
  "status": {},
  "results": {
    "candidate_docs": 42,
    "searches_with_match": 3,
    "bytes_searched": 8192,
    "matched_lines": 3,
    "matched_occurrences": 4,
    "hits": []
  }
}
```

`backend` 枚举：

- `indexed_clean`
- `indexed_workspace_view`
- `rg_fallback`
- `scan_fallback`

### `GlobCompleted`

```json
{
  "kind": "glob_completed",
  "repo_id": "/path/to/repo",
  "status": {},
  "paths": [
    "/path/to/repo/src/lib.rs",
    "/path/to/repo/src/main.rs"
  ]
}
```

`glob` 返回的是当前 corpus / ignore / hidden 规则下可见的文件路径，而不是索引里的路径列表。

## 已实现方法

当前 daemon 的稳定对外方法如下：

- `initialize`
- `initialized`
- `ping`
- `open_repo`
- `ensure_repo`
- `get_repo_status`
- `refresh_repo`
- `build_base_snapshot`
- `rebuild_base_snapshot`
- `search`
- `glob`
- `close_repo`
- `base_snapshot/build`
- `base_snapshot/rebuild`
- `task/status`
- `task/cancel`
- `shutdown`
- `exit`

其中：

- `build_base_snapshot` / `rebuild_base_snapshot` 是同步方法，返回最终 `RepoStatus`
- `base_snapshot/build` / `base_snapshot/rebuild` 是异步方法，立即返回 `TaskStatus`

## 会话方法

### `initialize`

用途：

- 建立 client/server 会话
- 协商 capabilities

请求：

```json
{
  "client_info": {
    "name": "my-client",
    "version": "0.1.0"
  },
  "capabilities": {
    "progress": true,
    "status_notifications": true,
    "task_notifications": true
  }
}
```

响应：

```json
{
  "kind": "initialize_result",
  "protocol_version": 1,
  "server_info": {
    "name": "flashgrep",
    "version": "0.1.0"
  },
  "capabilities": {
    "workspace_open": true,
    "workspace_ensure": true,
    "workspace_list": false,
    "workspace_refresh": true,
    "base_snapshot_build": true,
    "base_snapshot_rebuild": true,
    "task_status": true,
    "task_cancel": true,
    "search_query": true,
    "glob_query": true,
    "progress_notifications": true,
    "status_notifications": true
  },
  "search": {
    "search_modes": [
      "files_with_matches",
      "line_matches",
      "count_only",
      "count_matches"
    ]
  }
}
```

### `initialized`

客户端声明已准备好接收 notification。

### `ping`

响应：

```json
{
  "kind": "pong",
  "now_unix_secs": 1710000000
}
```

### `shutdown`

请求 server 进入优雅关闭流程。

### `exit`

无响应通知，用于结束进程。

## Repo 生命周期方法

### `open_repo`

用途：

- 注册 repo runtime
- 启动后台 maintenance / workspace probe 循环
- 不自动构建 base snapshot
- 要求 `repo_path` 位于非 bare Git worktree 内，且该 repo 已有 `HEAD` commit
- 只恢复上次持久化的 workspace overlay（校验 manifest 身份与 `HEAD`，不遍历 worktree），
  authoritative worktree reconcile 交给 maintenance worker；因此响应中的
  `status.workspace_probe_pending` 为 `true`，`dirty_files` / `phase` 是上次观测值

补充约定：

- 如果调用方通过 managed client 打开 repo，daemon 发现/复用默认按 `storage_root` 派生的 state file 路径进行。
- `open_repo` 本身不要求接入方自己先拉起 daemon；这部分可以由 managed client 负责。
- `repo_path` 可以是 repo 子目录；响应中的 `repo_id`、`status.repo_path` 和默认
  `storage_root` 都会归一化到 Git top-level。

请求：

```json
{
  "repo_path": "/path/to/repo",
  "storage_root": null,
  "config": {
    "tokenizer": "sparse_ngram",
    "corpus_mode": "respect_ignore",
    "include_hidden": false,
    "max_file_size": 2097152,
    "min_sparse_len": 3,
    "max_sparse_len": 8
  },
  "refresh": {
    "base_delta_max_segments": 8,
    "base_delta_max_delete_segments": 8,
    "base_delta_max_bytes_ratio": 0.1,
    "base_head_cache_entries": 4,
    "overlay_merge_min_delay_ms": 2000,
    "overlay_merge_retry_delay_ms": 10000
  }
}
```

响应：

```json
{
  "kind": "repo_opened",
  "repo_id": "/path/to/repo",
  "status": {}
}
```

### `ensure_repo`

用途：

- 确保 repo 已打开
- 若没有 repo root、config 和 schema 兼容的 base snapshot，同步执行一次 initial build
- base HEAD 与当前 HEAD 不同时复用已有 base，并由后台增量 advance 对齐

请求参数与 `open_repo` 相同。

响应：

```json
{
  "kind": "repo_ensured",
  "repo_id": "/path/to/repo",
  "status": {},
  "indexed_docs": 1234
}
```

说明：

- `indexed_docs = null` 表示 repo 已打开，且无需新建 base snapshot
- `indexed_docs = null` 表示已有 base snapshot 的 `HEAD`、config fingerprint、repo root 均匹配当前 Git discovery
- 如果 repo 已提交/checkout 到新的 `HEAD`，`ensure_repo` 会同步构建当前 `HEAD + config`
  的 base snapshot，而不是复用任意旧 snapshot
- 重启后不再在打开阶段做 Git dirty probe：与 `open_repo` 一样只恢复持久化 overlay，
  并用 `workspace_probe_pending` 声明仍欠的 reconcile；这样超大仓的打开耗时从秒级降到
  百毫秒级，语义变化只在于 dirty 字段是最近观测值

### `get_repo_status`

请求：

```json
{
  "repo_id": "/path/to/repo"
}
```

响应：

```json
{
  "kind": "repo_status",
  "status": {}
}
```

### `refresh_repo`

用途：

- 主动从 worktree 重算 dirty 集
- 当后台 probe 尚未收敛或客户端怀疑状态过期时使用
- 这是 `workspace_probe_pending=true` 时拿到 authoritative 状态的唯一手段：调用返回时
  该字段已清零，`dirty_files` / `phase` / overlay 都对应当次 worktree 观测结果

请求：

```json
{
  "repo_id": "/path/to/repo",
  "force": false
}
```

响应：

```json
{
  "kind": "repo_status",
  "status": {}
}
```

### `close_repo`

用途：

- 关闭 repo runtime
- 停止后台 maintenance / workspace probe 循环

请求：

```json
{
  "repo_id": "/path/to/repo"
}
```

响应：

```json
{
  "kind": "repo_closed",
  "repo_id": "/path/to/repo"
}
```

## 索引方法

### `build_base_snapshot`

同步构建缺失的 base snapshot。

daemon base snapshot build 只使用 Git identity；snapshot key 形如：

```text
base-git-<head_commit>-<config_fingerprint>
```

repo fingerprint fallback 只属于库层/低层能力，不属于 daemon indexed workspace runtime。

请求：

```json
{
  "repo_id": "/path/to/repo"
}
```

响应：

```json
{
  "kind": "repo_base_snapshot_built",
  "indexed_docs": 1234,
  "status": {}
}
```

### `rebuild_base_snapshot`

同步重建 base snapshot。

请求参数与 `build_base_snapshot` 相同。

响应：

```json
{
  "kind": "repo_base_snapshot_rebuilt",
  "indexed_docs": 1234,
  "status": {}
}
```

语义：

- 如果当前仍有可用 snapshot，重建期间搜索继续使用最近一次可用视图
- 新索引切换成功后，再更新 repo 状态

### `base_snapshot/build`

异步启动一次 base snapshot build。

请求：

```json
{
  "repo_id": "/path/to/repo"
}
```

响应：

```json
{
  "kind": "task_started",
  "task": {}
}
```

### `base_snapshot/rebuild`

异步启动一次 base snapshot rebuild。

请求和响应结构与 `base_snapshot/build` 相同。

## 查询方法

### `search`

用途：

- 对当前 repo 视图执行一次搜索

请求：

```json
{
  "repo_id": "/path/to/repo",
  "query": {
    "pattern": "foo.*bar",
    "patterns": [],
    "case_insensitive": false,
    "multiline": false,
    "dot_matches_new_line": false,
    "fixed_strings": false,
    "word_regexp": false,
    "line_regexp": false,
    "top_k_tokens": 6,
    "max_count": null,
    "global_max_results": null,
    "search_mode": "line_matches"
  },
  "scope": {
    "roots": [],
    "globs": [],
    "iglobs": [],
    "type_add": [],
    "type_clear": [],
    "types": [],
    "type_not": []
  }
}
```

响应见 `SearchCompleted`。

约束：

- `scope.roots` 必须为空，或解析后仍位于 `repo_id` 对应 repo 根目录之内
- daemon 不接受借助 `roots` 越出 repo 边界的搜索请求
- `QuerySpec` 不支持上下文行（`before_context` / `after_context`）；需要上下文的调用方要自己走 `rg`
- 没有 `allow_scan_fallback` 开关：indexed repo 未 ready 时由 daemon 内部按
  `QueryBackend`（index → `rg` → 全量扫描）自行过渡，调用方无法请求或禁止它，
  也不能借此绕过 `open_repo` / `ensure_repo` 的 Git worktree discovery 要求

调用方应至少读取：

- `backend`
- `status`

这样才能判断本次是否：

- 走了 index
- 使用了 dirty overlay / repair
- 退回 `rg` 或全量扫描

`status.workspace_probe_pending=true` 表示本次结果基于最近观测到的 worktree 视图：daemon
不会为此自行降级到全量扫描，需要 authoritative 结果的调用方应调用一次 `refresh_repo`
后重搜。

### `glob`

用途：

- 独立执行路径枚举 / glob / type filter 解析
- 不要求 index 已存在
- 复用 repo 当前配置中的 ignore / hidden / max-file-size 规则

请求：

```json
{
  "repo_id": "/path/to/repo",
  "scope": {
    "roots": [
      "src"
    ],
    "globs": [
      "*.rs"
    ],
    "iglobs": [],
    "type_add": [],
    "type_clear": [],
    "types": [
      "rust"
    ],
    "type_not": []
  }
}
```

响应见 `GlobCompleted`。

约束：

- `scope.roots` 必须为空，或解析后仍位于 `repo_id` 对应 repo 根目录之内
- `glob` 语义是 repo 内路径枚举，不允许把 daemon 当作任意目录扫描器使用
- daemon 可以在单个 repo runtime 内缓存 `glob(scope)` 结果，但 background probe 或 refresh 只要改变当前可见文件集，这份缓存就必须失效

## 任务方法

### `task/status`

请求：

```json
{
  "task_id": "task-7"
}
```

响应：

```json
{
  "kind": "task_status",
  "task": {}
}
```

### `task/cancel`

请求：

```json
{
  "task_id": "task-7"
}
```

响应：

```json
{
  "kind": "task_cancelled",
  "task_id": "task-7",
  "accepted": true
}
```

## 通知

### `$/progress`

用途：

- 推送长任务阶段进度

参数：

```json
{
  "task_id": "task-7",
  "workspace_id": "/path/to/repo",
  "kind": "build_base_snapshot",
  "phase": "tokenizing",
  "message": "Tokenizing repository files",
  "processed": 120,
  "total": 400
}
```

### `workspace/statusChanged`

用途：

- repo 状态变化通知

参数：

```json
{
  "workspace_id": "/path/to/repo",
  "status": {}
}
```

### `task/finished`

用途：

- 任务结束通知

参数：

```json
{
  "task": {}
}
```
