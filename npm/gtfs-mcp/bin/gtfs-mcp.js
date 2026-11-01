#!/usr/bin/env node
"use strict";

const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");

const { ensureBinary } = require("../lib/release");

const pkg = require("../package.json");

function run(binary, args) {
  const child = spawn(binary, args, {
    stdio: "inherit",
    env: process.env,
    windowsHide: false,
  });
  child.on("error", (error) => {
    console.error(`gtfs-mcp: failed to start ${binary}: ${error.message}`);
    process.exit(1);
  });
  child.on("exit", (code, signal) => {
    if (signal) {
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code === null ? 1 : code);
  });
}

async function main() {
  const override = (process.env.GTFS_MCP_BINARY || "").trim();
  if (override) {
    if (!fs.existsSync(override)) {
      console.error(`gtfs-mcp: GTFS_MCP_BINARY does not exist: ${override}`);
      process.exit(1);
    }
    run(override, process.argv.slice(2));
    return;
  }
  try {
    const binary = await ensureBinary({
      version: pkg.version,
      platform: process.platform,
      arch: process.arch,
      env: process.env,
    });
    run(binary, process.argv.slice(2));
  } catch (error) {
    console.error(`gtfs-mcp: ${error.message}`);
    process.exit(1);
  }
}

if (require.main === module) {
  main();
}

module.exports = { main, run };
