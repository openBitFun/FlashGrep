# Flashgrep MCP

这是 `flashgrep` 的独立 Node MCP server。

典型启动方式：

```bash
node src/mcp/server.js --repo /path/to/repo
```

这个 server 会在当前 MCP session 内自己拉起并管理一个 `flashgrep serve --stdio` 子进程。
MCP server 退出时，会对这个后端发 `shutdown`，必要时再终止进程；不会把它 detach 成独立 daemon。

如果没有传 `--repo`，server 会默认把启动进程的当前工作目录（`cwd`）当作 repo 根目录。

MCP `initialize` 和 `tools/list` 不会打开 repo 或构建索引。具体工具调用才会通过
daemon `open_repo` / `ensure_repo` 进入 indexed workspace runtime；该 runtime 要求
目标路径位于已有 `HEAD` commit 的 Git worktree 内。非 Git 目录会在工具调用时返回
明确错误，而不会让 MCP 初始化失败。

Trae 配置示例：

```json
{
  "mcpServers": {
    "flashgrep": {
      "command": "node",
      "args": [
        "/absolute/path/to/flashgrep/src/mcp/server.js",
        "--repo",
        "/absolute/path/to/target-repo"
      ]
    }
  }
}
```
