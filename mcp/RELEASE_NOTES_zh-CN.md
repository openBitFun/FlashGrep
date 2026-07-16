# flashgrep MCP 变更说明

发布时间：2026-07-06 17:19:44 +0800

## 本次包内容

- MCP Server 版本：`0.2.10`
- 内置 `flashgrep` 后端版本：`0.2.10`
- 已替换全部平台二进制：
  - `flashgrep-aarch64-apple-darwin`
  - `flashgrep-x86_64-apple-darwin`
  - `flashgrep-aarch64-unknown-linux-musl`
  - `flashgrep-x86_64-unknown-linux-musl`
  - `flashgrep-aarch64-pc-windows-msvc.exe`
  - `flashgrep-x86_64-pc-windows-msvc.exe`

## 关键修复

### 子目录启动复用仓库根索引

修复了从仓库子目录启动 MCP 时重复创建子目录索引的问题。

现在 `flashgrep` 会先通过 Git discovery 找到 canonical Git worktree root，再打开 repo。比如从仓库子目录：

```text
<repo-root>/base
```

启动 MCP 时，会归一化为：

```text
repo_id=<repo-root>
storage_root=<repo-root>/.flashgrep-index-engine
```

不会再创建：

```text
<repo-root>/base/.flashgrep-index-engine
```

### Git-only indexed workspace runtime

daemon 的 indexed workspace runtime 改为只支持非 bare Git worktree，且要求仓库已有 `HEAD` commit。

非 Git 目录不会在 MCP 初始化阶段直接失败；`initialize` 和 `tools/list` 仍可用。真正打开或确保 repo 的工具调用会返回明确错误。

### base snapshot 身份校验

base snapshot 会校验：

- 当前 Git `HEAD`
- canonical repo root
- index 配置 fingerprint

这样可以避免切换分支、切换 checkout 或配置变化后误用旧 snapshot。

### dirty workspace 检测收敛到 Git

dirty workspace 检测改为以 Git tracked / untracked visible paths 为准，减少 daemon runtime 里的文件系统扫描兜底分支。

### scoped workspace probe 快路径

当搜索限制在某个子树时，daemon 可以只刷新相关路径范围的 overlay 状态，不必每次做完整 worktree probe。

## MCP 响应调整

### 搜索响应文本瘦身

`search_code` 的 `content[0].text` 不再返回完整 pretty JSON，而是返回紧凑的人类可读摘要和最多 20 条预览。

完整机器可读结果仍保留在：

```text
structuredContent
```

这减少了 MCP 客户端传给模型的大段重复文本，但如果调用方显式设置很大的 `global_max_results`，`structuredContent` 本身仍会随结果数量增长。

### 搜索耗时上报

`search_code` 返回新增：

```json
{
  "timings": {
    "total_ms": 353,
    "open_repo_ms": 92,
    "daemon_search_ms": 260
  }
}
```

说明：

- `total_ms`：MCP `search_code` 总耗时
- `open_repo_ms`：打开/确认 repo runtime 的耗时
- `daemon_search_ms`：daemon 执行搜索请求的耗时

不会向 stdout 或 stderr 额外打印耗时日志，避免影响 MCP 协议和客户端日志。

## 已验证

已在 Chromium 大型 Git worktree 上完成验证：

- 从仓库子目录启动 MCP
- `repo_status` 正确归一到 Git worktree root
- `storage_root` 正确使用根目录 `.flashgrep-index-engine`
- `search_code` 走 `indexed_clean`
- `search_code` 返回包含 `timings`
- `content[0].text` 为摘要，完整结果保留在 `structuredContent`

示例验证结果：

```text
server_info.version=0.2.10
repo_id=<repo-root>
storage_root=<repo-root>/.flashgrep-index-engine
phase=ready_clean
search_backend=indexed_clean
matched_lines=1
```

## 相关上游变更

本次主要基于 `flashgrep v0.2.10`，其中修复子目录索引复用的关键提交为：

```text
74766fbc08a5f28502f102dc7afc5e36f76a3e3f
daemon: require git discovery for repo open
时间：2026-06-16 20:37:06 +0800
```
