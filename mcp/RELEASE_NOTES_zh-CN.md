# flashgrep MCP 变更说明

发布时间：2026-08-18 +0800

## 本次包内容

- MCP Server 版本：`0.2.15`
- 内置 `flashgrep` 后端版本：`0.2.15`
- 打包平台：
  - `flashgrep-aarch64-apple-darwin`
  - `flashgrep-x86_64-apple-darwin`
  - `flashgrep-aarch64-unknown-linux-musl`
  - `flashgrep-x86_64-unknown-linux-musl`
  - `flashgrep-aarch64-pc-windows-msvc.exe`
  - `flashgrep-x86_64-pc-windows-msvc.exe`

本次是可观测性修复版本，只做一件事：让后台 base 维护任务的失败**真的能被看见**。索引、
查询、打开路径的行为一律不变，没有性能改动。

## 关键改动

### 后台维护失败此前是系统性不可见的，不是偶发丢失

`advance_base_snapshot` 和 `compact_base_deltas` 失败时会把错误写进 `RepoStatus.last_error`。
问题在于这个槽位是与工作树探测**共用**的，而每次探测成功都会把它清空。探测是秒级完成的
——在 v0.2.15 之前的构建上实测约 **3.5 s**，而嵌入式客户端的空闲状态轮询间隔是 **5 s**。

也就是说，这不是「有时候会错过」：在任何以人类时间尺度轮询状态的客户端看来，失败**从来
不会出现**。错误被正确地报告了，然后在任何人有机会读到它之前被自己的探测擦掉。这类 bug
的危险之处在于它伪装成「一切正常」——维护任务反复失败、base delta 持续堆积，而状态面板
始终干净。

### 独立的粘滞槽位

新增 `RepoStatus.last_maintenance_error`：

- **写入**：上述两类后台任务中的任意一个失败时。
- **清除**：只有**同类**工作成功时才清除。`advance` 成功不会掩盖一次 `compact` 失败，
  反之亦然——两者失败的原因不同，混在一起清就等于重新引入本次要修的那个问题。
- **工作树探测永远不碰它。** 这是这次修复的全部要点，所以它被写成不变量而不是注释。

`last_error` 的语义不变，仍然是「探测与任务共用的最近一次错误」。新字段是补充，不是替换
——要的是让维护失败**多一个不会被擦掉的去处**，而不是改动既有调用方读到的内容。

### 线上兼容

wire 字段带 `#[serde(default, skip_serializing_if = "Option::is_none")]`：

- 早于 v0.2.15 的 daemon 不发这个字段，新客户端解码为 `None`。对那些构建来说这是**正确
  读法**——在它们上面，这个失败是真的观测不到，`None` 如实反映了这一点，而不是「没有失败」
  的断言。
- 早于本版本的客户端对着新 daemon 也照常工作：多出来的字段不参与它们的反序列化。

## 工程质量

- `cargo clippy --release --all-targets` 无告警，`cargo fmt --check` 干净。
- 回归测试 `base_maintenance_failure_outlives_the_probes_that_clear_last_error`
  （`daemon::service::tests::branch_switch`）：**不带修复时会失败**
  （等待槽位被填充直至超时），带修复时通过。测试特意让探测在维护失败之后继续跑，断言的正是
  「探测清不掉它」这条不变量，而不只是「字段被写过一次」：它只回收 base snapshot 目录的写
  权限，overlay 在同级目录因而探测持续成功，正是当年会擦掉错误的那个条件。
- 完整测试在开发机（macOS，Apple M4，release 构建）上为 `244 passed, 3 failed, 1 ignored`。
  三个失败项与 v0.2.14 记录的是同一批，**均已在干净的 `v0.2.13` tag 上复现**，与本次改动
  无关：
  - `bench_rg_runner_uses_snapshot_visible_scope_for_custom_repo`——该环境中 `rg` 是 shell
    函数而非真实可执行文件，测试需要启动该可执行文件。
  - `watcher_file_events_use_git_pathspec_fast_probe` 与
    `watcher_reconciles_tracked_move_and_recreate_sequences`——macOS FSEvents 上报的是目录
    粒度事件，事件快路径拿不到具体文件名。
- 3.5 s 那个探测时长是在 v0.2.14 打包二进制上、由嵌入该 daemon 的客户端侧实测得到的，不是
  估算；5 s 轮询间隔是该客户端的既有配置。

## 后续设计

打开路径上剩余的改进空间与 v0.2.14 所述相同：设计一份可证明安全的持久化工作树指纹，让后台
那次 reconcile 本身也能省掉——当前只是把它移出了关键路径。该方向尚未实现。

本仓库的 branch switch 增量 base promotion 改造方案仍位于：

```text
docs/branch_switch_incremental_index_plan.md
```

该文档为设计方案，不属于 `0.2.15` 已实现功能。
