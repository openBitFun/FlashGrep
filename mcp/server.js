#!/usr/bin/env node

const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");
const { version: MCP_SERVER_VERSION } = require("./package.json");

const JSONRPC_VERSION = "2.0";
const MCP_SERVER_NAME = "flashgrep-mcp";
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
const MCP_TEXT_FILE_PREVIEW_LIMIT = 20;

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

    const instructions =
      `Tools operate on ${this.options.repoPath} by default. ` +
      "Individual tool calls may override repo_path.";

    return successResponse(id, {
      protocolVersion,
      capabilities: {
        tools: {
          listChanged: false,
        },
      },
      serverInfo: {
        name: MCP_SERVER_NAME,
        title: "Flashgrep Search MCP",
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
    const startedAt = monotonicNowMs();
    const args = parseSearchArguments(arguments_);
    const openStartedAt = monotonicNowMs();
    const repoContext = await openRepoForCall(this, args);
    const openElapsedMs = elapsedMsSince(openStartedAt);
    const indexTask = await startIndexBuildIfMissing(repoContext);
    const patterns = [args.pattern, ...args.regexp];
    const pattern = combinePatterns(patterns, args.fixed_strings);
    const searchParams = buildSearchParams(repoContext, args, pattern, patterns);
    const searchMethod =
      args.search_mode === "line_matches" ? "search/grouped_line_matches" : "search";
    const daemonStartedAt = monotonicNowMs();
    const response = await sendDaemonRequest(repoContext.backend, searchMethod, {
      ...searchParams,
      query: {
        ...searchParams.query,
        search_mode:
          searchMethod === "search/grouped_line_matches"
            ? "line_matches"
            : args.search_mode,
      },
    });
    const daemonElapsedMs = elapsedMsSince(daemonStartedAt);
    const totalElapsedMs = elapsedMsSince(startedAt);
    const timings = {
      total_ms: totalElapsedMs,
      open_repo_ms: openElapsedMs,
      daemon_search_ms: daemonElapsedMs,
    };

    return {
      daemon_addr: repoContext.addr,
      repo_id: repoContext.repoId,
      requested_root: repoContext.requestedPath,
      index_task: indexTask,
      status: response.status ?? repoContext.status,
      timings,
      response,
    };
  }

  async globFiles(arguments_) {
    const args = parseGlobArguments(arguments_);
    const repoContext = await openRepoForCall(this, args);
    const response = await sendDaemonRequest(repoContext.backend, "glob", {
      repo_id: repoContext.repoId,
      scope: buildPathScope(args, repoContext),
    });
    return {
      daemon_addr: repoContext.addr,
      repo_id: repoContext.repoId,
      requested_root: repoContext.requestedPath,
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
      requested_root: repoContext.requestedPath,
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
      requested_root: repoContext.requestedPath,
      response,
    };
  }

  async buildIndex(arguments_) {
    const args = parseRepoArguments(arguments_);
    const repoContext = await openRepoForCall(this, args);
    const task = await startIndexBuildIfMissing(repoContext);
    return {
      daemon_addr: repoContext.addr,
      repo_id: repoContext.repoId,
      requested_root: repoContext.requestedPath,
      task,
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
    repoPath: normalizeExistingPath(process.cwd()),
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
    top_k_tokens: nonNegativeOrDefault(value.top_k_tokens, 6, "top_k_tokens"),
    max_count: nullableNonNegative(value.max_count, "max_count"),
    global_max_results:
      nullableNonNegative(value.global_max_results, "global_max_results") ?? 100,
    search_mode: enumValue(
      value.search_mode,
      "search_mode",
      ["files_with_matches", "line_matches", "count_only", "count_matches"],
      "line_matches",
    ),
  };
}

async function openRepoForCall(server, args) {
  const { options } = server;
  const repoPath = resolveRepoPath(options.repoPath, args.repo_path);
  const storageRoot = resolveStorageRoot(options.storageRoot, args.storage_root);
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
    requestedPath: repoPath,
    status: result.status,
  };
}

async function startIndexBuildIfMissing(repoContext) {
  if (repoContext.status.phase !== "missing_base_snapshot") {
    return repoContext.status.active_task_id
      ? { task_id: repoContext.status.active_task_id, already_running: true }
      : null;
  }
  const result = await sendDaemonRequest(repoContext.backend, "base_snapshot/build", {
    repo_id: repoContext.repoId,
  });
  if (result.kind !== "task_started") {
    throw new Error(`unexpected base_snapshot/build response: ${JSON.stringify(result)}`);
  }
  return result.task;
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
        new Error(formatBackendExit(backend, "flashgrep backend exited")),
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
      `failed to initialize flashgrep backend within ${START_TIMEOUT_MS}ms`,
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
  if (process.env.FLASHGREP_DAEMON_BIN) {
    return process.env.FLASHGREP_DAEMON_BIN;
  }

  const bundledBinary = bundledDaemonProgram();
  if (bundledBinary && fs.existsSync(bundledBinary)) {
    return bundledBinary;
  }

  const binaryFileName = process.platform === "win32" ? "flashgrep.exe" : "flashgrep";
  const releaseBinary = path.resolve(__dirname, "../target/release", binaryFileName);
  if (fs.existsSync(releaseBinary)) {
    return releaseBinary;
  }
  const debugBinary = path.resolve(__dirname, "../target/debug", binaryFileName);
  if (fs.existsSync(debugBinary)) {
    return debugBinary;
  }
  return "flashgrep";
}

function bundledDaemonProgram() {
  const binaryName = {
    "darwin:arm64": "flashgrep-aarch64-apple-darwin",
    "darwin:x64": "flashgrep-x86_64-apple-darwin",
    "linux:arm64": "flashgrep-aarch64-unknown-linux-musl",
    "linux:x64": "flashgrep-x86_64-unknown-linux-musl",
    "win32:arm64": "flashgrep-aarch64-pc-windows-msvc.exe",
    "win32:x64": "flashgrep-x86_64-pc-windows-msvc.exe",
  }[`${process.platform}:${process.arch}`];
  return binaryName ? path.resolve(__dirname, "bin", binaryName) : null;
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
    return Promise.reject(new Error(formatBackendExit(backend, "flashgrep backend exited")));
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
      new Error("flashgrep backend closed stdio without sending a response"),
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

// Set of type names recognized by the ignore crate's TypesBuilder::add_defaults().
// See: https://github.com/BurntSushi/ripgrep/blob/master/crates/ignore/src/default_types.rs
const BUILTIN_TYPE_NAMES = new Set([
  "ada", "agda", "aidl", "alire", "amake", "asciidoc", "asm", "asp", "ats",
  "avro", "awk", "bat", "batch", "bazel", "bitbake", "boxlang", "brotli",
  "buildstream", "bzip2", "c", "cabal", "candid", "carp", "cbor", "ceylon",
  "cfml", "clojure", "cmake", "cmd", "cml", "coffeescript", "config", "coq",
  "cpp", "creole", "crystal", "cs", "csharp", "cshtml", "csproj", "css",
  "csv", "cuda", "cython", "d", "dart", "devicetree", "dhall", "diff",
  "dita", "docker", "dockercompose", "dts", "dvc", "ebuild", "edn", "elisp",
  "elixir", "elm", "erb", "erlang", "fennel", "fidl", "fish", "flatbuffers",
  "fortran", "fsharp", "fut", "gap", "gdscript", "gleam", "gn", "go",
  "gprbuild", "gradle", "graphql", "groovy", "gzip", "h", "haml", "hare",
  "haskell", "hbs", "hs", "html", "hy", "idris", "janet", "java", "jinja",
  "jl", "js", "json", "jsonl", "julia", "jupyter", "k", "kconfig", "kotlin",
  "lean", "less", "license", "lilypond", "lisp", "llvm", "lock", "log",
  "lua", "lz4", "lzma", "m4", "make", "mako", "man", "markdown", "md",
  "matlab", "meson", "minified", "mint", "mk", "ml", "motoko", "msbuild",
  "nim", "nix", "objc", "objcpp", "ocaml", "org", "pants", "pascal", "pdf",
  "perl", "php", "po", "pod", "postscript", "prolog", "protobuf", "ps",
  "puppet", "purs", "py", "python", "qmake", "qml", "qrc", "qui", "r",
  "racket", "raku", "rdoc", "readme", "reasonml", "red", "rescript",
  "robot", "rst", "ruby", "rust", "sass", "scala", "scdoc", "seed7", "sh",
  "slim", "smarty", "sml", "solidity", "soy", "spark", "spec", "sql", "ssa",
  "stylus", "sv", "svelte", "svg", "swift", "swig", "systemd", "taskpaper",
  "tcl", "tex", "texinfo", "textile", "tf", "thrift", "toml", "ts",
  "typescript", "twig", "txt", "typoscript", "typst", "usd", "v", "vala",
  "vb", "vcl", "verilog", "vhdl", "vim", "vimscript", "vue", "webidl",
  "wgsl", "wiki", "xml", "xz", "yacc", "yaml", "yang", "z", "zig", "zsh",
  "zstd",
]);

function buildPathScope(args, repoContext) {
  const typeAdd = [...args.type_add];
  const types = [];
  for (const name of args.file_type) {
    if (BUILTIN_TYPE_NAMES.has(name)) {
      types.push(name);
    } else {
      // Unrecognized type name: treat as a glob extension and register it
      // via type_add so that TypesBuilder can resolve it.
      typeAdd.push(`${name}:*.${name}`);
      types.push(name);
    }
  }
  const roots = args.roots.length > 0
    ? args.roots.map((root) =>
        path.isAbsolute(root) ? root : path.resolve(repoContext.requestedPath, root),
      )
    : defaultScopeRoots(repoContext);
  return {
    roots,
    globs: args.glob,
    iglobs: args.iglob,
    type_add: typeAdd,
    type_clear: args.type_clear,
    types,
    type_not: args.file_type_not,
  };
}

function defaultScopeRoots(repoContext) {
  const repoRoot = normalizePathForComparison(repoContext.status.repo_path);
  const requestedRoot = normalizePathForComparison(repoContext.requestedPath);
  return requestedRoot === repoRoot ? [] : [repoContext.requestedPath];
}

function normalizePathForComparison(value) {
  const normalized = path.resolve(value);
  return process.platform === "win32" ? normalized.toLowerCase() : normalized;
}

function buildSearchParams(repoContext, args, pattern, patterns) {
  return {
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
      top_k_tokens: args.top_k_tokens,
      max_count: args.max_count,
      global_max_results: args.global_max_results,
      search_mode: args.search_mode,
    },
    scope: buildPathScope(args, repoContext),
  };
}

function resolveRepoPath(defaultRepoPath, overrideRepoPath) {
  const repoPath = overrideRepoPath ?? defaultRepoPath;
  if (!repoPath) {
    throw new Error(
      "repo_path is required when no default repository path is configured",
    );
  }
  return repoPath;
}

function resolveStorageRoot(defaultStorageRoot, overrideStorageRoot) {
  return overrideStorageRoot ?? defaultStorageRoot;
}

function toolDefinitions() {
  return [
    {
      name: "search_code",
      title: "Search Code",
      description:
        "Run a flashgrep search against the configured repository or a repo_path override.",
      inputSchema: {
        type: "object",
        properties: {
          repo_path: schemaString(
            "Workspace path. A subdirectory reuses its Git worktree root index and becomes the default search scope.",
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
          top_k_tokens: schemaInteger("Planner token budget. Default is 6."),
          max_count: schemaNullableInteger(
            "Per-file match cap, mirroring ripgrep --max-count. In line_matches mode this limits matching lines returned per file; use global_max_results to cap total returned results.",
          ),
          global_max_results: schemaNullableInteger(
            "Global result cap for the selected search mode. Defaults to 100. Caps returned lines for line_matches, paths for files_with_matches, file_counts for count_only, and file_match_counts for count_matches.",
          ),
          search_mode: {
            type: "string",
            enum: ["files_with_matches", "line_matches", "count_only", "count_matches"],
          },
          glob: schemaStringArray("Include glob filters."),
          iglob: schemaStringArray("Case-insensitive include glob filters."),
          roots: schemaStringArray(
            "Restrict search to workspace-relative or absolute subpaths.",
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
        "Resolve visible files using flashgrep's ignore, glob, and file type rules.",
      inputSchema: {
        type: "object",
        properties: {
          repo_path: schemaString(
            "Workspace path. A subdirectory reuses its Git worktree root index and becomes the default glob scope.",
          ),
          storage_root: schemaString("Override index storage root for this call."),
          glob: schemaStringArray("Include glob filters."),
          iglob: schemaStringArray("Case-insensitive include glob filters."),
          roots: schemaStringArray(
            "Restrict globbing to workspace-relative or absolute subpaths.",
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
        "Open the repository in the session backend and return its current flashgrep status.",
      inputSchema: {
        type: "object",
        properties: {
          repo_path: schemaString(
            "Workspace path. Subdirectories are resolved to their Git worktree root index.",
          ),
          storage_root: schemaString("Override index storage root for this call."),
        },
        additionalProperties: false,
      },
    },
    {
      name: "refresh_repo",
      title: "Refresh Repo",
      description: "Ask flashgrep to refresh workspace state for the repository.",
      inputSchema: {
        type: "object",
        properties: {
          repo_path: schemaString(
            "Workspace path. Subdirectories are resolved to their Git worktree root index.",
          ),
          storage_root: schemaString("Override index storage root for this call."),
          force: schemaBool(
            "Force a refresh even if flashgrep thinks the repo is up to date.",
          ),
        },
        additionalProperties: false,
      },
    },
    {
      name: "build_index",
      title: "Build Index",
      description:
        "Start building the repository base snapshot if it is missing and return immediately.",
      inputSchema: {
        type: "object",
        properties: {
          repo_path: schemaString(
            "Workspace path. Subdirectories build and reuse their Git worktree root index.",
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
        text: formatToolText(structuredContent, isError),
      },
    ],
    structuredContent,
    isError,
  };
}

function formatToolText(structuredContent, isError) {
  if (isError) {
    return structuredContent?.error ?? JSON.stringify(structuredContent);
  }
  if (structuredContent?.response?.kind?.startsWith?.("search_")) {
    return formatSearchToolText(structuredContent);
  }
  return JSON.stringify(structuredContent, null, 2);
}

function formatSearchToolText(result) {
  const response = result.response ?? {};
  const results = response.results ?? {};
  const summary = results.summary ?? {};
  const lines = [
    `repo_id: ${result.repo_id}`,
    `backend: ${response.backend ?? "-"}`,
    `search_kind: ${response.kind ?? "-"}`,
    `timings_ms: total=${result.timings?.total_ms ?? "-"} open_repo=${result.timings?.open_repo_ms ?? "-"} daemon_search=${result.timings?.daemon_search_ms ?? "-"}`,
    `matches: lines=${results.matched_lines ?? 0} occurrences=${results.matched_occurrences ?? 0} files=${summary.files_with_matches ?? resultFileCount(results)} limit_reached=${Boolean(results.limit_reached)}`,
  ];
  const files = Array.isArray(results.files) ? results.files : [];
  if (files.length > 0) {
    lines.push(`files_preview: first ${Math.min(files.length, MCP_TEXT_FILE_PREVIEW_LIMIT)} of ${files.length}`);
    for (const entry of files.slice(0, MCP_TEXT_FILE_PREVIEW_LIMIT)) {
      const [filePath, lineNumbers] = entry;
      lines.push(`- ${filePath}: ${Array.isArray(lineNumbers) ? lineNumbers.join(",") : ""}`);
    }
  }
  const matchedPaths = Array.isArray(results.matched_paths) ? results.matched_paths : [];
  if (matchedPaths.length > 0) {
    lines.push(`paths_preview: first ${Math.min(matchedPaths.length, MCP_TEXT_FILE_PREVIEW_LIMIT)} of ${matchedPaths.length}`);
    for (const filePath of matchedPaths.slice(0, MCP_TEXT_FILE_PREVIEW_LIMIT)) {
      lines.push(`- ${filePath}`);
    }
  }
  const fileCounts = Array.isArray(results.file_counts) ? results.file_counts : [];
  if (fileCounts.length > 0) {
    lines.push(`file_counts_preview: first ${Math.min(fileCounts.length, MCP_TEXT_FILE_PREVIEW_LIMIT)} of ${fileCounts.length}`);
    for (const item of fileCounts.slice(0, MCP_TEXT_FILE_PREVIEW_LIMIT)) {
      lines.push(`- ${item.path}: ${item.matched_lines}`);
    }
  }
  const fileMatchCounts = Array.isArray(results.file_match_counts)
    ? results.file_match_counts
    : [];
  if (fileMatchCounts.length > 0) {
    lines.push(`file_match_counts_preview: first ${Math.min(fileMatchCounts.length, MCP_TEXT_FILE_PREVIEW_LIMIT)} of ${fileMatchCounts.length}`);
    for (const item of fileMatchCounts.slice(0, MCP_TEXT_FILE_PREVIEW_LIMIT)) {
      lines.push(`- ${item.path}: ${item.matched_occurrences}`);
    }
  }
  lines.push("Full machine-readable results are available in structuredContent.");
  return lines.join("\n");
}

function resultFileCount(results) {
  if (Array.isArray(results.files)) {
    return results.files.length;
  }
  if (Array.isArray(results.matched_paths)) {
    return results.matched_paths.length;
  }
  if (Array.isArray(results.file_counts)) {
    return results.file_counts.length;
  }
  if (Array.isArray(results.file_match_counts)) {
    return results.file_match_counts.length;
  }
  return 0;
}

function monotonicNowMs() {
  return Number(process.hrtime.bigint() / 1_000_000n);
}

function elapsedMsSince(startedAt) {
  return Math.max(0, monotonicNowMs() - startedAt);
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
  process.stdout.write(`Usage: node mcp/server.js [OPTIONS]

Options:
  --repo <REPO>  (defaults to the server process cwd)
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
        "timed out waiting for flashgrep backend shutdown",
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
        "timed out waiting for flashgrep backend to exit after SIGTERM",
      );
    } catch {
      backend.child.kill("SIGKILL");
      await withTimeout(
        backend.exitPromise,
        SHUTDOWN_TIMEOUT_MS,
        "timed out waiting for flashgrep backend to exit after SIGKILL",
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
