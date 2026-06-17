# Git-Only Workspace Runtime Plan

## Decision

daemon 的 indexed workspace runtime 只支持 Git worktree。

这里的 indexed workspace runtime 指：

- `open_repo` / `ensure_repo`
- base snapshot build / rebuild
- dirty catalog
- workspace overlay
- daemon indexed search 的 `base snapshot + published overlay snapshot` 视图

非 Git 目录不再进入这条路径。它们可以继续由一次性扫描、`rg` 风格搜索或更低层的本地工具 API 处理，但不承担 daemon 的 base / overlay 增量索引语义。

这个决策的目标是把 workspace runtime 收敛到一个清晰模型：

```text
Git HEAD snapshot + current Git worktree delta
```

而不是同时维护：

```text
Git HEAD delta
repo fingerprint delta
watcher observed path delta
full filesystem scan delta
non-Git polling fallback
```

## Motivation

overlay 的语义天然是“当前工作区相对某个稳定 base snapshot 的补丁层”。Git 提供了这个 base 的身份和差异边界：

- `HEAD` commit 可以作为 base snapshot identity。
- `git diff <base_head>` 可以表达 tracked 文件相对 base 的变化。
- `git ls-files --others --exclude-standard` 可以表达未跟踪但可见的新增文件。
- `.gitignore` / exclude 规则由 Git 自己解释，避免 runtime 维护另一套近似规则。

非 Git 目录只能把“某次扫描结果”当成 base。后续为了保持正确性，需要额外处理：

- watcher 丢事件后的全量扫描
- 目录删除和 rename 的路径扩展
- ignore 规则变化
- hidden / max file size / binary probe 与旧 snapshot 的不一致
- mtime 和 content hash 的兜底比较
- 非 Git polling 间隔和 Git polling 间隔两套策略

这些分支会继续抬高维护成本，也容易让未来实现误以为这些路径是同等重要的主线。

## Scope

### In Scope

- daemon 打开的 repo 必须是非 bare Git worktree。
- `repo_id` 使用 canonical Git worktree root。
- base snapshot 必须锚定 Git `HEAD` commit 和 config fingerprint。
- workspace dirty detection 使用 Git 作为唯一正确性来源。
- watcher 只作为低延迟唤醒信号，不作为非 Git 兼容机制。
- 非 Git daemon open 明确失败，并给出可操作错误信息。
- 默认 `storage_root` / managed daemon state file 必须基于 discovered Git top-level 派生，避免同一 worktree 的子目录输入生成不同 runtime。
- daemon indexed runtime 继续支持 `respect_ignore` / `no_ignore` 配置，但两者都必须走 Git dirty probe，不回退到 filesystem full scan。

### Out Of Scope

- 不在本方案中删除所有低层 `scan_paths` / `SearchEngine` 能力。
- 不要求 CLI 的一次性搜索必须依赖 Git。
- 不承诺支持 bare repository。
- 不把未发布的 dirty catalog 作为查询真相源。

## External Behavior

### `open_repo` / `ensure_repo`

输入路径先通过 Git discovery 归一化：

```text
git -C <input> rev-parse --is-inside-work-tree
git -C <input> rev-parse --is-bare-repository
git -C <input> rev-parse --show-toplevel
git -C <input> rev-parse --verify HEAD^{commit}
```

目标行为：

- 如果不在 Git worktree 内，返回协议错误。
- 如果是 bare repo，返回协议错误。
- 如果 repo 还没有 `HEAD` commit，返回协议错误。
- 如果输入是 repo 子目录，daemon 归一化到 Git top-level；调用方需要用 query scope 限制子目录搜索。

推荐错误信息：

```text
indexed daemon workspace requires a Git worktree with a HEAD commit: <path>
```

### Repository Identity

`repo_id` 使用 canonical Git top-level 路径。

这避免同一个 worktree 通过不同子目录、symlink 或相对路径被打开成多个 runtime。

Git discovery 必须发生在这些派生之前：

- 默认 `storage_root`
- managed daemon state file path
- `RepoManager` 的 runtime map key
- `BuildConfig::normalized()` 后传入 runtime 的 `repo_path`

如果调用方没有显式传入 `storage_root`，默认值必须是：

```text
<canonical-git-top-level>/.flashgrep-index-engine
```

如果调用方显式传入 `storage_root`，保留该路径的含义，只做 normalizing / internal path exclusion。若它位于 worktree 内，仍需要从 Git pathspec 和 filesystem scanner 中排除；若它位于 worktree 外，不应尝试写 repo 的 `.gitignore`。

实现上不要再用 `repo_path.join(".git").exists()` 判断 Git runtime。linked worktree 的 `.git` 可能是文件，后续逻辑应复用 discovery 得到的 `git_dir` / `common_git_dir` 或显式的 `GitWorkspace` 状态。

### Snapshot Identity

base snapshot key 只使用 Git identity：

```text
base-git-<head_commit>-<config_fingerprint>
```

repo fingerprint fallback 不再用于 daemon indexed runtime。

这个方案不要求迁移持久化 snapshot key 格式；先沿用当前 `base-git-...` 格式，避免把 runtime 收敛和 on-disk format migration 绑在一起。

## Target Runtime Model

```text
Git worktree root
        |
        v
+------------------+
| Base Snapshot    |
| HEAD + config    |
+--------+---------+
         |
         v
+------------------+       filesystem event
| Git Dirty Probe  | <----------------+
| diff + untracked |                  |
+--------+---------+                  |
         |                            |
         v                            |
+------------------+                  |
| Dirty Catalog    |                  |
+--------+---------+                  |
         |                            |
         v                            |
+------------------+                  |
| Overlay Writer   |                  |
| live + deletes   |                  |
+--------+---------+                  |
         |                            |
         v                            |
+------------------+                  |
| Published View   |                  |
| base + overlay   |                  |
+------------------+                  |
```

查询仍然只读取：

```text
base snapshot + published overlay snapshot
```

dirty catalog 只负责调度、恢复和状态展示。

## Dirty Detection

### Correctness Source

dirty 集合由 Git 计算：

```text
git diff --name-status -z --no-renames <base_head> -- .
git ls-files -z --others --exclude-standard -- .
```

使用 `--no-renames` 的原因是让 rename 表达成 delete + add，和 overlay 的 live document / delete tombstone 模型一致。

`corpus_mode` 的 Git 命令差异：

- `respect_ignore`
  - tracked 候选：`git diff --name-status -z --no-renames <base_head> -- . <storage-exclude-pathspec>`
  - untracked 候选：`git ls-files -z --others --exclude-standard -- . <storage-exclude-pathspec>`
- `no_ignore`
  - tracked 候选同上。
  - untracked 候选不加 `--exclude-standard`，仍然通过 `<storage-exclude-pathspec>` 排除 flashgrep 自身存储。

Git 命令输出先作为 repo-relative logical path 解析；dirty catalog / overlay 现有结构如果继续存 absolute path，需要在同一个集中函数里转换，catalog lookup 一律使用 logical path。

`--name-status -z` 解析要求：

- `M` / `T` / mode change 等当前存在的 tracked path 进入 current path classifier。
- `D` 进入 deleted candidate。
- `A` 进入 current path classifier。
- 使用 `--no-renames` 后不需要维护 rename 兼容分支；rename 由 Git 表达为 delete + add。

每个候选路径再根据当前文件系统和 base catalog 分类：

- base 有、当前不存在：`deleted`
- base 有、当前不再属于 corpus：`deleted`
- base 有、当前内容不同：`modified`
- base 没有、当前属于 corpus：`new`
- 其他情况：clean

这里的“属于 corpus”只检查 daemon 本地配置能决定的条件：

- 路径仍在 worktree 内，且不是 flashgrep internal storage。
- 普通文件。
- hidden 策略。
- max file size。
- binary / text probe。

不要在 classifier 里再用 filesystem scan 重新解释 `.gitignore`。ignore 可见性已经由 Git 候选集合决定；tracked 文件不应因为后续 `.gitignore` 规则变化被本地 scanner 误判为 deleted。

Git 命令失败不再返回 `None` 触发 scan fallback。daemon Git runtime 内，Git probe 失败就是 probe failure：标记 unhealthy / degraded 或返回 protocol error，由上层维护循环重试。

### Probe Failure Behavior

Git probe 失败时不发布推测出来的新 overlay，也不清空上一份已发布 view。

运行中 probe 失败的默认行为：

- 保留上一份已成功发布的 `base snapshot + overlay snapshot`，作为 last-known-good 查询视图。
- 不更新 dirty catalog 为新的推测集合。
- 不发布新的 overlay snapshot。
- status 标记 `probe_healthy=false`，记录 `last_error`，并由 maintenance worker 后续重试。
- 如果失败说明 repo 已经不可用或状态无法继续解释，可以进入 `Degraded`，indexed search 返回明确错误。

这里的 last-known-good 指最后一次 Git probe 成功、overlay 构建成功并原子发布的查询视图。它可能不是最新工作区状态，但它比 Git 失败后用 filesystem scan 拼出的混合语义结果更可控。

`open_repo` / `ensure_repo` 的 discovery 失败不属于 last-known-good 场景；没有有效 Git worktree / bare repo / 无 `HEAD` commit 时，应直接返回协议错误。

### HEAD Changes

每次 dirty probe / reconcile 都要读取当前 `HEAD`，并和 base snapshot metadata 中的 `head_commit` 比较。

如果当前 `HEAD` 和 base `HEAD` 不一致：

- 继续可以用 `git diff <base_head>` 计算相对旧 base 的 overlay，以保持查询视图代表当前 worktree。
- 同时必须标记 base snapshot identity stale，并调度当前 `HEAD + config` 的 base rebuild。
- `ensure_repo` 不能只看“是否存在任意 base snapshot”；如果现有 snapshot identity 和当前 `HEAD + config` 不匹配，应同步 rebuild 或返回需要 rebuild 的状态。
- rebuild 发布后，清理旧 dirty catalog / overlay，并重新用当前 base probe 一次。

这样运行中的 commit / checkout / rebase 不会把 daemon 长期停留在旧 base 上，也不需要引入非 Git fallback。

### Watcher Role

watcher 继续使用 `notify` 递归监听 worktree，但它只做两件事：

1. 过滤 internal storage path。
2. 调度一次尽快执行的 Git dirty probe。

正常文件/目录事件会记录一个 bounded candidate path set，用这些 path 作为 Git pathspec 缩小 dirty probe 范围。这个 fast path 仍然只相信 Git 输出；watcher path 只是 Git probe 的 scope，不直接决定 dirty 集合。

以下事件会升级到 full Git probe：

- `.git` / Git metadata 变化。
- `.gitignore` / `.gitmodules` / `.gitattributes` 变化。
- repo root only event。
- candidate path 数量超过上限。
- watcher backend error。

即使 watcher 丢事件，低频 full Git reconcile 也会恢复正确性。

### Polling / Reconcile

只保留 Git worktree 的 reconcile 策略：

- watcher active：低延迟事件触发 + 低频周期性 full Git reconcile。
- watcher inactive：较短周期 Git poll fallback。

删除 non-Git poll interval。

带明确非 repo-root scope roots 的 search 会先执行一次 scoped Git pathspec probe，再读取 published view。这用于避免 macOS FSEvents 聚合延迟影响“搜索刚编辑的单个文件”这类交互路径。

## Overlay Construction

overlay 构建语义保持不变：

- `modified` / `new` 路径读取当前 worktree 内容，生成 live document。
- `deleted` 路径生成 delete key。
- 如果 `modified` 文件当前无法读成 corpus 文档，生成 delete key。
- published overlay shadow base 中相同 logical path 的结果。

逻辑视图仍是：

```text
workspace = (base - overlay_shadowed_paths) + overlay_live_docs
```

区别在于 dirty 集合只来自 Git dirty probe，不再从非 Git full scan 推导。

## Implementation Plan

### Phase 1: Git Discovery Gate

新增一个集中入口，例如 `GitWorkspace::discover(path)`：

- 校验 `is-inside-work-tree`。
- 解析 canonical top-level。
- 校验非 bare。
- 校验 `HEAD^{commit}` 存在。
- 返回 `worktree_root`、`head_commit`、`git_dir`、`common_git_dir`。

`OpenRepoRequest` / `EnsureRepoRequest` 进入 `RepoRuntime::new` 前先完成 discovery，并把 `build_config.repo_path` 归一化为 worktree root。

这一步还要负责重新派生默认路径：

1. 先 discover 输入路径得到 canonical Git top-level。
2. 如果 `storage_root` 未显式传入，用 Git top-level 下的 `.flashgrep-index-engine`。
3. 基于这个 normalized build config 派生 managed daemon state file。
4. 使用 Git top-level 作为 `repo_id` 和 runtime map key。

### Phase 2: Base Snapshot Identity 收敛

daemon build / rebuild 只允许 Git snapshot mode：

- snapshot key 使用 `head_commit + config_fingerprint`。
- daemon 不再为 repo fingerprint fallback 创建 base snapshot。
- base catalog 中的 path 继续使用 repo-relative logical path。
- daemon 调用 base snapshot build 时必须通过类型或 option 显式要求 `head_commit` 存在；不能仅依赖前置 discovery，避免底层 `head_commit: None` 的 scan branch 被误触发。
- 打开已存在 snapshot 时必须校验 metadata 中的 `head_commit`、`config_fingerprint`、`repo_root` 都和当前 discovery / config 一致。

库层如果仍保留 repo fingerprint fallback，需要明确标注为 daemon 外的低层能力。

### Phase 3: Dirty Probe 收敛

替换 daemon dirty detection 为单一 Git probe：

- tracked 候选来自 `git diff --name-status -z --no-renames <base_head> -- .`
- untracked 候选来自 `git ls-files -z --others --exclude-standard -- .`
- 候选路径统一走 current path classifier。
- `respect_ignore` / `no_ignore` 只影响 untracked 候选是否加 `--exclude-standard`；不能重新启用 non-Git scan diff。

删除 daemon 中这些分支：

- non-Git full scan dirty diff
- non-Git observed path diff
- repo fingerprint dirty comparison
- non-Git workspace poll intervals
- Git command error 后的 scan fallback
- classifier 内用于重新解释 ignore 规则的 per-path scanner

### Phase 4: Watcher Fast Path

watcher 保留路径过滤和 debounce / schedule 行为，并记录 bounded candidate paths。

`probe_workspace_overlay_from_events` 消费 event buffer 后：

- 如果事件需要 full probe，执行 full Git dirty probe。
- 如果有 candidate paths，执行 Git pathspec scoped dirty probe。
- 如果没有可用 candidate，退回 full Git dirty probe。

这个 pathspec fast path 只缩小 Git 命令范围，不重新引入 non-Git observed-path classifier。周期性 full Git reconcile 继续作为 watcher 丢事件后的兜底。

### Phase 5: Protocol And Docs

更新 daemon protocol 文档：

- `open_repo` 要求 Git worktree。
- `repo_id` 是 Git top-level。
- 非 Git 错误是预期行为。
- 默认 `storage_root` 基于 Git top-level，而不是调用方传入的子目录。
- `ensure_repo` 会校验当前 `HEAD + config` identity，不只检查是否已有任意 snapshot。

更新 workspace architecture 文档：

- base snapshot 不再描述为 `Git HEAD or repo fingerprint`。
- daemon compatibility boundary 明确是 Git-only indexed workspace runtime。

更新 MCP 文档 / 行为：

- MCP `initialize` 不应同步调用 `ensure_repo` 或 build index。
- 默认 repo 不合法时，`tools/list` 和 initialize 仍应成功；具体工具调用在 `open_repo` / `ensure_repo` 时返回 Git worktree 错误。
- 如果用户需要搜索非 Git 目录，MCP 需要单独的一次性 scan/rg 工具入口；现有 indexed repo tools 不再通过 `allow_scan_fallback` 绕过 Git discovery。

## Performance Acceptance

在删除 observed-path dirty diff 前，需要用大仓库验证 Git dirty probe 的成本。

最低验收：

- clean worktree probe 不应明显慢于当前周期性 reconcile。
- 单文件修改后的 overlay publish 延迟需要保持在交互可接受范围。
- watcher active 时不能因为连续保存触发无限并发 Git probe；maintenance worker 仍需串行合并事件。

如果 Git dirty probe 在超大仓库上不可接受，允许继续扩展 Git-aware fast path，但它必须仍以 Git 为正确性边界。例如：

- watcher 只收集候选路径。
- 候选路径用 `git check-ignore` / `git ls-files --error-unmatch` / base catalog 分类。
- 周期性 full Git reconcile 负责兜底。

这个 fast path 只能是 Git runtime 内的性能优化，不恢复非 Git 兼容分支。

## Migration Notes

### Tests

需要调整测试分层：

- daemon service / overlay / lifecycle 测试使用真实 Git fixture。
- 非 Git fixture 只保留在低层 scan/search API 测试中。
- 对非 Git `open_repo` 增加明确失败测试。
- 对 repo 子目录输入增加测试：返回的 `repo_id`、默认 `storage_root`、managed state file 都落到同一个 Git top-level。
- 对 linked worktree 增加测试：不要依赖 `.git` 是目录。
- 对空 Git repo 增加明确失败测试。
- 对 `respect_ignore` / `no_ignore` 的 untracked 文件分别测试，确保两者都不走 filesystem full scan fallback。
- 对运行中 commit / checkout 后的 stale base rebuild 增加测试。
- 对 MCP initialize 增加测试：initialize 不打开 repo；工具调用才报告 Git worktree 错误。

### Compatibility

这是 daemon indexed runtime 的 breaking change。

集成方如果传入普通目录，会收到明确错误；解决方式是：

- 在 Git repo 内调用 daemon。
- 或使用 CLI / 库层的一次性 scan / rg 搜索入口。

现有 daemon indexed repo endpoints 不再承诺为非 Git 目录提供 `allow_scan_fallback`。如果 MCP 或其他 daemon 集成需要非 Git 搜索，应新增独立 endpoint / tool，而不是让 indexed repo runtime 重新绕过 Git discovery。

### Rollout

建议先落 Phase 1 的 Git discovery gate 和错误文档，再删除内部非 Git 分支。

这样可以先固定外部 contract，后续代码清理不会再受兼容承诺牵制。

## Open Questions

1. 空 Git repo 是否要支持空 base snapshot？

   建议第一版不支持。没有 `HEAD` commit 时，base identity 不稳定，收益很低。

2. 输入子目录是否要作为默认搜索 scope？

   建议不做。daemon runtime 以 Git top-level 为 repo identity，子目录搜索交给 query scope。

3. linked worktree 是否支持？

   建议支持。只要 discovery 能返回有效 `show-toplevel`、`git_dir`、`common_git_dir` 和 `HEAD^{commit}`，runtime 不需要区分普通 worktree 和 linked worktree；实现不能假设 `.git` 一定是目录。
