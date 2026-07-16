# flashgrep MCP

`flashgrep MCP` 用于在支持 MCP 的 IDE 中接入 flashgrep 代码搜索能力。

本目录是 MCP server、文档和 Agent Skill 的唯一源码。GitHub Actions 会把这里的内容与六个平台的 `flashgrep` 二进制组合成 `flashgrep-mcp.zip`；`bin/` 是发布产物，不提交到源码仓库。

## 环境要求

使用前需满足以下条件：

1. 已安装 `Node.js 18+`
2. 已在 IDE 中将本目录下的 `server.js` 配置为 MCP Server

## 路径约定

下文使用以下占位符：

- `<FLASHGREP_MCP_DIR>`：本目录解压后的实际路径
- `<REPO>`：需要搜索的 Git worktree 或其子目录

示例：

```text
<FLASHGREP_MCP_DIR> = /Users/you/tools/flashgrep-mcp
<REPO> = /Users/you/workspace/project
```

## 配置方法

### Trae

在 Trae 中使用 `server.js` 作为 MCP Server 入口。

#### 使用当前打开的仓库

```json
{
  "mcpServers": {
    "flashgrep": {
      "command": "node",
      "args": [
        "<FLASHGREP_MCP_DIR>/server.js"
      ]
    }
  }
}
```

MCP 会自动发现当前目录所属的 Git worktree 根目录并复用根索引；如果当前目录是 monorepo 子目录，默认搜索范围仍限制在该子目录。

#### 固定搜索某个仓库

如有需要，也可显式指定 `--repo` 参数

```json
{
  "mcpServers": {
    "flashgrep": {
      "command": "node",
      "args": [
        "<FLASHGREP_MCP_DIR>/server.js",
        "--repo",
        "<REPO>"
      ]
    }
  }
}
```

### Cursor

在项目级 `.cursor/mcp.json` 中添加以下配置：

```json
{
  "mcpServers": {
    "flashgrep": {
      "command": "node",
      "args": [
        "<FLASHGREP_MCP_DIR>/server.js"
      ]
    }
  }
}
```

### Claude Code

可以直接在项目根目录放一个 `.mcp.json`：

```json
{
  "mcpServers": {
    "flashgrep": {
      "type": "stdio",
      "command": "node",
      "args": [
        "<FLASHGREP_MCP_DIR>/server.js"
      ],
      "env": {}
    }
  }
}
```

### Codex

可以直接在 `~/.codex/config.toml` 中添加：

```toml
[mcp_servers.flashgrep]
command = "node"
args = ["<FLASHGREP_MCP_DIR>/server.js", "--repo", "<REPO>"]
```

## 可选参数

### `--repo <REPO>`

指定当前工作目录或需要搜索的子目录。MCP 会自动发现其 Git worktree 根目录作为索引根。


### `--storage-root <DIR>`

指定索引数据的存储目录。

通常无需配置。仅在需要将索引文件存放到指定目录时使用。

### `--daemon-bin <PATH>`

手动指定后端二进制文件路径。

通常无需配置。发布包中的 `server.js` 会从 `bin/` 目录中自动选择与当前系统匹配的二进制文件；直接从源码仓库运行时会使用 `target/release/flashgrep` 或 `target/debug/flashgrep`。

## 可用工具

当前 MCP Server 提供以下 5 个工具。

### `repo_status`

查看当前仓库状态。

常用于确认仓库路径、索引状态，以及当前是否已有可用的基础索引。

### `build_index`

异步启动基础索引构建并立即返回。普通搜索无需先调用该工具；`search_code` 会在缺少索引时自动启动后台构建。

### `search_code`

搜索代码。

用于在当前仓库中执行代码搜索。

索引尚未建立或正在构建时，搜索会立即使用过渡后端返回结果，不会等待索引完成。响应中的 `backend` 字段会说明本次使用的是索引还是过渡搜索。

常用限制参数：

- `global_max_results`: 限制本次搜索整体返回的结果数量；`search_code` 默认是 `100`。不同 `search_mode` 下限制的单位不同：`line_matches` 限制匹配行数，`files_with_matches` 限制路径数，`count_only` 限制 `file_counts` 条目数，`count_matches` 限制 `file_match_counts` 条目数。
- `max_count`: 限制每个文件内最多返回/统计多少个匹配，语义类似 `rg --max-count`。如果想让整个搜索只返回 1 条结果，应使用 `global_max_results: 1`，不是 `max_count: 1`。

### `glob_files`

按 glob 或文件类型列出文件。

例如，可用于列出某个目录下的所有 `js` 文件。

### `refresh_repo`

刷新当前工作区状态。

切换分支、修改代码，或怀疑搜索结果不是最新时，可先调用该工具。

## 首次使用流程

首次接入时直接描述搜索需求即可，不需要先检查状态、建立索引或等待索引完成。例如：

```text
请使用 flashgrep MCP 搜索 ManagedDaemonClient。
```

```text
用 flashgrep 搜索所有引用 ManagedDaemonClient 的地方。
```

```text
在当前仓库里搜索 Content-Security-Policy。
```

Agent 会调用 `search_code`，并基于 flashgrep 返回结果继续分析代码。

## 日常使用方式

配置完成后，无需手动执行命令。

在 IDE 对话中直接说明搜索意图即可，Agent 会根据需求调用对应的 flashgrep MCP 工具， 或者直接在第一次对话的时候显式提到用flashgrep的搜索工具代替原有的搜索。



示例：

```text
用 flashgrep 搜索 ManagedDaemonClient。
```

```text
帮我找一下 repo_status 是在哪里实现的，用 flashgrep 搜索。
```

```text
用 flashgrep 查一下 mcp 目录下有哪些 js 文件。
```

```text
先用 flashgrep 列出 mcp 下的所有 js 文件，然后帮我看入口文件。
```

```text
用 flashgrep 搜索 build_index 的调用位置。
```

```text
用 flashgrep 搜索包含 refresh_repo 的代码，并总结它的调用链。
```

## 切换分支或修改代码后

切换分支、拉取新代码或修改代码后，如果需要确保搜索结果基于最新工作区内容，可以在搜索前输入：

```text
我刚切换了分支，请先使用 flashgrep MCP 刷新当前仓库状态，然后再搜索 ManagedDaemonClient。
```

或：

```text
先调用 refresh_repo 刷新工作区状态，再用 flashgrep 搜索 build_index。
```

Agent 会先调用 `refresh_repo`，再继续执行后续搜索。

## 常见问题

### 索引尚未建立或正在构建

这是正常的过渡状态。`search_code` 会启动缺失的索引构建，并使用 `rg_fallback` 或 `scan_fallback` 立即返回本次搜索结果。后续搜索会在索引可用后自动切换到 indexed backend。

### 搜索结果像是旧分支内容

通常是工作区状态尚未刷新。
可以输入：

```text
请先使用 flashgrep MCP 刷新当前仓库状态，然后重新搜索。
```

或：

```text
先 refresh_repo，再用 flashgrep 搜索刚才的关键词。
```

### MCP 已连接成功，但搜索的是错误仓库

通常是 MCP Server 启动目录不属于目标 Git worktree。
可以在 MCP 配置中显式添加 `--repo` 参数：

```json
{
  "mcpServers": {
    "flashgrep": {
      "command": "node",
      "args": [
        "<FLASHGREP_MCP_DIR>/server.js",
        "--repo",
        "<REPO>"
      ]
    }
  }
}
```

其中 `<REPO>` 可以指向仓库根目录或 monorepo 中需要搜索的子目录。子目录会复用 worktree 根索引，同时保持子目录搜索范围。

配置完成后，可以在 IDE 对话中输入：

```text
请使用 flashgrep MCP 查看当前仓库状态，确认 repo 路径是否正确。
```

### Agent Skill（可选）

本仓库额外提供了一个用于 Agent 的搜索策略 skill：

```text
<FLASHGREP_MCP_DIR>/skills/flashgrep-search
```

该 skill 用于指导 Agent 在代码仓库检索任务中优先使用 flashgrep MCP。其主要约束包括：

- 优先使用 flashgrep MCP 进行仓库发现、文件枚举和代码搜索
- 直接使用 `search_code` 和 `glob_files`，不要把 `repo_status` 或 `build_index` 作为搜索前置步骤
- 不等待索引构建完成；索引过渡状态由 MCP 自动处理
- 仅在 flashgrep MCP 调用失败、当前目录不是受支持的 Git worktree 或仓库处于 degraded 状态时使用 Agent 内建搜索

该 skill 适用于支持 skill、instruction 或规则配置的 Agent 环境。
在这类环境中，可以将以下目录安装或复制到对应的本地 skill 目录：

```text
<FLASHGREP_MCP_DIR>/skills/flashgrep-search
```

也可以在单次任务中显式引用该路径，使 Agent 按该 skill 中的搜索策略执行。

## 维护与打包

在主仓库中直接调试 MCP server：

```bash
cargo build
node mcp/server.js --repo /path/to/repo
```

CI 会先构建六个平台的 release 二进制，再运行：

```bash
./scripts/package-mcp.sh <BINARY_DIR> dist
```

生成的 `dist/flashgrep-mcp.zip` 就是完整对外分发包。不要再手工维护另一份 `server.js` 或 skill 副本。
