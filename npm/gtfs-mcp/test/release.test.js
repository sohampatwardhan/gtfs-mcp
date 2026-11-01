"use strict";

const assert = require("node:assert/strict");
const crypto = require("crypto");
const test = require("node:test");
const zlib = require("zlib");

const {
  archiveName,
  cacheDir,
  checksumMatches,
  extractArchive,
  extractTarGz,
  extractZip,
  parseChecksum,
  proxyFor,
  releaseAssetUrl,
  sha256,
  targetForPlatform,
} = require("../lib/release");

test("maps each supported platform to a release archive", () => {
  const cases = [
    ["linux", "x64", "x86_64-unknown-linux-musl", "tar.gz", "gtfs-mcp"],
    ["linux", "arm64", "aarch64-unknown-linux-musl", "tar.gz", "gtfs-mcp"],
    ["darwin", "x64", "x86_64-apple-darwin", "tar.gz", "gtfs-mcp"],
    ["darwin", "arm64", "aarch64-apple-darwin", "tar.gz", "gtfs-mcp"],
    ["win32", "x64", "x86_64-pc-windows-msvc", "zip", "gtfs-mcp.exe"],
  ];
  for (const [platform, arch, target, ext, binary] of cases) {
    const spec = targetForPlatform(platform, arch);
    assert.deepEqual(spec, { target, ext, binary });
    assert.equal(archiveName(spec), `gtfs-mcp-${target}.${ext}`);
  }
  assert.equal(targetForPlatform("linux", "ppc64"), null);
  assert.equal(targetForPlatform("win32", "arm64"), null);
});

test("release URLs use the package version", () => {
  assert.equal(
    releaseAssetUrl("0.3.0", "sha256sums.txt"),
    "https://github.com/sohampatwardhan/gtfs-mcp/releases/download/v0.3.0/sha256sums.txt",
  );
});

test("checksum verification accepts the matching sha256sums line", () => {
  const bytes = Buffer.from("gtfs-mcp-bytes");
  const digest = sha256(bytes);
  const name = "gtfs-mcp-x86_64-unknown-linux-musl.tar.gz";
  const sums = `0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  other.txt\n${digest}  ${name}\n`;
  assert.equal(parseChecksum(sums, name), digest);
  assert.equal(parseChecksum(sums, "missing"), null);
  assert.equal(checksumMatches(bytes, digest), true);
  assert.equal(checksumMatches(bytes, digest.toUpperCase()), true);
  assert.equal(checksumMatches(bytes, "ab".repeat(32)), false);
  assert.equal(checksumMatches(Buffer.from("other"), digest), false);
});

test("cache directory is keyed by version", () => {
  const env = { XDG_CACHE_HOME: "/tmp/xdg", LOCALAPPDATA: "C:\\Users\\me\\AppData\\Local" };
  assert.equal(cacheDir("0.3.0", env, "linux"), "/tmp/xdg/gtfs-mcp/0.3.0");
  assert.match(cacheDir("0.3.0", env, "win32"), /gtfs-mcp[\\/]cache[\\/]0\.3\.0$/);
  assert.match(cacheDir("0.3.0", {}, "darwin"), /Library[\\/]Caches[\\/]gtfs-mcp[\\/]0\.3\.0$/);
});

test("HTTPS_PROXY is skipped for NO_PROXY hosts", () => {
  const env = {
    HTTPS_PROXY: "http://proxy.example:8080",
    NO_PROXY: "example.com,localhost",
  };
  assert.equal(proxyFor("https://example.com/gtfs.zip", env), "");
  assert.equal(
    proxyFor("https://github.com/sohampatwardhan/gtfs-mcp/releases/download/v0.3.0/sha256sums.txt", env),
    "http://proxy.example:8080",
  );
  assert.equal(proxyFor("https://github.com/file", {}), "");
});

function ustar(name, body) {
  const header = Buffer.alloc(512);
  header.write(name, 0, "utf8");
  header.write("0000644\0", 100);
  header.write("0000000\0", 108);
  header.write("0000000\0", 116);
  header.write(body.length.toString(8).padStart(11, "0") + "\0", 124);
  header.write("00000000000\0", 136);
  header.write("        ", 148);
  header.write("0", 156);
  header.write("ustar\0", 257);
  header.write("00", 263);
  let sum = 0;
  for (const byte of header) sum += byte;
  header.write(sum.toString(8).padStart(6, "0") + "\0 ", 148);
  const padding = Buffer.alloc((512 - (body.length % 512)) % 512);
  return zlib.gzipSync(Buffer.concat([header, body, padding, Buffer.alloc(1024)]));
}

test("extracts the binary from a gzipped ustar archive", () => {
  const body = Buffer.from("#!/bin/gtfs\n");
  const archive = ustar("gtfs-mcp", body);
  const spec = targetForPlatform("linux", "x64");
  assert.deepEqual(extractTarGz(archive, spec.binary), body);
  assert.deepEqual(extractArchive(archive, spec), body);
});

test("extracts a deflated zip entry", () => {
  const body = Buffer.from("MZ-fake-exe");
  const compressed = zlib.deflateRawSync(body);
  const name = Buffer.from("gtfs-mcp.exe");
  const header = Buffer.alloc(30);
  header.writeUInt32LE(0x04034b50, 0);
  header.writeUInt16LE(20, 4);
  header.writeUInt16LE(8, 8);
  header.writeUInt32LE(compressed.length, 18);
  header.writeUInt32LE(body.length, 22);
  header.writeUInt16LE(name.length, 26);
  const zip = Buffer.concat([header, name, compressed]);
  const spec = targetForPlatform("win32", "x64");
  assert.deepEqual(extractZip(zip, spec.binary), body);
  const digest = sha256(zip);
  assert.equal(checksumMatches(zip, digest), true);
});
