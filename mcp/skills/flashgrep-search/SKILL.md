---
name: flashgrep-search
description: Use flashgrep MCP for repository search without waiting for index construction. Fall back to the agent's built-in search only when flashgrep is unavailable.
---

# Flashgrep Search

Use `search_code` and `glob_files` directly without checking `repo_status` or calling `build_index` first. When the MCP server's default repository is not the current workspace, pass the current workspace path as `repo_path`; do not hardcode a repository path in the request. Search starts a missing index build in the background and returns results through a transition backend while the index is unavailable or still building. Do not wait for index construction. Use the agent's built-in search only when the flashgrep MCP call fails, the workspace is not a supported Git worktree, or the repository is degraded.
