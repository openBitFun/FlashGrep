#!/usr/bin/env node

const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");

const JSONRPC_VERSION = "2.0";
const MCP_SERVER_NAME = "codgrep-mcp";
const MCP_SERVER_VERSION = "0.1.0";
const SUPPORTED_PROTOCOL_VERSIONS = [
  "2025-11-25",
  "2025-06-18",
  "2025-03-26",
  "2024-11-05",
];
const DEFAULTS = {
  tokenizer: "sparse_ngram",
  corpusMode: "respect_ignore",
  hidden: false,
  maxFileSize: 8 * 1024 * 1024,
  minSparseLen: 3,
  maxSparseLen: 8,
  rebuildDirtyThreshold: 256,
  overlayAutoCheckpointMaxUncommittedOps: 256,
  overlayMergeMinDelayMs: 2000,
  overlayMergeRetryDelayMs: 10000,
};
const START_TIMEOUT_MS = 10_000;
const RETRY_INTERVAL_MS = 100;
const SHUTDOWN_TIMEOUT_MS = 2_000;

async function main() {
  const options = parseServerArgs(process.argv.slice(2));
  const server = new McpServer(options);
  const transport = new ContentLengthTransport(process.stdin, process.stdout);
  installSignalHandlers(server);

  try {
    for await (const { message, framing } of transport.readMessages()) {
      const response = await server.handleRequest(message);
      if (response) {
        transport.writeMessage(response, framing);
      }
    }
  } finally {
    await server.close();
  }
}

class McpServer {
  constructor(options) {
    this.options = options;
    this.backend = null;
    this.backendStart = null;
    this.backendClose = null;
    this.closed = false;
  }

  async handleRequest(request) {
    const { jsonrpc, id = null, method, params = {} } = request;
    if (jsonrpc !== JSONRPC_VERSION) {
      return errorResponse(id, -32600, `unsupported jsonrpc version: ${jsonrpc}`);
    }

    switch (method) {
      case "initialize":
        return this.handleInitialize(id, params);
      case "notifications/initialized":
      case "notifications/cancelled":
        return null;
      case "ping":
        return successResponse(id, {});
      case "tools/list":
        return successResponse(id, { tools: toolDefinitions() });
      case "tools/call":
        return this.handleToolCall(id, params);
      default:
        return id == null
          ? null
          : errorResponse(id, -32601, `method not found: ${method}`);
    }
  }

  handleInitialize(id, params) {
    const protocolVersion = params?.protocolVersion;
    if (typeof protocolVersion !== "string") {
      return errorResponse(id, -32602, "invalid request params: protocolVersion is required");
    }
    if (!SUPPORTED_PROTOCOL_VERSIONS.includes(protocolVersion)) {
      return errorResponse(id, -32602, "Unsupported protocol version", {
        supported: SUPPORTED_PROTOCOL_VERSIONS,
        requested: protocolVersion,
      });
    }

    const instructions = this.options.repoPath
      ? `Tools operate on ${this.options.repoPath} by default. Individual tool calls may override repo_path.`
      : "Tool calls must provide repo_path unless the server is started with --repo.";

    return successResponse(id, {
      protocolVersion,
      capabilities: {
        tools: {
          listChanged: false,
        },
      },
      serverInfo: {
        name: MCP_SERVER_NAME,
        title: "Codgrep Search MCP",
        version: MCP_SERVER_VERSION,
      },
      instructions,
    });
  }

  async handleToolCall(id, params) {
    if (!params || typeof params.name !== "string") {
      return errorResponse(id, -32602, "invalid request params: tool name is required");
    }

    try {
      let result;
      switch (params.name) {
        case "search_code":
          result = await this.searchCode(params.arguments ?? {});
          break;
        case "glob_files":
          result = await this.globFiles(params.arguments ?? {});
          break;
        case "repo_status":
          result = await this.repoStatus(params.arguments ?? {});
          break;
        case "refresh_repo":
          result = await this.refreshRepo(params.arguments ?? {});
          break;
        case "build_index":
          result = await this.buildIndex(params.arguments ?? {});
          break;
        default:
          return errorResponse(id, -32602, `Unknown tool: ${params.name}`);
      }

      return successResponse(id, toolResult(result, false));
    } catch (error) {
      return successResponse(
        id,
        toolResult(
          {
            error: error.message,
          },
          true,
        ),
      );
    }
  }

  async searchCode(arguments_) {
    const args = parseSearchArguments(arguments_);
    const repoContext = await openRepoForCall(this, args);
    if (
      !isIndexedSearchReady(repoContext.status.phase) &&
      !args.allow_scan_fallback
    ) {
      throw new Error(
        `repo is not ready for indexed search (phase=${repoContext.status.phase}); call build_index first or retry with allow_scan_fallback=true`,
      );
    }
    const patterns = [args.pattern, ...args.regexp];
    const pattern = combinePatterns(patterns, args.fixed_strings);
    const response = await sendDaemonRequest(repoContext.backend, "search", {
      repo_id: repoContext.repoId,
      query: {
        pattern,
        patterns,
        case_insensitive: args.case_insensitive,
        multiline: args.multiline,
        dot_matches_new_line: args.multiline_dotall,
        fixed_strings: args.fixed_strings,
        word_regexp: args.word_regexp,
        line_regexp: args.line_regexp,
        before_context: args.before_context,
        after_context: args.after_context,
        top_k_tokens: args.top_k_tokens,
        max_count: args.max_count,
        global_max_results: args.global_max_results,
        search_mode: args.search_mode,
      },
      scope: buildPathScope(args),
      consistency: args.consistency,
      allow_scan_fallback: args.allow_scan_fallback,
    });

    return {
      daemon_addr: repoContext.addr,
      repo_id: repoContext.repoId,
      status: repoContext.status,
      response,
    };
  }

  async globFiles(arguments_) {
    const args = parseGlobArguments(arguments_);
    const repoContext = await openRepoForCall(this, args);
    const response = await sendDaemonRequest(repoContext.backend, "glob", {
      repo_id: repoContext.repoId,
      scope: buildPathScope(args),
    });
    return {
      daemon_addr: repoContext.addr,
      repo_id: repoContext.repoId,
      response,
    };
  }

  async repoStatus(arguments_) {
    const args = parseRepoArguments(arguments_);
    const repoContext = await openRepoForCall(this, args);
    const response = await sendDaemonRequest(repoContext.backend, "get_repo_status", {
      repo_id: repoContext.repoId,
    });
    return {
      daemon_addr: repoContext.addr,
      repo_id: repoContext.repoId,
      response,
    };
  }

  async refreshRepo(arguments_) {
    const args = parseRefreshArguments(arguments_);
    const repoContext = await openRepoForCall(this, args);
    const response = await sendDaemonRequest(repoContext.backend, "refresh_repo", {
      repo_id: repoContext.repoId,
      force: args.force,
    });
    return {
      daemon_addr: repoContext.addr,
      repo_id: repoContext.repoId,
      response,
    };
  }

  async buildIndex(arguments_) {
    const args = parseRepoArguments(arguments_);
    const repoContext = await ensureRepoForCall(this, args);
    return {
      daemon_addr: repoContext.addr,
      repo_id: repoContext.repoId,
      indexed_docs: repoContext.indexedDocs,
      status: repoContext.status,
    };
  }

  async ensureBackend() {
    if (this.closed) {
      throw new Error("MCP server is shutting down");
    }
    if (this.backend) {
      try {
        await pingDaemon(this.backend);
        return this.backend;
      } catch {
        await shutdownBackend(this.backend);
        this.backend = null;
      }
    }

    if (this.backendStart) {
      return this.backendStart;
    }

    const startPromise = this.startBackend();
    this.backendStart = startPromise;
    try {
      return await startPromise;
    } finally {
      if (this.backendStart === startPromise) {
        this.backendStart = null;
      }
    }
  }

  async startBackend() {
    const backend = await spawnBackend(this.options);
    if (this.closed) {
      await shutdownBackend(backend);
      throw new Error("MCP server is shutting down");
    }
    backend.exitPromise.finally(() => {
      if (this.backend === backend) {
        this.backend = null;
      }
    });
    this.backend = backend;
    return backend;
  }

  async close() {
    if (this.backendClose) {
      return this.backendClose;
    }

    this.closed = true;
    this.backendClose = (async () => {
      const backend = this.backend;
      const pendingStart = this.backendStart;
      this.backend = null;
      if (backend) {
        await shutdownBackend(backend);
        return;
      }
      if (pendingStart) {
        try {
          const startedBackend = await pendingStart;
          await shutdownBackend(startedBackend);
        } catch {
          // Startup failure path already cleaned up its own child state.
        }
      }
    })();

    return this.backendClose;
  }
}

class ContentLengthTransport {
  constructor(input, output) {
    this.input = input;
    this.output = output;
  }

  async *readMessages() {
    let buffer = Buffer.alloc(0);
    for await (const chunk of this.input) {
      buffer = Buffer.concat([buffer, chunk]);
      while (true) {
        const trimmedPrefix = buffer
          .toString("utf8")
          .replace(/^\s+/, "");
        if (trimmedPrefix.startsWith("{") || trimmedPrefix.startsWith("[")) {
          const newlineIndex = buffer.indexOf("\n");
          if (newlineIndex !== -1) {
            const line = buffer.subarray(0, newlineIndex).toString("utf8").trim();
            buffer = buffer.subarray(newlineIndex + 1);
            if (line.length === 0) {
              continue;
            }
            yield {
              message: JSON.parse(line),
              framing: "newline",
            };
            continue;
          }
        }

        const headerEnd = buffer.indexOf("\r\n\r\n");
        if (headerEnd === -1) {
          break;
        }
        const header = buffer.subarray(0, headerEnd).toString("utf8");
        const match = header.match(/Content-Length:\s*(\d+)/i);
        if (!match) {
          throw new Error("missing Content-Length header");
        }
        const contentLength = Number.parseInt(match[1], 10);
        const messageEnd = headerEnd + 4 + contentLength;
        if (buffer.length < messageEnd) {
          break;
        }
        const body = buffer.subarray(headerEnd + 4, messageEnd).toString("utf8");
        buffer = buffer.subarray(messageEnd);
        yield {
          message: JSON.parse(body),
          framing: "content-length",
        };
      }
    }
  }

  writeMessage(message, framing = "content-length") {
    const bodyText = JSON.stringify(message);
    if (framing === "newline") {
      this.output.write(`${bodyText}\n`);
      return;
    }

    const body = Buffer.from(bodyText, "utf8");
    this.output.write(`Content-Length: ${body.length}\r\n\r\n`);
    this.output.write(body);
  }
}

function parseServerArgs(argv) {
  const args = {
    repoPath: undefined,
    storageRoot: undefined,
    daemonBin: undefined,
    config: {
      tokenizer: DEFAULTS.tokenizer,
      corpus_mode: DEFAULTS.corpusMode,
      include_hidden: DEFAULTS.hidden,
      max_file_size: DEFAULTS.maxFileSize,
      min_sparse_len: DEFAULTS.minSparseLen,
      max_sparse_len: DEFAULTS.maxSparseLen,
    },
    refresh: {
      rebuild_dirty_threshold: DEFAULTS.rebuildDirtyThreshold,
      overlay_auto_checkpoint_max_uncommitted_ops:
        DEFAULTS.overlayAutoCheckpointMaxUncommittedOps,
      overlay_merge_min_delay_ms: DEFAULTS.overlayMergeMinDelayMs,
      overlay_merge_retry_delay_ms: DEFAULTS.overlayMergeRetryDelayMs,
    },
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = argv[index + 1];
    switch (arg) {
      case "--repo":
        args.repoPath = normalizeExistingPath(requireValue(arg, next));
        index += 1;
        break;
      case "--storage-root":
        args.storageRoot = path.resolve(requireValue(arg, next));
        index += 1;
        break;
      case "--daemon-bin":
        args.daemonBin = path.resolve(requireValue(arg, next));
        index += 1;
        break;
      case "--tokenizer":
        args.config.tokenizer = normalizeTokenizer(requireValue(arg, next));
        index += 1;
        break;
      case "--corpus-mode":
        args.config.corpus_mode = normalizeCorpusMode(requireValue(arg, next));
        index += 1;
        break;
      case "--hidden":
      case "-.":
        args.config.include_hidden = true;
        break;
      case "--max-file-size":
        args.config.max_file_size = parseNonNegativeInt(arg, next);
        index += 1;
        break;
      case "--min-sparse-len":
        args.config.min_sparse_len = parseNonNegativeInt(arg, next);
        index += 1;
        break;
      case "--max-sparse-len":
        args.config.max_sparse_len = parseNonNegativeInt(arg, next);
        index += 1;
        break;
      case "--rebuild-dirty-threshold":
        args.refresh.rebuild_dirty_threshold = parseNonNegativeInt(arg, next);
        index += 1;
        break;
      case "--overlay-auto-checkpoint-max-uncommitted-ops":
        args.refresh.overlay_auto_checkpoint_max_uncommitted_ops = parseNonNegativeInt(arg, next);
        index += 1;
        break;
      case "--overlay-merge-min-delay-ms":
        args.refresh.overlay_merge_min_delay_ms = parseNonNegativeInt(arg, next);
        index += 1;
        break;
      case "--overlay-merge-retry-delay-ms":
        args.refresh.overlay_merge_retry_delay_ms = parseNonNegativeInt(arg, next);
        index += 1;
        break;
      case "--help":
      case "-h":
        printHelp();
        process.exit(0);
      default:
        throw new Error(`unknown argument: ${arg}`);
    }
  }

  return args;
}

function parseRepoArguments(value) {
  return {
    repo_path: optionalPath(value.repo_path),
    storage_root: optionalResolvedPath(value.storage_root),
  };
}

function parseRefreshArguments(value) {
  const args = parseRepoArguments(value);
  return {
    ...args,
    force: Boolean(value.force),
  };
}

function parseGlobArguments(value) {
  const args = parseRepoArguments(value);
  return {
    ...args,
    glob: stringArray(value.glob),
    iglob: stringArray(value.iglob),
    roots: pathArray(value.roots),
    file_type: stringArray(value.file_type),
    file_type_not: stringArray(value.file_type_not),
    type_add: stringArray(value.type_add),
    type_clear: stringArray(value.type_clear),
  };
}

function parseSearchArguments(value) {
  if (typeof value?.pattern !== "string" || value.pattern.length === 0) {
    throw new Error("invalid tool arguments: pattern is required");
  }
  return {
    ...parseGlobArguments(value),
    pattern: value.pattern,
    regexp: stringArray(value.regexp),
    fixed_strings: Boolean(value.fixed_strings),
    case_insensitive: Boolean(value.case_insensitive),
    multiline: Boolean(value.multiline),
    multiline_dotall: Boolean(value.multiline_dotall),
    word_regexp: Boolean(value.word_regexp),
    line_regexp: Boolean(value.line_regexp),
    before_context: nonNegativeOrDefault(value.before_context, 0, "before_context"),
    after_context: nonNegativeOrDefault(value.after_context, 0, "after_context"),
    top_k_tokens: nonNegativeOrDefault(value.top_k_tokens, 6, "top_k_tokens"),
    max_count: nullableNonNegative(value.max_count, "max_count"),
    global_max_results: nullableNonNegative(
      value.global_max_results,
      "global_max_results",
    ),
    search_mode: enumValue(
      value.search_mode,
      "search_mode",
      [
        "count_only",
        "count_matches",
        "first_hit_only",
        "materialize_matches",
      ],
      "materialize_matches",
    ),
    consistency: enumValue(
      value.consistency,
      "consistency",
      ["snapshot_only", "workspace_eventual", "workspace_strict"],
      "workspace_eventual",
    ),
    allow_scan_fallback: Boolean(value.allow_scan_fallback),
  };
}

async function ensureRepoForCall(server, args) {
  const { options } = server;
  const repoPath = resolveRepoPath(options.repoPath, args.repo_path);
  const storageRoot = resolveStorageRoot(options.storageRoot, args.storage_root, repoPath);
  const daemon = await server.ensureBackend();
  const result = await sendDaemonRequest(daemon, "ensure_repo", {
    repo_path: repoPath,
    storage_root: storageRoot,
    config: options.config,
    refresh: options.refresh,
  });
  if (result.kind !== "repo_ensured") {
    throw new Error(`unexpected ensure_repo response: ${JSON.stringify(result)}`);
  }
  return {
    backend: daemon,
    addr: daemon.addr,
    repoId: result.repo_id,
    indexedDocs: result.indexed_docs ?? null,
    status: result.status,
  };
}

async function openRepoForCall(server, args) {
  const { options } = server;
  const repoPath = resolveRepoPath(options.repoPath, args.repo_path);
  const storageRoot = resolveStorageRoot(options.storageRoot, args.storage_root, repoPath);
  const daemon = await server.ensureBackend();
  const result = await sendDaemonRequest(daemon, "open_repo", {
    repo_path: repoPath,
    storage_root: storageRoot,
    config: options.config,
    refresh: options.refresh,
  });
  if (result.kind !== "repo_opened") {
    throw new Error(`unexpected open_repo response: ${JSON.stringify(result)}`);
  }
  return {
    backend: daemon,
    addr: daemon.addr,
    repoId: result.repo_id,
    status: result.status,
  };
}

function isIndexedSearchReady(phase) {
  return phase === "ready_clean" || phase === "ready_dirty";
}

async function spawnBackend(options) {
  const program = selectDaemonProgram(options.daemonBin);
  const child = spawn(program, ["serve", "--stdio"], {
    stdio: ["pipe", "pipe", "pipe"],
  });
  if (child.stderr) {
    child.stderr.on("data", (chunk) => {
      process.stderr.write(chunk);
    });
  }

  const backend = {
    addr: "stdio",
    child,
    transport: new ContentLengthTransport(child.stdout, child.stdin),
    pending: new Map(),
    nextRequestId: 1,
    readLoop: null,
    exited: false,
    exitCode: null,
    exitSignal: null,
    spawnError: null,
    exitPromise: null,
  };
  backend.exitPromise = new Promise((resolve) => {
    child.once("exit", (code, signal) => {
      backend.exited = true;
      backend.exitCode = code;
      backend.exitSignal = signal;
      rejectPendingRequests(
        backend,
        new Error(formatBackendExit(backend, "codgrep backend exited")),
      );
      resolve({ code, signal });
    });
  });
  backend.readLoop = readBackendMessages(backend);

  child.once("error", (error) => {
    backend.spawnError = error;
    rejectPendingRequests(backend, error);
  });

  try {
    await withTimeout(
      initializeBackend(backend),
      START_TIMEOUT_MS,
      `failed to initialize codgrep backend within ${START_TIMEOUT_MS}ms`,
    );
    return backend;
  } catch (error) {
    if (backend.spawnError) {
      await shutdownBackend(backend);
      throw backend.spawnError;
    }
    await shutdownBackend(backend);
    throw error;
  }
}

function selectDaemonProgram(explicitProgram) {
  if (explicitProgram) {
    return explicitProgram;
  }
  if (process.env.CODGREP_DAEMON_BIN) {
    return process.env.CODGREP_DAEMON_BIN;
  }

  const releaseBinary = path.resolve(__dirname, "../../target/release/cg");
  if (fs.existsSync(releaseBinary)) {
    return releaseBinary;
  }
  const debugBinary = path.resolve(__dirname, "../../target/debug/cg");
  if (fs.existsSync(debugBinary)) {
    return debugBinary;
  }
  return "cg";
}

async function initializeBackend(backend) {
  const result = await sendDaemonRequest(backend, "initialize", {
    client_info: {
      name: MCP_SERVER_NAME,
      version: MCP_SERVER_VERSION,
    },
    capabilities: {
      progress: false,
      status_notifications: false,
      task_notifications: false,
    },
  });
  if (result?.kind !== "initialize_result") {
    throw new Error(`unexpected initialize response: ${JSON.stringify(result)}`);
  }
  sendDaemonNotification(backend, "initialized");
}

async function pingDaemon(backend) {
  await sendDaemonRequest(backend, "ping", {});
}

function sendDaemonRequest(backend, method, params) {
  if (backend.exited) {
    return Promise.reject(new Error(formatBackendExit(backend, "codgrep backend exited")));
  }
  if (backend.spawnError) {
    return Promise.reject(backend.spawnError);
  }

  const id = backend.nextRequestId;
  backend.nextRequestId += 1;
  const request = {
    jsonrpc: JSONRPC_VERSION,
    id,
    method,
    ...(params === undefined ? {} : { params }),
  };

  return new Promise((resolve, reject) => {
    backend.pending.set(id, { resolve, reject });
    try {
      backend.transport.writeMessage(request);
    } catch (error) {
      backend.pending.delete(id);
      reject(error);
    }
  });
}

function sendDaemonNotification(backend, method, params) {
  backend.transport.writeMessage({
    jsonrpc: JSONRPC_VERSION,
    method,
    ...(params === undefined ? {} : { params }),
  });
}

async function readBackendMessages(backend) {
  try {
    for await (const { message } of backend.transport.readMessages()) {
      if (typeof message?.id === "number") {
        const pending = backend.pending.get(message.id);
        if (!pending) {
          continue;
        }
        backend.pending.delete(message.id);
        if (message.error) {
          pending.reject(new Error(message.error.message));
        } else {
          pending.resolve(message.result);
        }
      }
    }
  } catch (error) {
    rejectPendingRequests(backend, error);
    return;
  }

  if (!backend.exited) {
    rejectPendingRequests(
      backend,
      new Error("codgrep backend closed stdio without sending a response"),
    );
  }
}

function rejectPendingRequests(backend, error) {
  if (backend.pending.size === 0) {
    return;
  }
  const pending = [...backend.pending.values()];
  backend.pending.clear();
  for (const request of pending) {
    request.reject(error);
  }
}

function buildPathScope(args) {
  return {
    roots: args.roots,
    globs: args.glob,
    iglobs: args.iglob,
    type_add: args.type_add,
    type_clear: args.type_clear,
    types: args.file_type,
    type_not: args.file_type_not,
  };
}

function resolveRepoPath(defaultRepoPath, overrideRepoPath) {
  const repoPath = overrideRepoPath ?? defaultRepoPath;
  if (!repoPath) {
    throw new Error(
      "repo_path is required when the MCP server is started without --repo",
    );
  }
  return repoPath;
}

function resolveStorageRoot(defaultStorageRoot, overrideStorageRoot, repoPath) {
  return overrideStorageRoot ?? defaultStorageRoot ?? path.join(repoPath, ".codgrep-index-engine");
}

function toolDefinitions() {
  return [
    {
      name: "search_code",
      title: "Search Code",
      description:
        "Run a codgrep search against the configured repository or a repo_path override.",
      inputSchema: {
        type: "object",
        properties: {
          repo_path: schemaString(
            "Repository root path. Optional when the server was started with --repo.",
          ),
          storage_root: schemaString("Override index storage root for this call."),
          pattern: schemaString("Primary regex or literal pattern."),
          regexp: schemaStringArray("Additional patterns to OR together with pattern."),
          fixed_strings: schemaBool("Treat pattern and regexp entries as literals."),
          case_insensitive: schemaBool("Enable case-insensitive matching."),
          multiline: schemaBool("Enable multiline regex mode."),
          multiline_dotall: schemaBool("Make . match newlines when multiline is enabled."),
          word_regexp: schemaBool("Require word boundaries around matches."),
          line_regexp: schemaBool("Require the whole line to match."),
          before_context: schemaInteger("Number of context lines before each match."),
          after_context: schemaInteger("Number of context lines after each match."),
          top_k_tokens: schemaInteger("Planner token budget. Default is 6."),
          max_count: schemaNullableInteger("Per-pattern result cap."),
          global_max_results: schemaNullableInteger(
            "Global result cap across the request.",
          ),
          search_mode: {
            type: "string",
            enum: [
              "count_only",
              "count_matches",
              "first_hit_only",
              "materialize_matches",
            ],
          },
          consistency: {
            type: "string",
            enum: ["snapshot_only", "workspace_eventual", "workspace_strict"],
          },
          allow_scan_fallback: schemaBool(
            "Allow scan fallback if indexed execution cannot serve the query.",
          ),
          glob: schemaStringArray("Include glob filters."),
          iglob: schemaStringArray("Case-insensitive include glob filters."),
          roots: schemaStringArray(
            "Restrict search to repo-relative or absolute subpaths.",
          ),
          file_type: schemaStringArray("Only search the given file types."),
          file_type_not: schemaStringArray("Exclude the given file types."),
          type_add: schemaStringArray("Additional ripgrep-style type definitions."),
          type_clear: schemaStringArray(
            "Type definitions to clear before applying type_add.",
          ),
        },
        required: ["pattern"],
        additionalProperties: false,
      },
    },
    {
      name: "glob_files",
      title: "Glob Files",
      description:
        "Resolve visible files using codgrep's ignore, glob, and file type rules.",
      inputSchema: {
        type: "object",
        properties: {
          repo_path: schemaString(
            "Repository root path. Optional when the server was started with --repo.",
          ),
          storage_root: schemaString("Override index storage root for this call."),
          glob: schemaStringArray("Include glob filters."),
          iglob: schemaStringArray("Case-insensitive include glob filters."),
          roots: schemaStringArray(
            "Restrict globbing to repo-relative or absolute subpaths.",
          ),
          file_type: schemaStringArray("Only include the given file types."),
          file_type_not: schemaStringArray("Exclude the given file types."),
          type_add: schemaStringArray("Additional ripgrep-style type definitions."),
          type_clear: schemaStringArray(
            "Type definitions to clear before applying type_add.",
          ),
        },
        additionalProperties: false,
      },
    },
    {
      name: "repo_status",
      title: "Repo Status",
      description:
        "Open the repository in the session backend and return its current codgrep status.",
      inputSchema: {
        type: "object",
        properties: {
          repo_path: schemaString(
            "Repository root path. Optional when the server was started with --repo.",
          ),
          storage_root: schemaString("Override index storage root for this call."),
        },
        additionalProperties: false,
      },
    },
    {
      name: "refresh_repo",
      title: "Refresh Repo",
      description: "Ask codgrep to refresh workspace state for the repository.",
      inputSchema: {
        type: "object",
        properties: {
          repo_path: schemaString(
            "Repository root path. Optional when the server was started with --repo.",
          ),
          storage_root: schemaString("Override index storage root for this call."),
          force: schemaBool(
            "Force a refresh even if codgrep thinks the repo is up to date.",
          ),
        },
        additionalProperties: false,
      },
    },
    {
      name: "build_index",
      title: "Build Index",
      description:
        "Ensure the repository has a searchable base snapshot, starting the backend process as needed.",
      inputSchema: {
        type: "object",
        properties: {
          repo_path: schemaString(
            "Repository root path. Optional when the server was started with --repo.",
          ),
          storage_root: schemaString("Override index storage root for this call."),
        },
        additionalProperties: false,
      },
    },
  ];
}

function toolResult(structuredContent, isError) {
  return {
    content: [
      {
        type: "text",
        text: JSON.stringify(structuredContent, null, 2),
      },
    ],
    structuredContent,
    isError,
  };
}

function successResponse(id, result) {
  return {
    jsonrpc: JSONRPC_VERSION,
    id,
    result,
  };
}

function errorResponse(id, code, message, data) {
  return {
    jsonrpc: JSONRPC_VERSION,
    id,
    error: {
      code,
      message,
      ...(data === undefined ? {} : { data }),
    },
  };
}

function schemaString(description) {
  return {
    type: "string",
    description,
  };
}

function schemaStringArray(description) {
  return {
    type: "array",
    description,
    items: {
      type: "string",
    },
  };
}

function schemaBool(description) {
  return {
    type: "boolean",
    description,
  };
}

function schemaInteger(description) {
  return {
    type: "integer",
    minimum: 0,
    description,
  };
}

function schemaNullableInteger(description) {
  return {
    anyOf: [
      {
        type: "integer",
        minimum: 0,
      },
      {
        type: "null",
      },
    ],
    description,
  };
}

function combinePatterns(patterns, fixedStrings) {
  if (patterns.length === 1) {
    return fixedStrings ? escapeRegex(patterns[0]) : patterns[0];
  }
  const branches = patterns.map((patternValue) =>
    fixedStrings ? escapeRegex(patternValue) : patternValue,
  );
  return `(?:${branches.join("|")})`;
}

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function normalizeExistingPath(value) {
  return fs.realpathSync.native(path.resolve(value));
}

function optionalPath(value) {
  if (value === undefined || value === null) {
    return undefined;
  }
  if (typeof value !== "string" || value.length === 0) {
    throw new Error("invalid tool arguments: repo_path must be a non-empty string");
  }
  return normalizeExistingPath(value);
}

function optionalResolvedPath(value) {
  if (value === undefined || value === null) {
    return undefined;
  }
  if (typeof value !== "string" || value.length === 0) {
    throw new Error("invalid tool arguments: storage_root must be a non-empty string");
  }
  return path.resolve(value);
}

function stringArray(value) {
  if (value === undefined || value === null) {
    return [];
  }
  if (!Array.isArray(value) || value.some((entry) => typeof entry !== "string")) {
    throw new Error("invalid tool arguments: expected an array of strings");
  }
  return value;
}

function pathArray(value) {
  return stringArray(value).map((entry) =>
    path.isAbsolute(entry) ? path.resolve(entry) : entry,
  );
}

function nullableNonNegative(value, fieldName) {
  if (value === undefined || value === null) {
    return null;
  }
  return nonNegativeOrDefault(value, 0, fieldName);
}

function nonNegativeOrDefault(value, defaultValue, fieldName) {
  if (value === undefined || value === null) {
    return defaultValue;
  }
  if (!Number.isInteger(value) || value < 0) {
    throw new Error(`invalid tool arguments: ${fieldName} must be a non-negative integer`);
  }
  return value;
}

function enumValue(value, fieldName, allowed, defaultValue) {
  if (value === undefined || value === null) {
    return defaultValue;
  }
  if (!allowed.includes(value)) {
    throw new Error(
      `invalid tool arguments: ${fieldName} must be one of ${allowed.join(", ")}`,
    );
  }
  return value;
}

function normalizeTokenizer(value) {
  switch (value) {
    case "trigram":
      return "trigram";
    case "sparse-ngram":
    case "sparse_ngram":
      return "sparse_ngram";
    default:
      throw new Error(`invalid tokenizer: ${value}`);
  }
}

function normalizeCorpusMode(value) {
  switch (value) {
    case "ignore":
    case "respect-ignore":
    case "respect_ignore":
      return "respect_ignore";
    case "no-ignore":
    case "no_ignore":
      return "no_ignore";
    default:
      throw new Error(`invalid corpus mode: ${value}`);
  }
}

function parseNonNegativeInt(flag, value) {
  const parsed = Number.parseInt(requireValue(flag, value), 10);
  if (!Number.isInteger(parsed) || parsed < 0) {
    throw new Error(`${flag} must be a non-negative integer`);
  }
  return parsed;
}

function requireValue(flag, value) {
  if (value === undefined) {
    throw new Error(`missing value for ${flag}`);
  }
  return value;
}

function printHelp() {
  process.stdout.write(`Usage: node src/mcp/server.js [OPTIONS]

Options:
  --repo <REPO>
  --storage-root <STORAGE_ROOT>
  --daemon-bin <DAEMON_BIN>
  --tokenizer <trigram|sparse-ngram>
  --corpus-mode <ignore|no-ignore>
  --hidden
  --max-file-size <BYTES>
  --min-sparse-len <N>
  --max-sparse-len <N>
  --rebuild-dirty-threshold <N>
  --overlay-auto-checkpoint-max-uncommitted-ops <N>
  --overlay-merge-min-delay-ms <MS>
  --overlay-merge-retry-delay-ms <MS>
  -h, --help
`);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function installSignalHandlers(server) {
  const shutdown = (exitCode) => {
    void server.close().finally(() => {
      process.exit(exitCode);
    });
  };

  process.once("SIGINT", () => shutdown(130));
  process.once("SIGTERM", () => shutdown(143));
}

async function shutdownBackend(backend) {
  if (!backend) {
    return;
  }
  if (backend.exited || backend.spawnError) {
    return;
  }

  if (backend.child.stdin && !backend.child.stdin.destroyed) {
    try {
      await withTimeout(
        sendDaemonRequest(backend, "shutdown"),
        SHUTDOWN_TIMEOUT_MS,
        "timed out waiting for codgrep backend shutdown",
      );
    } catch {
      // Fall through to process termination below.
    }
  }

  if (!backend.exited) {
    backend.child.kill("SIGTERM");
    try {
      await withTimeout(
        backend.exitPromise,
        SHUTDOWN_TIMEOUT_MS,
        "timed out waiting for codgrep backend to exit after SIGTERM",
      );
    } catch {
      backend.child.kill("SIGKILL");
      await withTimeout(
        backend.exitPromise,
        SHUTDOWN_TIMEOUT_MS,
        "timed out waiting for codgrep backend to exit after SIGKILL",
      ).catch(() => {});
    }
  }
}

function formatBackendExit(backend, prefix) {
  if (backend.exitSignal) {
    return `${prefix} (signal=${backend.exitSignal})`;
  }
  return `${prefix} (code=${backend.exitCode ?? "unknown"})`;
}

function withTimeout(promise, timeoutMs, message) {
  let timer = null;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => {
      reject(new Error(message));
    }, timeoutMs);
  });
  return Promise.race([promise, timeout]).finally(() => {
    if (timer) {
      clearTimeout(timer);
    }
  });
}

main().catch((error) => {
  process.stderr.write(`${error.message}\n`);
  process.exit(1);
});
