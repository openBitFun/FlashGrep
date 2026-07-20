# flashgrep MCP 变更说明

发布时间：2026-07-20 +0800

## 本次包内容

- MCP Server 版本：`0.2.12`
- 内置 `flashgrep` 后端版本：`0.2.12`
- 打包平台：
  - `flashgrep-aarch64-apple-darwin`
  - `flashgrep-x86_64-apple-darwin`
  - `flashgrep-aarch64-unknown-linux-musl`
  - `flashgrep-x86_64-unknown-linux-musl`
  - `flashgrep-aarch64-pc-windows-msvc.exe`
  - `flashgrep-x86_64-pc-windows-msvc.exe`

## 关键修复

### 缺失索引时立即返回搜索结果

首次打开尚未建立索引的仓库时，`search_code` 会在后台启动 base snapshot 构建，并立即
通过 transition backend 返回结果，不再等待完整索引构建完成。

transition `rg` 改为流式读取。达到 `global_max_results` 后会主动终止子进程，避免高频
匹配在大型仓库中继续消耗 CPU 和内存。

### 同一仓库的 MCP 进程共享 daemon

同一 Git worktree 的多个 MCP session 现在通过轻量 `mcp-bridge` 连接同一个 managed
daemon，共享：

- base index reader
- workspace overlay reader
- workspace watcher
- 后台构建任务

大量 subagent 并发搜索时不会再为每个 MCP 进程各自启动一套重型 daemon 和索引 reader。
MCP JSON-RPC 请求也改为并发分发，慢搜索不会阻塞无关请求。

### 跨进程索引构建串行化

base snapshot 构建和发布增加了 OS 文件锁。多个 MCP/subagent 同时请求建立同一仓库索引
时，只会有一个实际构建者；重复请求复用已有活动任务，避免 staging 清理和发布竞争。

### 子目录继续复用 Git worktree 根索引

从仓库子目录启动 MCP 时，会使用 canonical Git worktree root 作为 repo identity 和索引
存储根，同时把搜索结果限制在用户打开的子目录范围内。

## 工程质量

- `cargo clippy --all-targets --all-features -- -D warnings` 已通过。
- 完整测试：`246 passed, 0 failed, 1 ignored`。
- MCP 集成测试覆盖多个 MCP 进程共享 managed daemon、并发请求和子目录索引复用。

## Chromium 大仓库验证

在 Chromium 大型 Git worktree 上完成了冷索引和多进程验证：

- 冷索引搜索约 `442 ms` 返回 10 条 transition 结果，索引继续在后台构建。
- 4 个并发 MCP session 只启动 1 个重型 managed daemon。
- managed daemon RSS 约 `383 MB`，轻量 bridge 进程内存占用接近 0。
- 热态 daemon 搜索耗时约 `7-13 ms`。
- 从 `chromium-src/base` 打开时复用 `chromium-src` 根索引，结果保持在 `base` scope。

具体耗时受机器、仓库状态、缓存和查询模式影响，上述数字用于说明本次验证环境中的行为。

## 后续设计

本仓库新增了 branch switch 增量 base promotion 的详细改造方案：

```text
docs/branch_switch_incremental_index_plan.md
```

该方案用于后续替换切分支时的粗粒度 base rebuild；它是设计文档，不属于 `0.2.12` 已实现
功能。
