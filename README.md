# flashgrep

本项目是一个本地 regex 预筛索引系统骨架，目标是把全库正则搜索拆成两段：

1. 先用字符级倒排索引召回候选文件。
2. 再只对候选文件执行真实 regex 验证。

当前实现采用：

- Git snapshot 或 repo fingerprint 驱动的 `base snapshot`
- `dirty catalog` 跟踪工作区变化
- `published workspace overlay` 表达当前 worktree 的已发布增量视图
- 保守型 regex 查询规划器
- 候选召回 + `regex` 精确验证
- daemon 驱动的 build / refresh / search / glob 协议

## 模块布局

- `src/cli.rs`
  命令行入口；当前只保留服务进程、daemon 管理和 build/bench 运维子命令。
- `src/files.rs`
  仓库扫描、文本过滤、基础文件元数据采集。
- `src/index_engine/`
  base snapshot、workspace overlay、manifest、segment、reader、merge 的核心实现。
- `src/daemon/`
  稳定外部协议、repo runtime、状态管理、任务调度。
- `src/workspace.rs`
  专家向本地 workspace facade，用于按需打开当前 workspace 视图。
- `src/search_engine.rs`
  简化的 build/search facade，适合本地工具和测试。
- `src/tokenizer/`
  token 抽取策略，当前含 trigram 和 sparse n-gram。
- `src/planner.rs`
  regex 查询规划，先做保守提取，不覆盖完整语义。
- `src/search.rs`
  搜索结果类型定义。

## 存储布局

repo 默认把索引数据落在 `.flashgrep-index-engine/`：

- `base-snapshot/`
  只读基线 snapshot，按 snapshot key 分目录保存。
- `workspace-overlay/`
  已发布 overlay shard，使用 manifest + segment + delete segment 结构。
- `dirty-catalog.json`
  跟踪工作区 dirty/building/ready/failed 状态。

## 快速开始

```bash
cargo run --bin flashgrep -- serve
```

推荐把 `flashgrep` 当成一个独立常驻进程来使用，形态更接近本地搜索 server，而不是 ripgrep 风格的一次性搜索 CLI。

如果你使用 `ManagedDaemonClient`：

- daemon 默认按 `index_path/daemon-state.json` 发现和复用。
- 多个调用方会通过同一路径下的 `daemon-state.lock` 协调启动，避免重复拉起。
- 调用方通常不需要自己再包一层自定义 daemon 生命周期管理器。

常用运维/调试入口：

```bash
# 前台拉起 server
cargo run --bin flashgrep -- serve --bind 127.0.0.1:4597

# 注册或打开一个 repo
cargo run --bin flashgrep -- daemon open --repo /path/to/repo

# 对 repo 建索引
cargo run --bin flashgrep -- daemon build --repo-id /path/to/repo

# 通过 daemon 发搜索请求
cargo run --bin flashgrep -- daemon search --repo-id /path/to/repo 'foo.*bar'

# 通过 daemon 做路径 glob / scope 解析
cargo run --bin flashgrep -- daemon glob --repo-id /path/to/repo -g '*.rs' src

# 查询 repo 状态
cargo run --bin flashgrep -- daemon status --repo-id /path/to/repo

# 关闭 repo，或关闭整个 server
cargo run --bin flashgrep -- daemon close --repo-id /path/to/repo
cargo run --bin flashgrep -- daemon shutdown
```

## MCP 集成

如果你想把 `flashgrep` 作为 MCP server 暴露给别的项目，而不改宿主源码，可以直接启动：

```bash
# Node MCP server，固定 repo
node src/mcp/server.js --repo /path/to/repo

# 也可以显式指定索引目录或 flashgrep daemon 二进制
node src/mcp/server.js --repo /path/to/repo --storage-root /tmp/flashgrep-index
node src/mcp/server.js --repo /path/to/repo --daemon-bin /path/to/flashgrep
```

这个 MCP server 当前暴露 5 个 tools：

- `search_code`
  对 repo 执行 flashgrep 搜索；不会隐式建索引。如果 repo 还没有 base snapshot，会直接报错，要求先调用 `build_index`，或者显式允许 `allow_scan_fallback`。
- `glob_files`
  按 ignore/glob/type 规则返回当前可见文件。
- `repo_status`
  返回 daemon 里的 repo 状态。
- `refresh_repo`
  触发 workspace refresh。
- `build_index`
  显式确保 repo 已完成基础索引构建。

典型 MCP 配置示例：

```json
{
  "mcpServers": {
    "flashgrep": {
      "command": "node",
      "args": [
        "/absolute/path/to/flashgrep/src/mcp/server.js",
        "--repo",
        "/path/to/repo"
      ]
    }
  }
}
```

说明：

- 如果启动时已经传了 `--repo`，tool 调用里可以省略 `repo_path`。
- 如果没有传 `--repo`，每次 tool 调用都需要带 `repo_path`。
- MCP transport 走 stdio，framing 使用 `Content-Length`。
- MCP server 自身是 Node 进程；它会在本次 MCP session 内拉起并持有一个 `flashgrep serve --stdio` 后端子进程。
- 如果没显式传 `--daemon-bin`，server 会优先尝试 `target/release/flashgrep`、`target/debug/flashgrep`，最后回退到环境里的 `flashgrep`。

如果只是离线准备索引或做 bench，也保留这两个入口：

```bash
# 通过 managed daemon 自动拉起/连接 server，并确保 repo 已可搜索
cargo run --bin flashgrep -- build --repo /path/to/repo

# benchmark 仍是本地运维工具入口
cargo run --bin flashgrep -- bench --suite-dir bench_data
```

当前 CLI 语义：

- 默认入口不再提供 ripgrep 风格的一次性搜索命令。
- 搜索相关命令通过 `daemon search` 暴露，面向服务进程调试和手工排障。
- 路径枚举/glob 相关能力通过 `daemon glob` 暴露。
- `serve` 用于拉起 TCP server，`daemon *` 用于向该 server 发协议请求。
- `build` 现在也会通过 managed daemon 走进程协议，不再直接在 CLI 进程里调用建索引库函数。

## External Interface

当前稳定的对外接口应视为 daemon 进程协议，以及围绕它的二进制与基础客户端：

- daemon protocol request/response/notification schema
- `DaemonClient` / `ManagedDaemonClient`
- `flashgrep serve` 与 `flashgrep daemon *` 这些围绕 daemon 的工具入口

其中：

- `daemon::protocol` 是跨语言/跨进程集成时的规范基线。
- 下游产品应自己在客户端侧封装 SDK / service facade；`flashgrep` 不再内置 Rust SDK facade。
- `flashgrep` 这边的交付物以 daemon 二进制 `flashgrep` 为主；像 BitFun 这类宿主只需要打包 `flashgrep` 并按协议接入。
- crate 根上仍然保留若干库导出，主要服务于本地工具、测试和专家向集成，不应当作稳定产品接入面。

入口选择（推荐）：

- 外部集成 / IDE / agent / 宿主产品：以 daemon 协议为准。
- 需要自动拉起和复用 daemon 的 Rust 调用方：使用 `ManagedDaemonClient`。
- 需要直接发协议请求的调用方：使用 `DaemonClient` + `daemon::protocol::*`。

路径边界约定：

- `flashgrep` 只把自己的产物目录 `.flashgrep-index`、`.flashgrep-bench`，以及调用方显式配置的 `index_path` 视为内部路径。
- `flashgrep` 不会内建保留某个宿主产品目录名；像 `.bitfun`、`.idea`、`.vscode` 这类目录是否排除，应由接入方或 ignore 规则决定。

最小接入示例：

```rust
use flashgrep::daemon::{
    protocol::{
        EnsureRepoParams, PathScope, QuerySpec, RepoConfig, Request, SearchModeConfig,
        SearchParams,
    },
    DaemonClient, ManagedDaemonClient,
};

let managed = ManagedDaemonClient::new();
let ensured = managed.ensure_repo(EnsureRepoParams {
    repo_path: "/path/to/repo".into(),
    index_path: None,
    config: RepoConfig::default(),
    refresh: Default::default(),
})?;

let client = DaemonClient::new(ensured.addr);
let response = client.send(Request::Search {
    params: SearchParams {
        repo_id: ensured.repo_id,
        query: QuerySpec {
            pattern: "LLVMContext".into(),
            patterns: vec!["LLVMContext".into()],
            case_insensitive: false,
            multiline: false,
            dot_matches_new_line: false,
            fixed_strings: true,
            word_regexp: false,
            line_regexp: false,
            before_context: 0,
            after_context: 0,
            top_k_tokens: 6,
            max_count: None,
            global_max_results: None,
            search_mode: SearchModeConfig::CountOnly,
        },
        scope: PathScope::default(),
        allow_scan_fallback: false,
    },
})?;
```

如果宿主产品只需要打包资源：

- 打包 `flashgrep` 二进制到产品资源目录。
- 启动时通过 `FLASHGREP_DAEMON_BIN` 或显式路径把二进制位置传给调用侧。
- 客户端自己管理更高层的 repo session、事件订阅、重试策略和 API facade。

核心职责边界（固定定义）：

- `daemon::protocol`：唯一稳定外部 contract。
- `DaemonClient` / `ManagedDaemonClient`：daemon protocol 的低层 transport/client 封装。
- 其他 crate library API：内部实现、测试、benchmark 和专家向辅助，不作为稳定外部接口。

## Internal Library Notes

crate 根上仍然保留若干库导出，主要用于：

- daemon 内部实现
- benchmark / fixture / 测试代码
- 专家向本地集成和调试

这些库接口不是推荐的外部接入方式；新的外部能力优先补到 daemon protocol，而不是继续扩展库层 facade。

## Daemon Overlay

daemon 运行时维护两类 workspace 状态：

- `dirty catalog`
  跟踪当前工作区相对 base snapshot 的变化集合和构建状态。
- `published overlay`
  只保存已经构建完成并原子发布、对查询可见的增量层。

这意味着：

- 查询只读取 `base snapshot + published overlay`。
- 未发布 overlay 在进程重启后允许丢失，daemon 会通过 startup rescan 补回。
- 查询统一使用 `WorkspaceEventual`，直接读取当前 `base snapshot + published overlay` 视图。

## Benchmark

当前项目已集成一版兼容 ripgrep `benchsuite` 命名方式的 benchmark runner。
benchmark 的 `flashgrep` 结果只统计上层 daemon/service 查询路径；不再提供底层 direct benchmark 口径。

语料目录默认按下面布局准备：

- `bench_data/linux/`
- `bench_data/subtitles/en.sample.txt`
- `bench_data/subtitles/ru.txt`

运行示例：

```bash
cargo run --bin flashgrep -- bench --suite-dir /Users/user/workspace/flashgrep/bench_data --filter linux_literal --rebuild
```

如果希望顺带对比 `rg`：

```bash
cargo run --bin flashgrep -- bench --suite-dir /Users/user/workspace/flashgrep/bench_data --filter subtitles_en_literal --compare-rg --rebuild
```

如果希望 dirty worktree 更贴近“只改了几份文件”的 agent 场景，可以给 worktree fixture 加一个文件数上限。这个模式会从完整语料里抽一个保留真实目录结构的小型子仓库，并保证包含当前 benchmark 需要命中的文件：

```bash
cargo run --bin flashgrep -- bench \
  --suite-dir /Users/user/workspace/flashgrep/bench_data \
  --filter linux_literal \
  --compare-worktree \
  --worktree-sample-files 32 \
  --rebuild
```

如果希望比较 cold query，可以切到 `cold` cache mode。这个模式会在每个 sample 前主动驱逐本次 benchmark 会访问到的 index / corpus 文件页缓存；如果你还想连系统级 page cache 一起清掉，可以额外提供一个 hook：

```bash
cargo run --bin flashgrep -- bench \
  --suite-dir /Users/user/workspace/flashgrep/bench_data \
  --filter linux_literal \
  --compare-rg \
  --cache-mode cold \
  --bench-iter 3 \
  --cold-hook 'sync && printf 3 | tee /proc/sys/vm/drop_caches >/dev/null'
```

说明：

- `flashgrep` 的 benchmark 会单独统计索引构建时间和查询时间。
- `--compare-worktree` 会额外跑 dirty worktree 查询路径；当前实现直接走“base snapshot + query-time dirty repair”，不再依赖持久化 overlay cache。
- `--worktree-sample-files <N>` 只影响 `--compare-worktree` 的 fixture 构造：不再复制完整目录，而是抽一个最多 `N` 个文本文件的小型真实子仓库。这个模式更适合测“少量 dirty 文件”的真实查询成本；不传时仍然保留完整仓库 worst-case。
- `rg` 对比项只统计直接搜索耗时，不包含任何建索引步骤。
- `--raw-output <path>` 会把每个 sample 以 CSV 形式落盘；除了 `runner` 原始 id 之外，还会额外写出 `runner_family` 和 `runner_mode`，方便脚本稳定地区分 `daemon_steady_state` / `dirty_first_query` / `dirty_cached_query` 等路径。
- bench 默认是 `warm` cache mode；`warmup_iter=1`、`bench_iter=3` 的默认值测出来的不是 cold query。
- bench 默认是 `trace` query mode：如果 ad-hoc benchmark 传了多个 `--pattern`，会把它们当成一条 query trace 顺序执行，而不是把同一个 query 热重复很多次；需要旧的 steady-state 语义时可显式传 `--query-mode same`。
- `cold` cache mode 默认使用 `posix_fadvise(..., DONTNEED)` 驱逐本次 benchmark 文件集的 page cache；额外的 `--cold-hook` 适合在 Linux 上配合 `drop_caches` 做更严格的系统级 cold run。
- 当前 Linux 语料只要求仓库已 clone，不要求本机必须能成功构建内核。

## 当前边界

- 只索引 UTF-8 文本文件。
- 查询规划器优先保证正确性，复杂表达式会降级到弱查询或全量候选。
- base snapshot 现在已经按 `snapshot_key` 缓存：Git 仓库使用 `HEAD commit + config fingerprint`，非 Git 目录使用稳定 fallback key。
- dirty Git worktree 在 base snapshot 缺失时会按 `HEAD` materialize 一份只读 base，不会把脏工作树直接写进 commit base。
- workspace 查询默认使用“base snapshot + 当前 dirty files 扫描修补”的两层视图，不再依赖持久化 overlay cache。
- dirty files 当前仍按 `size + mtime` 探测，不是 content hash。
- 当前没有引入阈值 sidecar；dirty files 始终走 query-time repair。
- CLI 和库层都不会在后台偷偷重建 base snapshot；如果需要新的 commit base，需要显式调用 build/rebuild 接口。

## 内部库层状态

`WorkspaceIndex::ensure_base_snapshot()` / `WorkspaceIndex::status()` 暴露的 `BaseSnapshotInfo` 现在包含：

- `snapshot_key`
- `snapshot_kind`
- `head_commit`
- `config_fingerprint`

如果后续需要把 freshness 结果作为稳定外部 contract 暴露，优先补到 daemon protocol / SDK，而不是继续扩展库层 facade。

## 下一步建议

- 给查询规划器补充更完整的 AST/HIR 解析。
- 为 posting list 增加压缩和缓存。
- 把 overlay 从“按 dirty state 命中的完整小索引 + 受影响 token 级重写”继续演进到“更细粒度的字节级 patch 和跨 base 复用”。
- 加入 doc_freq 统计与更激进的 top-k token 选择器。
