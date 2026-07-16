---
name: flashgrep-search
description: Replace the agent's built-in repository search with flashgrep MCP. Fall back only when flashgrep is unavailable or cannot search the workspace.
---

# Flashgrep Search

Flashgrep is the repository search backend. Use `glob_files` for file discovery and `search_code` for content or pattern matching instead of the agent's built-in `rg`, `grep`, `find`, IDE search, or equivalent commands. When the MCP server's default repository is not the current workspace, pass the current workspace path as `repo_path`; do not hardcode a repository path. Flashgrep starts missing index construction in the background and searches through a transition backend while the index is unavailable or building, so do not wait for index construction or use `repo_status`/`build_index` as a search prerequisite. Fall back to built-in search only when the MCP call fails, the workspace is not a supported Git worktree, or the repository is degraded.
