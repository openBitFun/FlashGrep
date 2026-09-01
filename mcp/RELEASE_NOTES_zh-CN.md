# flashgrep MCP 变更说明

发布时间：2026-09-01 +0800

## 本次包内容

- MCP Server 版本：`0.2.16`
- 内置 `flashgrep` 后端版本：`0.2.16`
- 打包平台：
  - `flashgrep-aarch64-apple-darwin`
  - `flashgrep-x86_64-apple-darwin`
  - `flashgrep-aarch64-unknown-linux-musl`
  - `flashgrep-x86_64-unknown-linux-musl`
  - `flashgrep-aarch64-pc-windows-msvc.exe`
  - `flashgrep-x86_64-pc-windows-msvc.exe`

本次版本修复长时间运行和首次构建大型 Git 仓库时的文件句柄耗尽问题，同时保持索引格式、
查询语义和结果不变。

## 关键改动

### Git blob reader 改为共享有界池

旧实现让每个 Git-backed segment reader 各自持有一个 `git cat-file --batch` 子进程。每个
子进程还会保留 stdin、stdout、stderr 三条管道；segment 数量增长后，daemon 的常驻句柄会
线性增长，并在 macOS 常见的 256 软上限下触发 `Too many open files (os error 24)`。

新实现让同一个 segment store 中的 reader 共享一个惰性创建、有界的 reader 池：

- 默认最多四个 `git cat-file --batch` reader；
- 可通过 `FLASHGREP_GIT_BLOB_READER_POOL_SIZE` 调整；
- 配置值最多为十六，避免重新引入无界资源增长；
- 没有 Git-backed 文档读取时不会提前启动子进程。

### posting bucket 写入不再常驻打开全部文件

首次构建 base snapshot 时，每个并行 shard 都可能落盘到 64 个 posting bucket。旧实现会
同时保持全部 bucket 文件打开，多个 shard 并行时可瞬间占用数百个文件句柄。

新实现为每个 bucket 使用 64 KiB 内存缓冲，达到阈值后临时打开对应文件、批量 append 并
立即关闭。每个 shard 的缓冲上限约 4 MiB，不改变落盘记录格式，也不会因为限制句柄而逐条
重开文件。

## 验证

- 在 macOS 将 BitFun 桌面进程的软文件句柄上限固定为 256，恢复并完整索引 Chromium：
  - 444,153 个文档；
  - 16 个 base shard；
  - 约 82 秒完成发布；
  - flashgrep 实际 FD 峰值 68，posting bucket FD 峰值 6；
  - 未出现 `os error 24`。
- 固定字符串、符号串和正则查询均通过 indexed backend；四组代表性查询与 `rg` 对照时，
  匹配文件数和匹配行数完全一致。
- `cargo clippy --all-targets -- -D warnings` 通过。
- 34 个 E2E、5 个 managed-daemon、5 个 MCP 集成测试全部通过。
- 单元测试中 249 项通过、1 项为既有 ignored；一个 macOS watcher 时序用例在并发全量运行
  时偶发失败，单独重跑通过，与本次 Git reader 和 posting bucket 改动无关。
