# 生产版本大批变更、查询与重启对比

使用同一份 50,000 文件隔离副本，先普通目录后 Git 仓；每轮按 1,000 → 10,000 → 50,000 个累计修改文件推进，再重启和还原。工具串行执行，轮次交替顺序。所有查询核对完整路径集合。

查询时间为持久 TCP RPC 的客户端耗时，包含传输、JSON 解析和结果接收；仅返回文件路径。表中 p50/p95 汇总各轮稳定阶段样本，更新耗时取各轮中位数，内存峰值取各轮最大值。单位为 ms 和 MiB。

| 模式 | 工具 | 累计修改 | 本轮新增修改 | 写文件 | 首次写入至收敛 | RSS 前/后 | 峰值 RSS |
|---|---|---:|---:|---:|---:|---:|---:|
| plain | flashgrep | 1000 | 1000 | 89.85 | 906.89 | 198.5 / 355.6 | 934.9 |
| plain | flashgrep | 10000 | 9000 | 856.93 | 11802.73 | 355.6 / 678.3 | 1235.6 |
| plain | flashgrep | 50000 | 40000 | 3408.50 | 49977.50 | 678.3 / 1401.4 | 2442.2 |
| git | flashgrep | 1000 | 1000 | 100.93 | 888.02 | 193.5 / 346.4 | 955.5 |
| git | flashgrep | 10000 | 9000 | 809.69 | 11798.04 | 346.4 / 663.3 | 1263.3 |
| git | flashgrep | 50000 | 40000 | 3470.21 | 41319.19 | 663.3 / 1394.4 | 2487.7 |

| 模式 | 工具 | 轮次 | 50k 查询后合并等待 ms | 合并空闲 RSS MiB | 含合并等待的峰值 MiB |
|---|---|---:|---:|---:|---:|
| plain | flashgrep | 0 | 20985.96 | 2297.7 | 2442.2 |
| plain | flashgrep | 1 | 55609.12 | 1196.3 | 1466.7 |
| plain | flashgrep | 2 | 58278.98 | 1279.8 | 1495.9 |
| git | flashgrep | 0 | 20409.53 | 2408.9 | 2498.3 |
| git | flashgrep | 1 | 58767.72 | 1204.4 | 1516.3 |
| git | flashgrep | 2 | 58723.07 | 1164.8 | 1452.4 |

仅在最后 50k 档的稳定查询之后等待后台合并空闲，不改变前三档之间的变更序列。查询后等待与结果可见延迟分开列出；这是新版增加的观测，旧基线没有同等的空闲观测。


| 模式 | 工具 | 累计修改 | 查询 | 命中文件 | p50 | p95 | 样本数 |
|---|---|---:|---|---:|---:|---:|---:|
| plain | flashgrep | 0 | rare | 0 | 0.20 | 0.24 | 60 |
| plain | flashgrep | 0 | all | 0 | 0.20 | 0.26 | 60 |
| plain | flashgrep | 0 | mixed | 328 | 0.83 | 1.10 | 60 |
| plain | flashgrep | 0 | missing | 0 | 0.20 | 0.24 | 60 |
| plain | flashgrep | 0 | regex | 0 | 0.33 | 0.38 | 60 |
| plain | flashgrep | 1000 | rare | 1 | 0.49 | 0.67 | 60 |
| plain | flashgrep | 1000 | all | 1000 | 9.82 | 10.50 | 60 |
| plain | flashgrep | 1000 | mixed | 328 | 1.41 | 2.12 | 60 |
| plain | flashgrep | 1000 | missing | 0 | 0.28 | 0.47 | 60 |
| plain | flashgrep | 1000 | regex | 20 | 3.10 | 3.71 | 60 |
| plain | flashgrep | 10000 | rare | 10 | 0.72 | 1.22 | 60 |
| plain | flashgrep | 10000 | all | 10000 | 20.52 | 23.82 | 60 |
| plain | flashgrep | 10000 | mixed | 328 | 1.89 | 2.59 | 60 |
| plain | flashgrep | 10000 | missing | 0 | 0.38 | 3.27 | 60 |
| plain | flashgrep | 10000 | regex | 200 | 4.95 | 6.15 | 60 |
| plain | flashgrep | 50000 | rare | 50 | 1.58 | 119.77 | 60 |
| plain | flashgrep | 50000 | all | 50000 | 75.16 | 193.94 | 60 |
| plain | flashgrep | 50000 | mixed | 328 | 1.78 | 41.66 | 60 |
| plain | flashgrep | 50000 | missing | 0 | 0.48 | 41.90 | 60 |
| plain | flashgrep | 50000 | regex | 1000 | 14.78 | 17.67 | 60 |
| git | flashgrep | 0 | rare | 0 | 0.22 | 0.27 | 60 |
| git | flashgrep | 0 | all | 0 | 0.22 | 0.27 | 60 |
| git | flashgrep | 0 | mixed | 328 | 2.09 | 2.39 | 60 |
| git | flashgrep | 0 | missing | 0 | 0.21 | 0.27 | 60 |
| git | flashgrep | 0 | regex | 0 | 0.35 | 0.41 | 60 |
| git | flashgrep | 1000 | rare | 1 | 0.50 | 0.70 | 60 |
| git | flashgrep | 1000 | all | 1000 | 9.67 | 10.98 | 60 |
| git | flashgrep | 1000 | mixed | 328 | 2.75 | 3.26 | 60 |
| git | flashgrep | 1000 | missing | 0 | 0.32 | 0.44 | 60 |
| git | flashgrep | 1000 | regex | 20 | 3.19 | 3.55 | 60 |
| git | flashgrep | 10000 | rare | 10 | 0.76 | 1.50 | 60 |
| git | flashgrep | 10000 | all | 10000 | 20.33 | 23.08 | 60 |
| git | flashgrep | 10000 | mixed | 328 | 2.91 | 7.62 | 60 |
| git | flashgrep | 10000 | missing | 0 | 0.42 | 3.50 | 60 |
| git | flashgrep | 10000 | regex | 200 | 5.07 | 6.25 | 60 |
| git | flashgrep | 50000 | rare | 50 | 1.54 | 57.69 | 60 |
| git | flashgrep | 50000 | all | 50000 | 74.22 | 200.15 | 60 |
| git | flashgrep | 50000 | mixed | 328 | 1.84 | 4.64 | 60 |
| git | flashgrep | 50000 | missing | 0 | 0.48 | 3.75 | 60 |
| git | flashgrep | 50000 | regex | 1000 | 15.00 | 20.03 | 60 |

| 模式 | 工具 | 轮次 | 启动及 open | open 后基线收敛 |
|---|---|---:|---:|---:|
| plain | flashgrep | 0 | 10537.89 | 0.70 |
| plain | flashgrep | 1 | 127.21 | 2480.44 |
| plain | flashgrep | 2 | 189.29 | 2434.11 |
| git | flashgrep | 0 | 11785.99 | 9918.96 |
| git | flashgrep | 1 | 150.51 | 2529.13 |
| git | flashgrep | 2 | 139.24 | 2522.41 |

每个模式/工具的第 0 轮从空索引开始，后续轮次复用已有索引；初次启动表逐轮列出，避免混合建库和重开成本。


基线就绪条件：flashgrep 查询集合正确、probe_healthy=true、无 workspace_probe_pending 且脏路径数符合预期；tgrep 查询集合正确、indexing=false 且文件数符合预期。没有额外等待所有后台校准或 revision 完全相等，因此首批更新可能与启动校准重叠；这些状态标准也不是两种工具内部索引进度的等价定义。


| 模式 | 工具 | 重启 open | 首次查询 | 首次命中文件（各轮） | 首次查询完整（各轮） | open 后收敛 | RSS | 峰值 RSS |
|---|---|---:|---:|---|---|---:|---:|---:|
| plain | flashgrep | 186.98 | 119.84 | 50000, 50000, 50000 | 是, 是, 是 | 2011.94 | 927.4 | 945.2 |
| git | flashgrep | 191.17 | 141.28 | 50000, 50000, 50000 | 是, 是, 是 | 1981.01 | 938.7 | 946.6 |

rare 为小结果集合，all 为全部修改文件，mixed 为原生符号 ScopedAllowBlockingForTesting，missing 为无匹配查询，regex 为固定范围正则。不同查询命中规模不同，应逐行比较同一查询。

重启采用正常关闭后的持久状态，操作系统页缓存未清空。open 包含进程连接与 flashgrep ensure_repo；tgrep 后台初始化的剩余成本体现在收敛时间，因此不能仅比较 open 一列。这里的收敛时间从首次查询之后开始计时。

RSS 每 250 ms 采样，只统计 daemon 进程；不等同于整个进程树或系统页缓存，可能漏掉短峰值。结果收敛不代表磁盘压缩已完全结束，尤其不能将 tgrep 的结果正确视为所有旧 posting 已清理。

该实验比较两个生产实现，不能单独证明内存存储相对磁盘存储的因果收益；其余索引结构、候选过滤和读取策略也不同。

原始报告与 JSON、trace、环境信息：/Users/user/workspace/flashgrep-production-benchmark-20260910/large-final。
