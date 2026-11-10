# 增量内存发布重构

状态：最终 366 项测试通过，12 轮生产接口实验与原目录 140 次查询核对完成，副本恢复与源码/二进制身份校验通过。完整速度与边界见 [复测报告](benchmarks/chromium_memory_publication_2026-09-10.md)。

目标：把查询可见与磁盘 checkpoint 拆开，保持统一 posting 查询实现，以及 Git 可选的工作区模型。

- `StagedSegmentStore` 为新段构建共享 posting 和不可变源码读者，先放内存；checkpoint 复用 posting 写盘，不再次分词。删除记录同样暂存内存。
- 暂存 store 的内容/posting/元数据估算预算为 256 MiB；超限溢写旧段，单个超大段直接写盘。构建瞬时内存、查询 reader 引用及落盘内容解码缓存另计，不能把该预算解释为 RSS 上限。
- 生产 overlay 每个完整批次先 flush 到内存段并 refresh；后台线程捕获一个版本，释放发布锁后编码磁盘文件，再短暂持锁发布 durable manifest。更新期间的新段、删除与内容保留在 refreshed 视图。
- checkpoint 同时校验替换 epoch 和原 durable manifest 身份，避免旧 base 或旧 engine 的完成回调覆盖新版本。写盘/manifest 发布失败不推进 committed 序号。失效的旧 engine 也禁止继续发布合并结果和执行垃圾回收；退休标记与这两个操作使用同一状态锁，避免替换实例后旧任务仍修改磁盘。
- 文件发现与持久化由独立工作线程处理，只消费各自的唤醒和 deadline。合并仍是持发布锁的一次有界任务；发现了更新尚未提交时推迟合并。
- 正常关闭等待两个 worker 后提交尚未持久化的内容与 catalog。崩溃后先加载最后持久化版本，再核对文件系统。
- 删除旧的同步 auto-checkpoint 操作数阈值配置及 CLI 标志；后台调度仍使用原 maintenance delay/retry 配置。基线磁盘格式不变，不需要因此重建已有基线。

## 验证过程记录

第一次完整测试：304/313 通过；其余包括旧同步持久化断言，以及需要在捕获 checkpoint 前 flush 的 Refresh 路径。修复后 overlay 专项 16/16 通过，内存 checkpoint 专项通过。

第二次完整测试：307/317 通过。九项 bench 子进程启动超时；采样随后确认测试程序与 CLI 停留在 macOS `_dyld_start`，旧版二进制同样不能正常启动。一项强制恢复测试暴露显式 pending 被周期 deadline 跳过的问题，已修复并加入回归覆盖，尚待完整重跑。

后续新增受控阻塞磁盘的并发发布测试和内存溢写/旧 reader 保留测试。`cargo check --all-targets` 已通过当前检查点；该检查点尚未完成最终验证；后续结果见下文和最终复测报告。

原始日志、采样和复测脚本：`/Users/user/workspace/flashgrep-memory-publication-20260910`。

最终完整功能测试（`tests6.log`）：320 项库测试、34 项 e2e、5 项 managed daemon、6 项 MCP 全部通过。覆盖阻塞磁盘期间的新发布/查询、旧 checkpoint 与替换实例隔离、内存预算溢写、持有旧读者、显式恢复唤醒、删除与重启恢复。MCP 入口残留同步阈值参数已同步移除。

恢复后的实验环境中，另一个桌面应用的 flashgrep 进程占用约 3 核；不终止用户应用。后续大目录试验交替运行两工具，记录重复样本；与历史测量比较时需要保留环境差异说明。

## 追加的分词构建优化

对照中发现 sparse n-gram 的 ASCII 标识符 fallback 检查构造完整区间列表却仅询问是否为空。改成提前返回的存在性判断，保持原 token 输出。新增 10,752 组 ASCII 输入/长度参数等价性检查；最终 321 项库测试与 45 项集成测试全部通过，`cargo check --all-targets` 通过。

此修改不改变索引语义和磁盘格式，最终复测直接复用上一轮兼容基线。内存发布首版二进制为 `430107…`，追加计算优化后为 `266301…`；两版源码和二进制均在实验目录单独归档。不能把跨时间矩阵的全部差值归因于单项优化，尤其 tgrep 同期也受系统负载变化影响。
