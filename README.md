# codgrep

本项目是一个本地 regex 预筛索引系统骨架，目标是把全库正则搜索拆成两段：

1. 先用字符级倒排索引召回候选文件。
2. 再只对候选文件执行真实 regex 验证。

当前实现先把最小链路跑通：

- 文件级索引
- `trigram` 和 `sparse-ngram` 两种 tokenizer 接口
- `docs.bin` / `doc_terms.bin` / `lookup.bin` / `postings.bin` 四文件落盘
- 保守型 regex 查询规划器
- 候选召回 + `regex` 精确验证
- 增量建索引的第一版：复用未变文件的文档级 token 集，只重读和重分词变更文件

## 模块布局

- `src/cli.rs`
  命令行入口；默认搜索入口对齐 ripgrep 形态，保留 `build` 和 `bench` 子命令。
- `src/files.rs`
  仓库扫描、文本过滤、基础文件元数据采集。
- `src/tokenizer/`
  token 抽取策略，当前含 trigram 和 sparse n-gram。
- `src/planner.rs`
  regex 查询规划，先做保守提取，不覆盖完整语义。
- `src/index/`
  索引构建、二进制格式和查询时倒排读取。
- `src/search.rs`
  搜索结果类型定义。

## 二进制格式

- `docs.bin`
  保存 tokenizer 模式和文档元数据。
- `doc_terms.bin`
  保存每个文档去重后的 token hash 集，供增量建索引复用。
- `lookup.bin`
  保存定长 lookup entry，支持 `mmap + binary search`。
- `postings.bin`
  顺序存 posting list，使用 delta + varint 编码。

## 快速开始

```bash
cargo run -- build --repo /path/to/repo
cargo run -- --index /path/to/repo/.codgrep-index 'foo.*bar' /path/to/repo
```

常用搜索示例：

```bash
# 按 ripgrep 风格传 pattern 和 path
cargo run -- 'foo.*bar' /path/to/repo/src

# 多 pattern
cargo run -- -e 'foo.*bar' -e 'baz.*qux' /path/to/repo

# 从 pattern 文件读取
cargo run -- -f patterns.txt /path/to/repo

# 字面量搜索，对齐 rg -F
cargo run -- -F 'a+b?' /path/to/repo

# 整词 / 整行匹配
cargo run -- -w 'TODO' /path/to/repo
cargo run -- -x 'TODO' /path/to/repo

# 只输出匹配文件
cargo run -- -l 'foo.*bar' /path/to/repo

# 只输出不匹配的文件
cargo run -- --files-without-match 'foo.*bar' /path/to/repo

# 统计每个文件的匹配行数
cargo run -- -c 'foo.*bar' /path/to/repo

# 统计每个文件的总匹配次数
cargo run -- --count-matches 'foo.*bar' /path/to/repo

# 对 summary 模式保留 0，行为与 ripgrep 的 --include-zero 一致
cargo run -- -c --include-zero 'foo.*bar' /path/to/repo

# 输出 ripgrep 风格的 JSON Lines 事件流
cargo run -- --json 'foo.*bar' /path/to/repo

# 只输出命中的文本片段
cargo run -- -o 'foo.*bar' /path/to/repo

# 输出前后文，对齐 rg 的 -A/-B/-C
cargo run -- -A 2 'foo.*bar' /path/to/repo
cargo run -- -B 2 'foo.*bar' /path/to/repo
cargo run -- -C 2 'foo.*bar' /path/to/repo

# 每个文件最多输出前 2 个匹配行
cargo run -- -m 2 'foo.*bar' /path/to/repo

# 只关心是否命中，不输出内容；退出码与 ripgrep 对齐
cargo run -- -q 'foo.*bar' /path/to/repo

# 列出当前索引里会被搜索的文件
cargo run -- --files /path/to/repo

# 用 glob 和 type 缩小搜索范围
cargo run -- -g '*.rs' -t rust 'foo.*bar' /path/to/repo

# 搜索期再缩小 corpus：隐藏文件 / ignore / 深度 / 大小
cargo run -- --hidden 'foo.*bar' /path/to/repo
cargo run -- --no-ignore 'foo.*bar' /path/to/repo
cargo run -- --ignore-file extra.ignore 'foo.*bar' /path/to/repo
cargo run -- --max-depth 2 'foo.*bar' /path/to/repo
cargo run -- --max-filesize 4096 'foo.*bar' /path/to/repo
```

CLI 退出码现在与 ripgrep 保持一致：

- `0`：找到匹配；或 `--files` / `--type-list` / `build` / `bench` 成功执行。
- `1`：搜索完成但没有匹配；或 `-L/--files-without-match` 没有可输出文件。
- `2`：参数错误、索引错误或运行时失败。

当前和 `rg` 的一个显式差异：

- `-L` 现在保留给 `--follow`，但 indexed search 还没有实现该语义；如需“无匹配文件”，请使用 `--files-without-match`。
- 搜索期的 `--hidden` / `--no-ignore` 如果超出索引覆盖范围，CLI 会自动 fallback 到 scan，保证结果正确，而不是静默漏结果。
- 搜索期的 `--ignore-file` 会像 ripgrep 一样额外缩小搜索范围，并且在 `--no-ignore` 下仍然生效。
- 搜索不会在热路径里检查索引是否过期；如果工作树里有新增、删除或改动文件，是否重建索引由调用方自己决定。

库 API 现在额外提供一层 `SearchEngine`：

- `SearchPolicy::IndexOnly`：只允许走索引；索引缺失时直接返回错误。
- `SearchPolicy::FallbackToScan`：索引缺失，或查询本身明显更适合 scan 时走 `Scan`；否则直接走索引。

如果使用方希望检查索引是否过期，可以显式调用 `WorkspaceIndex::probe_freshness()` / `probe_freshness_if_due(ttl)`，或更底层的 `IndexSearcher::diff_against_worktree()` / `stale_reason()`、`SearchEngine::diff_against_worktree()` / `stale_reason()`。如果使用方希望主动刷新索引，可以显式调用 `build_index` / `rebuild_index()`，或 `build_index_with_options` / `rebuild_index_with_options`（附带进度/取消钩子）；库本身不会在搜索后偷偷调度后台重建。

`--json` 现在对齐 ripgrep 的基本约束：

- 只用于搜索结果输出，不与 `--files`、`--type-list`、`-l`、`-L`、`-c`、`--count-matches` 组合。
- 输出事件序列为 `begin` / `match|context` / `end` / `summary`。

## 库用法

当前对外 API 分三层：

- 稳定层（crate 根）：
  - `SearchEngine` / `WorkspaceIndex` / `BuildConfig` / `QueryConfig` / `SearchResults` / `SearchResponse`
  - `build_index_with_options` / `rebuild_index_with_options` / `IndexBuildOptions`
- 专家层（`codgrep::advanced`）：
  - `IndexSearcher` / `WorkspaceSnapshot` / `PathFilter` / `IndexProgress*` / JSON 模型
- 实验层（`codgrep::experimental`）：
  - `planner`、`tokenizer`、低层 `index_format` 工具（后续可能调整）

入口选择（推荐）：

- 一次性脚本/工具链任务：优先用 crate 级 `build_index*` / `rebuild_index*` free functions。
- 常驻服务 / IDE / agent 集成：优先持有 `SearchEngine` 或 `WorkspaceIndex`。

核心职责边界（固定定义）：

- `SearchEngine`：面向大多数调用方的统一搜索入口。
- `WorkspaceIndex`：面向需要管理 `base snapshot` / `overlay` / `freshness` 的集成方。
- `advanced::IndexSearcher`：面向高级场景的低层只读索引查询接口。

建议新用户先掌握这组最小 API：

- `BuildConfig`
- `QueryConfig`
- `SearchEngine`
- `SearchResponse`
- `SearchResults`
- `SearchMode`
- `WorkspaceIndex`
- `WorkspaceIndexOptions`
- `WorkspaceFreshness`
- `build_index_with_options`
- `rebuild_index_with_options`
- `IndexBuildOptions`

如果你希望把项目当成一个可复用库，而不是只走 CLI，当前推荐的接入顺序是：

1. 用 `WorkspaceIndex::open(...)` 打开工作区 facade。
2. 启动时用 `ensure_base_snapshot()` 确保 base snapshot 已存在。
3. 调度侧按自己的策略调用 `probe_freshness()` / `probe_freshness_if_due(ttl)` 观察工作区是否偏离 base。
4. 对同一个只读视图做多次查询时，优先复用同一个 `WorkspaceSnapshot`。

最小示例：

```rust
use codgrep::{
    BuildConfig, CorpusMode, QueryConfig, TokenizerMode, WorkspaceIndex, WorkspaceIndexOptions,
};

fn main() -> codgrep::Result<()> {
    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: BuildConfig {
            repo_path: "/path/to/repo".into(),
            index_path: "/path/to/repo/.codgrep-index".into(),
            tokenizer: TokenizerMode::Trigram,
            corpus_mode: CorpusMode::RespectIgnore,
            include_hidden: false,
            max_file_size: 2 * 1024 * 1024,
            min_sparse_len: 3,
            max_sparse_len: 32,
        },
    })?;

    workspace.ensure_base_snapshot()?;
    let freshness = workspace.probe_freshness_if_due(std::time::Duration::from_secs(2))?;
    if freshness.needs_base_snapshot() {
        workspace.ensure_base_snapshot()?;
    }

    let snapshot = workspace.snapshot()?;
    let results = snapshot.search(&QueryConfig {
        regex_pattern: "foo.*bar".into(),
        patterns: vec!["foo.*bar".into()],
        ..QueryConfig::default()
    })?;

    println!(
        "base={}",
        freshness.base_snapshot_key.as_deref().unwrap_or("<missing>")
    );
    println!("hits={}", results.hits.len());
    Ok(())
}
```

调用建议：

- `WorkspaceIndex::search()` 适合直接做一次性查询。
- `WorkspaceIndex::snapshot()` 适合先固定一个只读视图，再连续跑多个 query。
- `SearchEngine::search_with_filter()` 是 path scope 的便捷入口，不建议继续扩展更多 `search_with_*` 变体。
- 如果需要 JSON 结构，可在搜索后做适配：`codgrep::advanced::json::JsonSearchReport::from_search_results(&results)`。
- `advanced::json` 主要用于 CLI/集成输出适配，当前不承诺长期 schema 完全稳定。
- `WorkspaceIndex::probe_freshness()` 每次都做一次真实 freshness 检查。
- `WorkspaceIndex::probe_freshness_if_due(ttl)` 适合调度层限频检查；`if_due` 表示“上次 probe 超过 `ttl` 才重新探测”。
- `WorkspaceIndex::search()` / `snapshot().search()` 会直接按“base snapshot + 当前 dirty files 修补”的视图执行查询，不需要额外 refresh 步骤。
- `snapshot_key` / `base_snapshot_key` 是应该对外缓存或记录的稳定身份；不要依赖内部目录命名。

## Benchmark

当前项目已集成一版兼容 ripgrep `benchsuite` 命名方式的 benchmark runner。

语料目录默认按下面布局准备：

- `bench_data/linux/`
- `bench_data/subtitles/en.sample.txt`
- `bench_data/subtitles/ru.txt`

运行示例：

```bash
cargo run -- bench --suite-dir /Users/user/workspace/codgrep/bench_data --filter linux_literal --rebuild
```

如果希望顺带对比 `rg`：

```bash
cargo run -- bench --suite-dir /Users/user/workspace/codgrep/bench_data --filter subtitles_en_literal --compare-rg --rebuild
```

如果希望额外测库层 facade 和 dirty worktree 路径：

```bash
cargo run -- bench \
  --suite-dir /Users/user/workspace/codgrep/bench_data \
  --filter linux_literal \
  --compare-worktree \
  --compare-workspace \
  --rebuild
```

如果希望 dirty worktree 更贴近“只改了几份文件”的 agent 场景，可以给 worktree fixture 加一个文件数上限。这个模式会从完整语料里抽一个保留真实目录结构的小型子仓库，并保证包含当前 benchmark 需要命中的文件：

```bash
cargo run -- bench \
  --suite-dir /Users/user/workspace/codgrep/bench_data \
  --filter linux_literal \
  --compare-worktree \
  --worktree-sample-files 32 \
  --rebuild
```

如果希望比较 cold query，可以切到 `cold` cache mode。这个模式会在每个 sample 前主动驱逐本次 benchmark 会访问到的 index / corpus 文件页缓存；如果你还想连系统级 page cache 一起清掉，可以额外提供一个 hook：

```bash
cargo run -- bench \
  --suite-dir /Users/user/workspace/codgrep/bench_data \
  --filter linux_literal \
  --compare-rg \
  --cache-mode cold \
  --bench-iter 3 \
  --cold-hook 'sync && printf 3 | tee /proc/sys/vm/drop_caches >/dev/null'
```

说明：

- `codgrep` 的 benchmark 会单独统计索引构建时间和查询时间。
- `--compare-worktree` 会额外跑 dirty worktree 查询路径；当前实现直接走“base snapshot + query-time dirty repair”，不再依赖持久化 overlay cache。历史 runner 名称里的 `dirty_first_query` / `dirty_cached_query` 仅为兼容已有 benchmark 输出。
- `--worktree-sample-files <N>` 只影响 `--compare-worktree` 的 fixture 构造：不再复制完整目录，而是抽一个最多 `N` 个文本文件的小型真实子仓库。这个模式更适合测“少量 dirty 文件”的真实查询成本；不传时仍然保留完整仓库 worst-case。
- `--compare-workspace` 会额外跑 clean workspace facade 路径；每个 sample 都会重新走一次 `snapshot().search()`，用于把工作区读视图构造成本也计入结果，而不是只测固定 snapshot 上的纯查询。
- `rg` 对比项只统计直接搜索耗时，不包含任何建索引步骤。
- `--raw-output <path>` 会把每个 sample 以 CSV 形式落盘；除了 `runner` 原始 id 之外，还会额外写出 `runner_family` 和 `runner_mode`，方便脚本稳定地区分 `base_index` / `workspace_snapshot` / `dirty_first_query` / `dirty_cached_query` 等路径。当前这两条 dirty runner 都走 query-time repair，只是为了兼容旧 benchmark 标签而保留了名称。
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
- `lookup.bin` 已做 `mmap`，`postings.bin` 仍是按 offset 解码读取。
- CLI 和库层都不会在后台偷偷重建 base snapshot；如果需要新的 commit base，需要显式调用 build/rebuild 接口。

## 库层状态

`WorkspaceIndex::ensure_base_snapshot()` / `WorkspaceIndex::status()` 暴露的 `BaseSnapshotInfo` 现在包含：

- `snapshot_key`
- `snapshot_kind`
- `head_commit`
- `config_fingerprint`

另外还可以直接调用：

- `WorkspaceIndex::probe_freshness()`
- `WorkspaceIndex::probe_freshness_if_due(ttl)`

这里的 `snapshot_key` / `base_snapshot_key` 才是外部库应该依赖的稳定身份。

## 下一步建议

- 给查询规划器补充更完整的 AST/HIR 解析。
- 为 posting list 增加压缩和缓存。
- 把 overlay 从“按 dirty state 命中的完整小索引 + 受影响 token 级重写”继续演进到“更细粒度的字节级 patch 和跨 base 复用”。
- 加入 doc_freq 统计与更激进的 top-k token 选择器。
