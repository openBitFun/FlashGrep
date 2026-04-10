# Codgrep Daemon Protocol v1

## 目标

`codgrep` 的独立进程协议需要满足三类调用方：

- IDE / 编辑器插件
- agent / 桌面应用 / 本地服务编排器
- 命令行调试工具

v1 的目标不是把 `codgrep` 伪装成通用 LSP，而是采用 **LSP-like 的协议风格**：

- 请求 / 响应使用 `JSON-RPC 2.0`
- 长任务通过 `task` 模型异步执行
- 进度与状态变化通过 notification 推送
- 所有关键状态都能通过显式 query 重新拉取

这样外部程序既能像接 LSP 一样接入，也不会被 `codgrep` 的搜索 / 索引语义束缚。

## 非目标

- 不暴露底层索引文件格式
- 不暴露 watcher 的原始文件系统事件流
- 不承诺 `advanced::json` 或实验层 API 的 schema 稳定性
- 不在 v1 中实现通用订阅 DSL

## 设计原则

1. 进度必须同时支持 push 和 pull。
2. 搜索结果必须携带足够的执行元信息，调用方能判断“这次结果到底是怎么来的”。
3. workspace 状态和 task 状态拆分，避免把“当前有任务在跑”和“当前 repo 能不能搜索”混成一个字段。
4. `rebuild` 不应让已有可用索引瞬间失效；能继续搜时就继续搜。
5. 对现有 daemon 能力尽量做增量升级，而不是推翻重写。

## 传输与 framing

协议层统一采用 `JSON-RPC 2.0` 消息模型。

v1 允许两种传输适配：

- `stdio`
  推荐用于编辑器 / agent 集成。framing 使用 LSP 风格 `Content-Length`。
- `tcp` / `unix socket`
  推荐用于本地独立 daemon。v1 可以继续兼容当前逐行 JSON 的 framing，但消息体应升级为 `JSON-RPC 2.0`。

消息语义不依赖具体 framing。

## 顶层约定

### 请求

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "workspace/open",
  "params": {}
}
```

### 响应

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {}
}
```

### 错误

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32010,
    "message": "workspace is busy rebuilding index",
    "data": {
      "workspace_id": "/repo"
    }
  }
}
```

### 通知

```json
{
  "jsonrpc": "2.0",
  "method": "$/progress",
  "params": {}
}
```

## 生命周期方法

### `initialize`

用途：

- 建立 client/server 会话
- 协商 capabilities
- 声明 transport / tracing / notification 支持情况

参数：

```json
{
  "clientInfo": {
    "name": "my-client",
    "version": "0.1.0"
  },
  "capabilities": {
    "progress": true,
    "statusNotifications": true,
    "taskNotifications": true
  }
}
```

返回：

```json
{
  "protocolVersion": 1,
  "serverInfo": {
    "name": "codgrep",
    "version": "0.1.0"
  },
  "capabilities": {
    "workspaceOpen": true,
    "workspaceEnsure": true,
    "workspaceList": true,
    "workspaceRefresh": true,
    "indexBuild": true,
    "indexRebuild": true,
    "taskStatus": true,
    "taskCancel": true,
    "searchQuery": true,
    "progressNotifications": true,
    "statusNotifications": true
  },
  "search": {
    "consistencyModes": [
      "snapshot_only",
      "workspace_eventual",
      "workspace_strict"
    ],
    "searchModes": [
      "count_only",
      "count_matches",
      "first_hit_only",
      "materialize_matches"
    ]
  }
}
```

### `initialized`

用途：

- 表示客户端已经准备好接收 notification
- server 可以从这里开始主动推状态变化

参数：`null`

### `shutdown`

用途：

- 请求 server 进入优雅关闭流程
- 返回成功后，客户端应继续发送 `exit`

### `exit`

用途：

- 无响应通知
- 对齐 LSP 习惯，结束进程

### `ping`

用途：

- 轻量健康检查

返回：

```json
{
  "nowUnixSecs": 1710000000
}
```

## 核心对象

### `WorkspaceId`

v1 继续沿用当前语义，默认使用 normalized `repo_path` 字符串。

后续如果需要支持同一路径多配置并存，可以升级为：

- `repo_path + config_fingerprint`

但 v1 不提前引入复杂 id。

### `WorkspaceStatus`

```json
{
  "workspaceId": "/path/to/repo",
  "repoPath": "/path/to/repo",
  "indexPath": "/path/to/repo/.codgrep-index",
  "snapshotKey": "git:abc123+cfg:deadbeef",
  "workspaceState": "ready_dirty",
  "indexAvailable": true,
  "watcherHealthy": true,
  "dirtyFiles": {
    "modified": 2,
    "deleted": 1,
    "new": 3
  },
  "rebuildRecommended": false,
  "lastProbeUnixSecs": 1710000000,
  "lastRebuildUnixSecs": 1710000100,
  "activeTaskId": "task-7",
  "lastError": null
}
```

字段定义：

- `workspaceState`
  表示 repo 当前对外可见状态，不等同于 task 状态。
- `indexAvailable`
  是否存在可用 base snapshot。
- `watcherHealthy`
  watcher 是否处于健康状态。watcher 出错但索引仍可搜索时，不应直接让客户端失明。
- `activeTaskId`
  当前前台任务；没有则为 `null`。

### `WorkspaceState`

- `missing_index`
- `ready_clean`
- `ready_dirty`
- `degraded`

说明：

- `indexing` / `rebuilding` 不放在这里，放进 task 状态。
- 如果需要表示“现在正在 build，但旧索引仍可用”，由 `workspaceState=ready_*` 加 `activeTaskId!=null` 表达。

### `TaskStatus`

```json
{
  "taskId": "task-7",
  "workspaceId": "/path/to/repo",
  "kind": "build_index",
  "state": "running",
  "phase": "tokenizing",
  "message": "Tokenizing repository files",
  "processed": 120,
  "total": 400,
  "startedUnixSecs": 1710000000,
  "updatedUnixSecs": 1710000004,
  "finishedUnixSecs": null,
  "cancellable": true,
  "error": null
}
```

`state` 枚举：

- `queued`
- `running`
- `completed`
- `failed`
- `cancelled`

`kind` 枚举：

- `build_index`
- `rebuild_index`
- `refresh_workspace`

`phase` 初版与现有构建进度对齐：

- `scanning`
- `tokenizing`
- `writing`
- `finalizing`
- `refreshing_overlay`

## 方法定义

## `workspace/open`

用途：

- 注册一个 repo runtime
- 启动 watcher
- 不自动触发 build

参数：

```json
{
  "repoPath": "/path/to/repo",
  "indexPath": null,
  "config": {
    "tokenizer": "sparse_ngram",
    "corpusMode": "respect_ignore",
    "includeHidden": false,
    "maxFileSize": 2097152,
    "minSparseLen": 3,
    "maxSparseLen": 8
  },
  "refresh": {
    "rebuildDirtyThreshold": 256
  }
}
```

返回：

```json
{
  "workspaceId": "/path/to/repo",
  "status": {}
}
```

## `workspace/ensure`

用途：

- 确保 workspace 已打开
- 如果缺少索引，则启动 build

参数与 `workspace/open` 相同。

返回：

```json
{
  "workspaceId": "/path/to/repo",
  "status": {},
  "ensuredBy": "existing_index",
  "taskId": null
}
```

`ensuredBy` 取值：

- `existing_index`
- `scheduled_build`
- `already_open`

说明：

- v1 建议 `workspace/ensure` 不阻塞等待 build 完成。
- 若触发了 build，则返回 `taskId`，并通过 notification 持续跟踪。

## `workspace/list`

用途：

- 返回当前 daemon 已打开的所有 workspace

返回：

```json
{
  "workspaces": [
    {
      "workspaceId": "/path/to/repo",
      "status": {}
    }
  ]
}
```

## `workspace/status`

参数：

```json
{
  "workspaceId": "/path/to/repo"
}
```

返回：

```json
{
  "status": {}
}
```

## `workspace/refresh`

用途：

- 主动从 worktree 重算 dirty 集
- 在 watcher 失步或客户端怀疑状态过期时使用

参数：

```json
{
  "workspaceId": "/path/to/repo",
  "force": false
}
```

返回：

```json
{
  "status": {}
}
```

## `workspace/close`

用途：

- 关闭 repo runtime
- 停止 watcher

参数：

```json
{
  "workspaceId": "/path/to/repo"
}
```

返回：

```json
{
  "workspaceId": "/path/to/repo"
}
```

## `index/build`

用途：

- 为缺失索引的 workspace 启动 build
- 异步返回 task handle

参数：

```json
{
  "workspaceId": "/path/to/repo"
}
```

返回：

```json
{
  "taskId": "task-7",
  "status": {}
}
```

约束：

- 如果已有运行中的 build/rebuild，返回错误
- 如果已有可用索引，是否允许重复 build 由实现决定；v1 推荐直接返回错误并提示使用 `index/rebuild`

## `index/rebuild`

用途：

- 强制重建索引
- 异步返回 task handle

参数与 `index/build` 相同。

返回：

```json
{
  "taskId": "task-8",
  "status": {}
}
```

语义：

- 如果 workspace 当前已有可用索引，`rebuild` 期间搜索应继续使用最近一次可用 snapshot。
- 新索引完成并切换成功后，再通过 `workspace/statusChanged` 通知客户端。
- 如果 workspace 当前没有可用索引，则行为等同于 `build`。

## `index/status`

用途：

- 拉取当前索引任务和索引可用性

参数：

```json
{
  "workspaceId": "/path/to/repo"
}
```

返回：

```json
{
  "workspaceId": "/path/to/repo",
  "indexAvailable": true,
  "activeTask": null,
  "status": {}
}
```

说明：

- 这个方法是 `workspace/status` 的索引视角快捷入口。
- 如果实现成本高，v1 可以先复用 `workspace/status` + `task/status`，`index/status` 后补。

## `search/query`

用途：

- 执行一次搜索

参数：

```json
{
  "workspaceId": "/path/to/repo",
  "query": {
    "pattern": "foo.*bar",
    "patterns": [
      "foo.*bar"
    ],
    "caseInsensitive": false,
    "multiline": false,
    "dotMatchesNewLine": false,
    "fixedStrings": false,
    "wordRegexp": false,
    "lineRegexp": false,
    "beforeContext": 0,
    "afterContext": 0,
    "topKTokens": 6,
    "maxCount": null,
    "searchMode": "materialize_matches"
  },
  "scope": {
    "roots": [],
    "globs": [],
    "iglobs": [],
    "typeAdd": [],
    "typeClear": [],
    "types": [],
    "typeNot": []
  },
  "consistency": "workspace_eventual",
  "allowScanFallback": false
}
```

返回：

```json
{
  "workspaceId": "/path/to/repo",
  "backend": "indexed_workspace_repair",
  "consistencyRequested": "workspace_eventual",
  "consistencyApplied": "workspace_eventual",
  "snapshotKey": "git:abc123+cfg:deadbeef",
  "workspaceState": "ready_dirty",
  "rebuildRecommended": false,
  "results": {
    "candidateDocs": 42,
    "searchesWithMatch": 3,
    "bytesSearched": 8192,
    "matchedLines": 3,
    "matchedOccurrences": 4,
    "hits": []
  }
}
```

必须携带的执行元信息：

- `backend`
- `consistencyRequested`
- `consistencyApplied`
- `snapshotKey`
- `workspaceState`
- `rebuildRecommended`

这样调用方才知道：

- 这次是否用了 index
- 是否用了 dirty repair
- 是否实际上退回 scan
- 当前索引状态是否值得提示用户重建

搜索期间如果存在活动中的 `build` / `rebuild`：

- 只要当前仍有可用 snapshot，`search/query` 就不应因为后台任务存在而直接失败。
- 返回值中的 `workspaceState` 仍然描述当前可用视图，而不是后台任务状态。
- 后台任务状态由 `activeTaskId` 和 `task/status` 表达。

## `task/status`

用途：

- 拉取某个任务状态

参数：

```json
{
  "taskId": "task-7"
}
```

返回：

```json
{
  "task": {}
}
```

## `task/list`

用途：

- 调试和运维使用

参数：

```json
{
  "workspaceId": "/path/to/repo",
  "includeFinished": false
}
```

返回：

```json
{
  "tasks": []
}
```

说明：

- 如果实现复杂，v1 可以晚于 `task/status`。

## `task/cancel`

用途：

- 取消长任务

参数：

```json
{
  "taskId": "task-7"
}
```

返回：

```json
{
  "taskId": "task-7",
  "accepted": true
}
```

说明：

- 同时支持标准 `$/cancelRequest`
- 但业务层仍保留 `task/cancel`，因为 build/rebuild 是显式任务对象，不一定与单个 RPC 请求生命周期绑定

## 通知

## `$/progress`

用途：

- 推送长任务阶段进度

参数：

```json
{
  "taskId": "task-7",
  "workspaceId": "/path/to/repo",
  "kind": "build_index",
  "phase": "tokenizing",
  "message": "Tokenizing repository files",
  "processed": 120,
  "total": 400
}
```

要求：

- 任务开始时至少发一次
- phase 变更时应发
- 完成前应发 final progress 或 `task/finished`

## `workspace/statusChanged`

用途：

- workspace 对外状态变化时推送

触发示例：

- `missing_index -> ready_clean`
- `ready_clean -> ready_dirty`
- `ready_dirty -> degraded`
- `activeTaskId` 变化

参数：

```json
{
  "workspaceId": "/path/to/repo",
  "status": {}
}
```

## `workspace/dirtyChanged`

用途：

- dirty 文件计数变化时推送

参数：

```json
{
  "workspaceId": "/path/to/repo",
  "dirtyFiles": {
    "modified": 2,
    "deleted": 1,
    "new": 3
  },
  "rebuildRecommended": false
}
```

说明：

- 这个通知不是必须首批实现。
- 如果实现顺序需要收缩，`workspace/statusChanged` 足够承载这类变化。

## `task/finished`

用途：

- 任务进入终态时推送

参数：

```json
{
  "task": {}
}
```

终态包括：

- `completed`
- `failed`
- `cancelled`

## `window/logMessage`

用途：

- 面向调试和排障的日志通知

参数：

```json
{
  "level": "warn",
  "message": "watch error: ...",
  "workspaceId": "/path/to/repo"
}
```

## 错误模型

建议区分两层：

- JSON-RPC 通用错误码
- `codgrep` 业务错误码

业务错误码建议：

- `workspace_not_found`
- `workspace_busy`
- `missing_index`
- `task_not_found`
- `task_not_cancellable`
- `invalid_query`
- `watcher_degraded`
- `internal_error`

错误 `data` 中至少尽量带：

- `workspaceId`
- `taskId`
- `retryable`

## 与当前实现的映射

当前已有能力：

- `open/ensure/status/refresh/build/rebuild/search/close/shutdown`
- repo 级状态：`phase/snapshot_key/dirty_files/rebuild_recommended/last_error`
- build 进度 hook：`IndexProgress`
- build cancel hook：`should_cancel`

当前缺失能力：

- `JSON-RPC 2.0` envelope
- notification 通道
- 独立 `task` 存储和 task id
- build/rebuild 异步化
- 可查询的 cancel 状态
- `workspace/list` / `task/list`
- `watcherHealthy` 明确建模

当前实现与目标语义：

- `open_repo` 现在会快速返回，并把初始 workspace 探测放到后台 `refresh_workspace` 任务中
- `RepoPhase::Opening` 表示 repo 已注册，但初始 overlay / freshness probe 仍在进行
- `workspace_eventual` 在 `opening / missing_index / indexing / rebuilding` 期间会尽量可用：
  - 直接优先走 `rg_fallback`
  - `rg` 不可用或失败时再退回内部 `scan_fallback`
- `workspace_strict` 在 repo 尚未 ready 时仍然返回 `not_ready`

## 状态机建议

### Workspace

```text
opening
  -> missing_index
  -> ready_clean
  -> ready_dirty
  -> degraded

missing_index
  -> ready_clean
  -> ready_dirty
  -> degraded
```

### Task

```text
queued -> running -> completed
                -> failed
                -> cancelled
```

说明：

- `opening` 只表示初始打开阶段
- `indexing / rebuilding` 仍会出现在 workspace 状态中，用来表达后台任务正在写入新索引
- 查询侧不要把这些状态简单等同于“不可搜索”；应结合 `backend` 看是否已经回退到 `rg` / `scan` fallback

## v1 实现顺序

### Phase 1

- 把现有协议 envelope 升级为 `JSON-RPC 2.0`
- 保留当前同步方法名的兼容层
- 补 `initialize` / `initialized` / `shutdown` / `exit`

### Phase 2

- 引入 `TaskManager`
- `index/build` / `index/rebuild` 改为异步任务
- 复用现有 `IndexBuildOptions.progress` 和 `should_cancel`

### Phase 3

- 增加 `$/progress`、`workspace/statusChanged`、`task/finished`
- client 断线重连后通过 `workspace/status` / `task/status` 补状态

### Phase 4

- 补 `workspace/list` / `task/list`
- 视需要增加 `index/status`

## 向后兼容策略

v1 落地时建议保留一段过渡期：

- 现有 `daemon *` CLI 继续可用
- client 仍可走当前 TCP 模式
- server 内部先支持旧协议转新协议的适配

这样可以先稳定服务端任务模型，再逐步切掉旧协议。

## 最小可实现接口集

如果首批只做最有价值的一组，推荐：

- `initialize`
- `initialized`
- `workspace/open`
- `workspace/ensure`
- `workspace/status`
- `workspace/refresh`
- `index/build`
- `index/rebuild`
- `search/query`
- `task/status`
- `task/cancel`
- `workspace/close`
- `shutdown`
- `exit`
- `$/progress`
- `workspace/statusChanged`
- `task/finished`

这组接口已经足够支撑：

- 外部程序感知索引是否存在
- 外部程序追踪 build/rebuild 进度
- 外部程序在 build 中仍能决定是否允许搜索
- 外部程序在错误、断线、重连后恢复状态
