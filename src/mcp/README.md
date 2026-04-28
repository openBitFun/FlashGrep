# Codgrep MCP

这是 `flashgrep` 的独立 Node MCP server。

典型启动方式：

```bash
node src/mcp/server.js --repo /path/to/repo
```

这个 server 会在当前 MCP session 内自己拉起并管理一个 `flashgrep serve --stdio` 子进程。
MCP server 退出时，会对这个后端发 `shutdown`，必要时再终止进程；不会把它 detach 成独立 daemon。

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
