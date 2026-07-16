# Changelog

## Unreleased

- Reused the canonical Git worktree root index for subdirectory workspaces while keeping search and glob results scoped to the opened subdirectory.
- Made `search_code` start a missing index build in the background and immediately return results through the transition backend.
- Made `build_index` asynchronous and removed the obsolete `allow_scan_fallback` option from the MCP and daemon search interfaces.
- Updated the bundled Agent Skill so index construction is no longer a prerequisite for search.

## v0.2.11 - 2026-07-16

- Reused the canonical root index for monorepo subdirectory MCP sessions while preserving the requested directory scope.
- Returned transition searches immediately through `rg` while a missing base index builds asynchronously.
- Fixed `rg` transition fallback argument ordering and strengthened the Agent Skill to replace built-in repository search.

## v0.2.10 - 2026-06-17

- Updated all bundled `flashgrep` binaries to `0.2.10`.
- Switched the daemon indexed workspace runtime to a Git-only model. Repositories opened through MCP must be non-bare Git worktrees with an existing `HEAD` commit; non-Git directories now fail at tool-call time with an explicit workspace error.
- Fixed repository identity and cache reuse when MCP is launched from a repository subdirectory: `flashgrep` now discovers the canonical Git worktree root before opening a repo, so subdirectory sessions reuse the root `.flashgrep-index-engine` cache instead of creating duplicate per-directory indexes.
- Improved managed daemon state-file resolution so root and subdirectory opens for the same Git worktree connect to the same daemon/index storage.
- Enforced base snapshot identity against the current Git `HEAD`, repository root, and index configuration so stale snapshots from a different checkout or config are not reused accidentally.
- Reworked dirty workspace detection to use Git as the source of truth for tracked and untracked visible paths, reducing filesystem scan fallback logic in the daemon runtime.
- Added a fast path for scoped workspace probes so searches limited to changed subtrees can refresh overlay state without forcing a full worktree probe.
- Clarified MCP/daemon behavior for invalid default repos: initialization and tool listing stay available, while repository validation happens when a tool opens or ensures a repo.
- Reduced duplicated MCP search response text: `content[0].text` now returns a compact human-readable summary and preview while full machine-readable results remain available in `structuredContent`.
- Added MCP `search_code` timing fields (`total_ms`, `open_repo_ms`, `daemon_search_ms`) to help diagnose slow searches from client feedback.

## v0.2.9 - 2026-06-11

- Updated bundled `flashgrep` binaries to `0.2.9`.
- Fixed `global_max_results` handling for `files_with_matches`, `count_only`, and `count_matches` so the limit is applied during indexed verification rather than after results are built.
- Clarified `search_code` option semantics: `global_max_results` caps total returned results for the selected search mode, while `max_count` is a per-file match cap similar to `rg --max-count`.
- Moved default repository warm-up into MCP `initialize`, so workspace overlay refresh happens during startup instead of being charged to the first `search_code` call.

## v0.2.8 - 2026-06-09

- Improved `search_code` results for MCP clients: line-match searches now return grouped file and line-number results, making responses shorter and easier for agents to consume.
- Reduced search response size for large repositories by avoiding unnecessary matched line text in line-match mode.
- Improved result limiting semantics so `global_max_results` applies to the selected search mode more predictably.
- Improved file type filtering: unknown `file_type` values are now treated as file extensions, so filters such as `tsx`, `vue`, or project-specific extensions work more naturally.
- Corrected the MCP server display title to `Flashgrep Search MCP`.
