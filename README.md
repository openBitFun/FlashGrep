# fast_code_search

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
cargo run -- --index /path/to/repo/.bitfun-index 'foo.*bar' /path/to/repo
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
- 搜索期的 `--hidden` / `--no-ignore` 只在索引本身覆盖对应 corpus 时可用；如果索引是在更窄的 corpus 上构建的，CLI 会直接报错要求重建索引，而不是静默漏结果。
- 搜索期的 `--ignore-file` 会像 ripgrep 一样额外缩小搜索范围，并且在 `--no-ignore` 下仍然生效。
- 搜索前会检查索引是否过期；如果工作树里有新增、删除或改动文件，CLI 会直接报 stale index 错误，要求先重建。

`--json` 现在对齐 ripgrep 的基本约束：

- 只用于搜索结果输出，不与 `--files`、`--type-list`、`-l`、`-L`、`-c`、`--count-matches` 组合。
- 输出事件序列为 `begin` / `match|context` / `end` / `summary`。

## Benchmark

当前项目已集成一版兼容 ripgrep `benchsuite` 命名方式的 benchmark runner。

语料目录默认按下面布局准备：

- `bench_data/linux/`
- `bench_data/subtitles/en.sample.txt`
- `bench_data/subtitles/ru.txt`

运行示例：

```bash
cargo run -- bench --suite-dir /Users/user/workspace/bitfun_grep/bench_data --filter linux_literal --rebuild
```

如果希望顺带对比 `rg`：

```bash
cargo run -- bench --suite-dir /Users/user/workspace/bitfun_grep/bench_data --filter subtitles_en_literal --compare-rg --rebuild
```

说明：

- `bitfun_grep` 的 benchmark 会单独统计索引构建时间和查询时间。
- `rg` 对比项只统计直接搜索耗时，不包含任何建索引步骤。
- 当前 Linux 语料只要求仓库已 clone，不要求本机必须能成功构建内核。

## 当前边界

- 只索引 UTF-8 文本文件。
- 查询规划器优先保证正确性，复杂表达式会降级到弱查询或全量候选。
- 当前增量建索引只复用文档级 token 集；posting 仍会按最新文档集整体重写，还不是基于 commit 的 overlay 索引。
- `lookup.bin` 已做 `mmap`，`postings.bin` 仍是按 offset 解码读取。
- 当前还没有自动重建 stale index；检测到过期后会直接报错。

## 下一步建议

- 给查询规划器补充更完整的 AST/HIR 解析。
- 为 posting list 增加压缩和缓存。
- 把增量建索引从“文档级 token 复用”演进到“base index + working tree overlay”。
- 加入 doc_freq 统计与更激进的 top-k token 选择器。
