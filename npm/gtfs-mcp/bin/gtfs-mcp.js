#!/usr/bin/env node
"use strict";

const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");

const packages = {
  "linux-x64": "@sohampatwardhan/gtfs-mcp-linux-x64",
  "linux-arm64": "@sohampatwardhan/gtfs-mcp-linux-arm64",
  "darwin-x64": "@sohampatwardhan/gtfs-mcp-darwin-x64",
  "darwin-arm64": "@sohampatwardhan/gtfs-mcp-darwin-arm64",
  "win32-x64": "@sohampatwardhan/gtfs-mcp-win32-x64",
};

const key = `${process.platform}-${process.arch}`;
const pkg = packages[key];
if (!pkg) {
  console.error(`gtfs-mcp: unsupported platform ${key}`);
  process.exit(1);
}

let bin;
try {
  const pkgJson = require.resolve(`${pkg}/package.json`);
  const exe = process.platform === "win32" ? "gtfs-mcp.exe" : "gtfs-mcp";
  bin = path.join(path.dirname(pkgJson), exe);
} catch (error) {
  console.error(
    `gtfs-mcp: optional dependency ${pkg} is not installed (${error.message})`,
  );
  process.exit(1);
}

if (!fs.existsSync(bin)) {
  console.error(`gtfs-mcp: ${bin} is missing from ${pkg}`);
  process.exit(1);
}

const child = spawn(bin, process.argv.slice(2), {
  stdio: "inherit",
  env: process.env,
  windowsHide: false,
});

child.on("error", (error) => {
  console.error(`gtfs-mcp: failed to start ${bin}: ${error.message}`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code === null ? 1 : code);
});
