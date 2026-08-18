# flashgrep MCP 变更说明

发布时间：2026-08-18 +0800

## 本次包内容

- MCP Server 版本：`0.2.14`
- 内置 `flashgrep` 后端版本：`0.2.14`
- 打包平台：
  - `flashgrep-aarch64-apple-darwin`
  - `flashgrep-x86_64-apple-darwin`
  - `flashgrep-aarch64-unknown-linux-musl`
  - `flashgrep-x86_64-unknown-linux-musl`
  - `flashgrep-aarch64-pc-windows-msvc.exe`
  - `flashgrep-x86_64-pc-windows-msvc.exe`

本次为性能版本，只做一件事：把「打开一个已经建好索引的仓库」这件事的开销从工作树规模上
摘下来。Chromium 上的 `open_repo` 从 8.5 s 降到 0.62 s，且三项改动各自独立可验证。

## 关键改动

### 干净工作树不再每次打开都重建整个 overlay

没有脏文件的仓库此前不会写出 overlay manifest，于是下一次打开找不到可复用的东西，打出
`persisted overlay source does not match the active base` 并从头重建——Chromium 上每次
3.9 s，而且永远不会自愈。**工作树越干净越吃亏**。现在空 overlay 也会 checkpoint 一份带
source identity 的 manifest，复用条件对干净仓库同样成立。

### 启动探测合并成一次工作树遍历

`git diff` 和 `git ls-files --others` 各自遍历一遍工作树，Chromium 上合计约 5.7 s。当
HEAD 仍与 base snapshot 一致、且 corpus mode 为 `RespectIgnore` 时，改用一条
`git status --porcelain=v2 -z --branch --untracked-files=all` 同时拿到 tracked 和
untracked 路径，降到 3.79–3.97 s。HEAD 已移动、`NoIgnore`、或解析器不认识的记录形状都
回退到原来的两条命令，宁可慢也不冒返回残缺脏集的风险；指标行会标明实际走的是
`mode=status` 还是 `mode=legacy`。

### 剩下那次遍历整个移出打开路径

`open_repo` 现在只调用 `restore_persisted_workspace_overlay`：校验持久化 manifest 的
source identity 与 `HEAD` 后直接挂载，完全不碰工作树。overlay 缺失、损坏或不匹配在这里
不再是错误，只意味着延迟探测会重建它。authoritative reconcile 以 0 延迟排给 maintenance
worker。Chromium 三轮实测 **686 / 622 / 611 ms**。

**成本没有变便宜，只是换了谁来等**：后台那次 reconcile 在 Chromium 上仍需 4.9–5.6 s。
因此新的语义写进协议而不是留给调用方猜——`RepoStatus.workspace_probe_pending` 为 `true`
时表示 daemon 仍欠一次 reconcile，`dirty_files` / `phase` / 已发布 overlay 都是**最近一次
观测值**而非当前值。该字段带 `#[serde(default)]`，对着旧 daemon 编译的客户端会解码为
`false`——对那些构建来说这是正确读法，它们本来就在 `open_repo` 内部同步 reconcile。

daemon **不会**因为 pending 就把查询降级到 `rg` 全量扫描：那等于把一次性的 3.8 s 打开
成本，换成每个大仓库首次搜索的多秒扫描。它继续用恢复出来的 overlay 服务查询，由调用方
决定是否接受；需要 authoritative 结果时调用一次 `refresh_repo`。

本仓库上的完整生命周期实测：`open_repo=90ms phase=ready_dirty probe_pending=true
dirty={50,0,29}`，紧接着的 `search` 由恢复出来的 overlay 服务（`backend=
indexed_workspace_view`，28 ms）；0.4 s 后后台 probe 落地，把 dirty 修正为 `{53,0,29}`
（这就是 pending 声明的真实过期量）并清零标记；`refresh_repo=54ms → probe_pending=false`。

## 工程质量

- `cargo clippy --all-targets` 无告警，`cargo fmt --check` 干净。
- 回归覆盖在 `daemon::service::tests::lifecycle`：打开必须报告
  `workspace_probe_pending=true`、第一条 search 必须由恢复的 overlay 而不是 fallback
  backend 服务、后台 probe 落地后 dirty 与 phase 必须自行收敛。
- 完整测试在开发机（macOS，Apple M4）上为 `243 passed, 3 failed, 1 ignored`。三个失败项
  **均已在干净的 `v0.2.13` tag 上复现**，与本次改动无关：
  - `bench_rg_runner_uses_snapshot_visible_scope_for_custom_repo`——该环境中 `rg` 是 shell
    函数而非真实可执行文件，测试需要启动该可执行文件。
  - `watcher_file_events_use_git_pathspec_fast_probe` 与
    `watcher_reconciles_tracked_move_and_recreate_sequences`——macOS FSEvents 上报的是目录
    粒度事件，事件快路径拿不到具体文件名。后者在 `v0.2.13` 上 5/5 失败，带本次改动后
    3 次中有 1 次通过。
- Chromium 数字为本机（Apple M4，macOS 15.5）release 构建、热缓存下的 3 轮读数。

## 后续设计

打开路径上剩余的改进空间是设计一份可证明安全的持久化工作树指纹，让后台那次 4.9–5.6 s 的
reconcile 本身也能省掉——当前只是把它移出了关键路径。该方向尚未实现。

本仓库的 branch switch 增量 base promotion 改造方案仍位于：

```text
docs/branch_switch_incremental_index_plan.md
```

该文档为设计方案，不属于 `0.2.14` 已实现功能。
