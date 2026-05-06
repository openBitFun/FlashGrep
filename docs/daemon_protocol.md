# Codgrep Daemon Protocol v1

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

v1 沿用当前实现语义：默认使用 normalized `repo_path` 字符串。

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
  "last_probe_unix_secs": 1710000000,
  "last_rebuild_unix_secs": 1710000100,
  "dirty_files": {
    "modified": 2,
    "deleted": 1,
    "new": 3
  },
  "rebuild_recommended": false,
  "active_task_id": null,
  "probe_healthy": true,
  "last_error": null
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
- `ready_dirty` 表示 daemon 已经或即将通过 overlay snapshot 表达当前 worktree 增量视图
- `overlay` 存在时表示 daemon 已打开 workspace overlay engine，并暴露其当前已发布 overlay 与后台维护状态

`overlay` 字段：

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
- `refresh_workspace`

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
      "count_only",
      "count_matches",
      "first_hit_only",
      "materialize_matches"
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

补充约定：

- 如果调用方通过 managed client 打开 repo，daemon 发现/复用默认按 `storage_root` 派生的 state file 路径进行。
- `open_repo` 本身不要求接入方自己先拉起 daemon；这部分可以由 managed client 负责。

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
    "rebuild_dirty_threshold": 256,
    "overlay_auto_checkpoint_max_uncommitted_ops": 256
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
- 若缺少 base snapshot，同步执行一次 build

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
- 重启后会重新扫描 worktree 并恢复当前状态，这会增加启动阶段耗时，但不会改变对外语义

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
    "before_context": 0,
    "after_context": 0,
    "top_k_tokens": 6,
    "max_count": null,
    "search_mode": "materialize_matches"
  },
  "scope": {
    "roots": [],
    "globs": [],
    "iglobs": [],
    "type_add": [],
    "type_clear": [],
    "types": [],
    "type_not": []
  },
  "allow_scan_fallback": false
}
```

响应见 `SearchCompleted`。

约束：

- `scope.roots` 必须为空，或解析后仍位于 `repo_id` 对应 repo 根目录之内
- daemon 不接受借助 `roots` 越出 repo 边界的搜索请求

调用方应至少读取：

- `backend`
- `status`

这样才能判断本次是否：

- 走了 index
- 使用了 dirty overlay / repair
- 退回 `rg` 或全量扫描

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
