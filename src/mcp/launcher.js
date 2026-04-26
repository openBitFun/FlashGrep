#!/usr/bin/env node

const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");

const logDir = path.resolve(__dirname, "logs");
const logPath = path.join(logDir, "trae-mcp.log");

fs.mkdirSync(logDir, { recursive: true });

function log(message) {
  const line = `[${new Date().toISOString()}] ${message}\n`;
  fs.appendFileSync(logPath, line);
}

function truncate(value, limit = 4000) {
  if (value.length <= limit) {
    return value;
  }
  return `${value.slice(0, limit)}...<truncated>`;
}

log(`launcher start argv=${JSON.stringify(process.argv.slice(2))}`);
log(`cwd=${process.cwd()}`);
log(`execPath=${process.execPath}`);

const serverPath = path.resolve(__dirname, "server.js");
const child = spawn(process.execPath, [serverPath, ...process.argv.slice(2)], {
  stdio: ["pipe", "pipe", "pipe"],
  env: process.env,
});

child.on("spawn", () => {
  log(`spawned child pid=${child.pid}`);
});

child.on("error", (error) => {
  log(`child process error=${error.stack || error.message}`);
});

child.on("exit", (code, signal) => {
  log(`child exit code=${code} signal=${signal}`);
});

process.stdin.on("data", (chunk) => {
  log(`stdin ${chunk.length} bytes: ${truncate(chunk.toString("utf8"))}`);
  child.stdin.write(chunk);
});

process.stdin.on("end", () => {
  log("stdin end");
  child.stdin.end();
});

child.stdout.on("data", (chunk) => {
  log(`stdout ${chunk.length} bytes: ${truncate(chunk.toString("utf8"))}`);
  process.stdout.write(chunk);
});

child.stderr.on("data", (chunk) => {
  log(`stderr ${chunk.length} bytes: ${truncate(chunk.toString("utf8"))}`);
  process.stderr.write(chunk);
});

process.on("SIGINT", () => {
  log("launcher received SIGINT");
  child.kill("SIGINT");
});

process.on("SIGTERM", () => {
  log("launcher received SIGTERM");
  child.kill("SIGTERM");
});
