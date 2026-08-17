# flashgrep MCP 变更说明

发布时间：2026-08-17 +0800

## 本次包内容

- MCP Server 版本：`0.2.13`
- 内置 `flashgrep` 后端版本：`0.2.13`
- 打包平台：
  - `flashgrep-aarch64-apple-darwin`
  - `flashgrep-x86_64-apple-darwin`
  - `flashgrep-aarch64-unknown-linux-musl`
  - `flashgrep-x86_64-unknown-linux-musl`
  - `flashgrep-aarch64-pc-windows-msvc.exe`
  - `flashgrep-x86_64-pc-windows-msvc.exe`

本次为修复版本，针对用户反馈的两个缺陷。两者根源相同：仓库 `phase` 是被写入的状态而非
从磁盘事实推导出来的状态，而每一条恢复路径又都以它卡住的那个 phase 作为准入条件。

## 关键修复

### 构建索引时切分支不再导致索引永久失效

在 base snapshot 构建过程中切换 Git 分支，此前会让索引一直处于失效状态。这需要同时打开
四道互相咬合的关卡：

- 任务结束时不再只清空 `active_task_id` 而不归还 `phase`。任务现在会记录自己是否占用了
  仓库 phase，未成功结束时必须交还，不再把仓库留在 `building_base_snapshot` 而背后没有
  任何任务。
- 后台 workspace probe 此前拒绝在 ready 之外的 phase 运行，而 probe 是唯一能从磁盘重新
  推导 phase 的代码——这正是失效从"恢复得慢"变成"永久"的原因。现在它也会以较慢的节奏
  探测那些搁浅在中间态或 degraded 且无任务在跑的仓库。
- `ensure_repo` 和 MCP server 两个构建入口此前只在 `missing_base_snapshot` 时触发，现在
  同样会把"正在构建但没有活跃任务"视为需要重建。
- 上述状态下搜索会静默走 `rg` fallback 并"成功"返回，因此该问题表现为索引变慢而不是报错。

同时修复了同类的潜在缺陷：`RepoPhase::Degraded` 此前同样是终态陷阱，由 probe 失败设置后
就再也不会被 probe。

### `mcp-bridge` 复用已有索引时不再超时

此前 `open_repo` 在回复 `initialize` 之前同步执行，使得大型索引上 10-12 秒的 open 落在
固定 10 秒的握手预算之内，健康的索引也会握手超时。

现在握手先在本地回答；该请求仍会重放给 daemon 以便注册连接，daemon 返回的重复响应按 id
抑制。open 改为阻塞第一个真实请求——在那里等待是符合预期的。

两侧的启动预算同时上调，并支持通过环境变量配置：

- `FLASHGREP_MCP_START_TIMEOUT_MS`
- `FLASHGREP_DAEMON_START_TIMEOUT_MS`

超时错误信息中会指明前者，便于大仓库场景下自行调整。

## 工程质量

- `cargo clippy --all-targets` 无告警。
- 完整测试：`241 passed, 1 failed, 1 ignored`。唯一失败项为
  `bench_rg_runner_uses_snapshot_visible_scope_for_custom_repo`，属于开发环境问题——该
  环境中 `rg` 是 shell 函数而非真实可执行文件，而该测试需要启动该可执行文件。已在干净
  工作树上确认该失败与本次改动无关。
- 新增三个回归测试，每个都单独验证过：只回退其对应的那一处修复即失败。

## 后续设计

本仓库的 branch switch 增量 base promotion 改造方案仍位于：

```text
docs/branch_switch_incremental_index_plan.md
```

该文档为设计方案，不属于 `0.2.13` 已实现功能。
