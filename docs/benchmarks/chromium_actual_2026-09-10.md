# Chromium 实际源码树：最终版检索对比

源码：`/Users/user/workspace/chromium-src`，HEAD `606acc70f6320a025c719d7eee8a4583c9205252`。索引均新建在实验目录；原仓既有 `.gitignore` 修改保留，测试前后 HEAD、tracked 状态和 ignore 文件哈希一致。

使用 14 组既有 Chromium 查询，涵盖符号、宽匹配、正则、大小写、无匹配和目录限定。两种索引工具按 tgrep → flashgrep 顺序串行构建和测量；每个工具内部随机化查询顺序，文件路径 RPC 预热 1 轮后测 5 轮，计数 CLI 预热 1 轮后测 3 轮。ripgrep 在构建前建立基准，最后另测 3 轮。RAYON_NUM_THREADS=10，rg --threads 10，tgrep serve --max-cpu 100。未清空操作系统页缓存。

检索范围为 C/C++（*.cc、*.h，部分查询只搜 *.cc），8 MiB 文件上限，默认隐藏/忽略规则。与既有查询集一致，显式排除 chrome/chrome_elf/dll_hash/**，因为该目录的 Git tracked 与 ignore 收录规则不同。目录限定通过相同的根相对 glob 下推到两个服务。索引本身仍按各工具的规则构建全树，文件总体不完全相同。

正确性：差异记录 **0** 条；所有查询的完整文件集合及每个文件的匹配行数均与 rg 比较，每次计时结果也检查。重启首查正确：flashgrep=True，tgrep=True。

## 返回命中文件路径

中位数，毫秒。flashgrep/tgrep 为持久 TCP RPC，包含传输和 JSON 解析；rg 为命令行端到端，包含启动和文件扫描。服务响应载荷不同（tgrep 带首条匹配信息），不能将差异完全归因于索引算法。

| ID | 查询 | flashgrep RPC | tgrep RPC | rg CLI | tgrep / flashgrep |
|---|---|---:|---:|---:|---:|
| chromium_01 | `ScopedAllowBlockingForTesting` | 9.47 | 22.12 | 3582.53 | 2.34× |
| chromium_02 | `\bscoped_refptr\b` | 255.69 | 204.23 | 3817.03 | 0.80× |
| chromium_03 | `BackForwardCache` | 11.59 | 13.08 | 3784.44 | 1.13× |
| chromium_04 | `IN_PROC_BROWSER_TEST_F` | 51.85 | 42.39 | 2351.75 | 0.82× |
| chromium_05 | `(?:scoped_refptr&#124;raw_ptr&#124;std::unique_ptr&#124;std::optional)` | 1039.97 | 702.79 | 3716.89 | 0.68× |
| chromium_06 | `[A-Z][A-Z0-9_]*_EXPORT` | 318.77 | 204.68 | 3611.17 | 0.64× |
| chromium_07 | `IN_PROC_BROWSER_TEST(?:_F)?\([^,]+,\s*[A-Za-z0-9_]+` | 58.52 | 51.09 | 2284.37 | 0.87× |
| chromium_08 | `#include "base/` | 1428.17 | 870.93 | 3713.92 | 0.61× |
| chromium_09 | `(?i)chromium` | 3152.44 | 1678.01 | 3732.91 | 0.53× |
| chromium_10 | `\bif\b` | 2967.84 | 1529.44 | 3833.37 | 0.52× |
| chromium_11 | `(?m)^\s*namespace base \{$` | 94.27 | 167.13 | 3813.62 | 1.77× |
| chromium_12 | `FlashgrepDefinitelyMissingSymbol` | 1.09 | 8.21 | 3787.51 | 7.55× |
| chromium_13 | `#include` | 557.00 | 419.65 | 1501.37 | 0.75× |
| chromium_14 | `TODO\([^)]+\)` | 304.03 | 288.73 | 3897.94 | 0.95× |

flashgrep 在 4/14 组文件路径查询中更快；tgrep/flashgrep 几何平均比值 1.00×。仅描述这组查询，不代表通用工作负载。

## 每个文件的匹配行数（CLI）

中位数，毫秒。包含各自原生命令的启动、TCP、输出编码等成本。flashgrep 有专门计数路径，tgrep 的 CLI 可能接收匹配行再计数；这些是接口实际成本，不能视为完全等价的纯索引内核测量。

| ID | flashgrep | tgrep | rg | tgrep / flashgrep |
|---|---:|---:|---:|---:|
| chromium_01 | 21.71 | 24.87 | 3464.99 | 1.15× |
| chromium_02 | 176.46 | 246.59 | 3714.19 | 1.40× |
| chromium_03 | 37.20 | 34.77 | 3864.45 | 0.93× |
| chromium_04 | 59.55 | 119.36 | 2212.69 | 2.00× |
| chromium_05 | 1042.49 | 1356.91 | 3522.53 | 1.30× |
| chromium_06 | 217.56 | 155.09 | 3652.02 | 0.71× |
| chromium_07 | 68.21 | 82.37 | 2144.10 | 1.21× |
| chromium_08 | 1429.91 | 1316.25 | 3645.37 | 0.92× |
| chromium_09 | 3241.20 | 1683.92 | 3617.46 | 0.52× |
| chromium_10 | 3107.88 | 3793.18 | 3921.76 | 1.22× |
| chromium_11 | 48.61 | 128.97 | 3803.39 | 2.65× |
| chromium_12 | 22.37 | 14.99 | 3759.84 | 0.67× |
| chromium_13 | 406.95 | 1171.70 | 1399.78 | 2.88× |
| chromium_14 | 294.17 | 357.93 | 3767.84 | 1.22× |

## 建库、内存与重启

各一次，不是多轮统计；RSS 每 250 ms 采样，仅 daemon 进程。flashgrep 为续跑后的查询 daemon，tgrep 为建库后启动的查询 daemon；两边均不含独立的完整建库内存记录，因此不是建库峰值比较。

| 指标 | flashgrep | tgrep |
|---|---:|---:|
| 新索引准备，秒 | 201.88 | 33.37 |
| 初次服务 open 后等待就绪，秒 | 2.53 | 6.09 |
| daemon 已观察峰值 RSS，MiB | 1078.4 | 4636.7 |
| 重启 open，毫秒 | 656.79 | 1763.98 |
| 重启首查，毫秒 | 14.72 | 45.02 |
| 首查后等待后台就绪，秒 | 113.34 | 6.85 |

索引文件数：flashgrep 444,129，tgrep 443,565。索引目录实际磁盘占用：flashgrep 2.34 GiB，tgrep 2.63 GiB。

flashgrep 新索引准备为启动服务并 ensure_repo；tgrep 为独立 index 命令，随后再启动 serve，工作边界不同。重启 open 不包含后台核对完成；两边首查都另行核对正确性。本轮直接读取真实源码树，不修改源码或切分支，因此不测编辑到可见延迟；变更实验是此前隔离副本的独立数据。

原始记录：bench.py、queries.json、measurements.json、differences.json、lifecycle.json、environment.json 和运行日志。summary.json 包含 p50、p95（样本较少时为最大值）和范围。

脚本修正记录：第一次运行的 rg 路径 glob 工作目录不正确，整组查询计时已丢弃重测，原记录保留在 attempt-1-scope-oracle；tgrep 已完成的首次建库仍保留。第二次运行在 flashgrep 的计数 CLI JSON 解析处停止，修正封装解析后用 resume.py 续跑；已通过的 tgrep 全部查询与原始建库时间保留，flashgrep 查询重新预热并完整测量，原记录见 attempt-2-count-parser。最终 392 条记录均来自修正后的有效测量，不包含被否决的行。

完整运行产物：`/Users/user/workspace/chromium-real-benchmark-20260910`。
