# fast_code_search

本项目是一个本地 regex 预筛索引系统骨架，目标是把全库正则搜索拆成两段：

1. 先用字符级倒排索引召回候选文件。
2. 再只对候选文件执行真实 regex 验证。

当前实现先把最小链路跑通：

- 文件级索引
- `trigram` 和 `sparse-ngram` 两种 tokenizer 接口
- `docs.bin` / `lookup.bin` / `postings.bin` 三文件落盘
- 保守型 regex 查询规划器
- 候选召回 + `regex` 精确验证

## 模块布局

- `src/cli.rs`
  命令行入口，提供 `build` 和 `search`。
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
- `lookup.bin`
  保存定长 lookup entry，支持 `mmap + binary search`。
- `postings.bin`
  顺序存 posting list，本版先用原始 `u32 doc_id`。

## 快速开始

```bash
cargo run -- build --repo /path/to/repo --index /path/to/repo/.bitfun-index
cargo run -- search --index /path/to/repo/.bitfun-index --pattern 'foo.*bar'
```

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
- posting list 还没做 delta/varint 压缩。
- `lookup.bin` 已做 `mmap`，`postings.bin` 仍是按 offset 读取。

## 下一步建议

- 给查询规划器补充更完整的 AST/HIR 解析。
- 为 posting list 增加压缩和缓存。
- 增加增量建索引和 mtime 校验。
- 加入 doc_freq 统计与更激进的 top-k token 选择器。
