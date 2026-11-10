# Watcher 与内存增量索引实验（2026-09-09）

## 实现范围

这是可运行的实验入口 `examples/watch_experiment.rs`，不是现有 daemon/MCP 默认路径的替换。正式 daemon 的 Git 分支维护、恢复语义和协议保持原有实现。实验数据用来判断后续应替换的具体环节。

- 普通目录：启动建立文件系统基础索引，随后由 watcher 提供变化范围；定期遍历目录核对大小、mtime 和可见路径集合。
- Git：复用现有不可变 Git 基础索引；启动以 Git diff 核对离线变化。保留 tracked-ignore 豁免和已跟踪文件 symlink 规则，监听 Git index / HEAD / refs 变化。日常发现可在 watcher 路径与 Git scoped diff 之间切换；分支切换产生的文件变化覆盖在原基线上，本原型不自动推进 Git 基础快照。
- 有界通知队列（实验入口默认 16384 条，可用 `--event-capacity 1024` 对照），溢出、OS rescan 信号、watcher 错误、忽略规则变化触发全量核对；安装 watcher 后才开始构建/打开基础索引，避免初始化的通知空窗。
- 内存/磁盘两种 overlay 共用现有分词、posting 与查询代码。构建在查询锁外进行，完成后一次性切换快照。文件恢复到基础快照内容后撤销对应 overlay 条目。
- 为隔离存储影响，本轮两种模式每次都构建当前完整 dirty 集合。尚未实现逐文档内存倒排更新、内存预算、后台 checkpoint 和旧磁盘 generation 回收。每次运行必须使用新的实验存储目录。
- 内存 segment 同时保存 posting 与查询源字节，tgrep 的 LiveIndex 主要保存 posting/mask/path。二者不是相同的内存布局，不能把结果理解成只把 tgrep 的 HashMap 移植到 flashgrep。

## 使用

```sh
cargo build --release --example watch_experiment
./target/release/examples/watch_experiment /absolute/directory \
  --storage /tmp/flashgrep-watch-new-run --overlay memory --discovery watcher
```

启动后输出一行 ready JSON；通过标准输入发送 NDJSON：

```json
{"op":"search","pattern":"TODO","glob":"*.cc"}
{"op":"status"}
{"op":"reconcile"}
{"op":"quit"}
```

Git 实验另外传 `--git-base-storage /absolute/existing-daemon-storage`，路径和索引配置必须与基础索引匹配。`--discovery git` 对每批路径调用 Git scoped diff；`--overlay disk` 切换为磁盘增量。周期核对默认 60 秒，性能试验显式设成 3600 秒以隔离普通通知路径；生产中不能把周期核对当成可省略的逻辑。

## 正确性与限制

测试额外覆盖 ignore 白名单隐藏文件、文件级扫描范围和 Git index-only 跟踪变化。测试覆盖内存 posting/删除记录、旧快照可读性、删除→恢复→新增、目录重命名、忽略规则变化、文本/二进制互转、停止 watcher 后的定期核对、Git 离线忽略变化及分支文件变化。

查询仍为最终一致：刚写完文件时可以读到旧发布。目录核对主要用大小/mtime 判断内容是否需要重读；恶意保持所有元数据且漏掉通知不在保证范围内。实验只在本机 macOS/FSEvents 上验证。原型要求 UTF-8 路径，目前不提供 daemon 完整查询协议。

## 实验记录

原始脚本、日志、逐次查询、内存采样保存在 `/Users/user/workspace/grep-watch-experiment-20260909`。最终测量表和结论见同目录 `REPORT.zh-CN.md`。

## 本轮结论

496 组变更试验全部收敛；另有两次启动预检发现收录规则不一致，已修正并保留失败日志。完整 Chromium 单轮复核中，现版修改 1000 文件约 8.16 秒，最终 watcher 内存/磁盘方案约 0.95/0.84 秒。内存没有稳定优于磁盘；主要收益来自直接处理事件范围和避免队列溢出引发全量核对。不能据此把正式 daemon 改成无内存上限、无 checkpoint 的纯内存方案。

推荐后续采用统一文件收录规则、按路径合并的有界事件队列、Git 专门处理基础快照/分支事件；在替换正式发布路径前完善增量 posting、内存预算及后台 checkpoint。完整耗时及限制见实验报告。

## 大增量检索补充实验

进一步在完整 Chromium 基础索引上构造 1,000 / 10,000 / 50,000 个真实源码文件的增量，比较热查询、每类首次调用与四线程并发。脚本、每次查询时间和独立 rg 真值位于 `/Users/user/workspace/grep-query-large-20260909`，汇总见该目录 `REPORT.zh-CN.md`。

实验入口新增 `query_bench` 和 `query_parallel`，分别测量引擎内部查询与并发批次；返回结果签名以验证稳定性，`include_results` 可返回完整路径及行号供真值核对。它们属于实验协议，不是正式 daemon API。示例：

```json
{"op":"query_bench","pattern":"token_[0-9]+","regex":true,"lines":true,"unlimited":true,"glob":"*.cc"}
{"op":"query_parallel","pattern":"token","workers":4,"repeats":10,"glob":"*.cc"}
```

`unlimited: true` 显式关闭默认 10,000 条行号结果上限；行号模式不包含正文文本渲染。引擎计时结束后才计算校验签名并序列化响应，客户端往返时间由脚本另行记录。
