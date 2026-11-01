"use strict";

const crypto = require("crypto");
const fs = require("fs");
const http = require("http");
const https = require("https");
const os = require("os");
const path = require("path");
const zlib = require("zlib");
const { URL } = require("url");

const REPO = "sohampatwardhan/gtfs-mcp";

const TARGETS = {
  "linux-x64": {
    target: "x86_64-unknown-linux-musl",
    ext: "tar.gz",
    binary: "gtfs-mcp",
  },
  "linux-arm64": {
    target: "aarch64-unknown-linux-musl",
    ext: "tar.gz",
    binary: "gtfs-mcp",
  },
  "darwin-x64": {
    target: "x86_64-apple-darwin",
    ext: "tar.gz",
    binary: "gtfs-mcp",
  },
  "darwin-arm64": {
    target: "aarch64-apple-darwin",
    ext: "tar.gz",
    binary: "gtfs-mcp",
  },
  "win32-x64": {
    target: "x86_64-pc-windows-msvc",
    ext: "zip",
    binary: "gtfs-mcp.exe",
  },
};

function targetForPlatform(platform, arch) {
  return TARGETS[`${platform}-${arch}`] || null;
}

function archiveName(spec) {
  return `gtfs-mcp-${spec.target}.${spec.ext}`;
}

function releaseAssetUrl(version, name) {
  return `https://github.com/${REPO}/releases/download/v${version}/${name}`;
}

function parseChecksum(sumsText, filename) {
  for (const line of String(sumsText).split(/\r?\n/)) {
    const match = /^([0-9a-fA-F]{64})  (\S+)$/.exec(line.trim());
    if (match && match[2] === filename) {
      return match[1].toLowerCase();
    }
  }
  return null;
}

function sha256(bytes) {
  return crypto.createHash("sha256").update(bytes).digest("hex");
}

function checksumMatches(bytes, expectedHex) {
  if (!expectedHex) return false;
  return sha256(bytes) === expectedHex.toLowerCase();
}

function cacheDir(version, env = process.env, platform = process.platform) {
  let root;
  if (platform === "win32") {
    root = env.LOCALAPPDATA || path.join(os.homedir(), "AppData", "Local");
    return path.join(root, "gtfs-mcp", "cache", version);
  }
  if (platform === "darwin") {
    return path.join(os.homedir(), "Library", "Caches", "gtfs-mcp", version);
  }
  const xdg = env.XDG_CACHE_HOME || path.join(os.homedir(), ".cache");
  return path.join(xdg, "gtfs-mcp", version);
}

function proxyFor(urlString, env = process.env) {
  const proxy =
    env.HTTPS_PROXY || env.https_proxy || env.HTTP_PROXY || env.http_proxy || "";
  if (!proxy) return "";
  const host = new URL(urlString).hostname;
  const noProxy = env.NO_PROXY || env.no_proxy || "";
  const skipped = noProxy
    .split(",")
    .map((item) => item.trim())
    .filter(Boolean);
  for (const item of skipped) {
    if (item === "*") return "";
    const bare = item.replace(/^\./, "");
    if (host === bare || host.endsWith(`.${bare}`)) return "";
  }
  return proxy;
}

function extractTarGz(buffer, wantedName) {
  const tar = zlib.gunzipSync(buffer);
  let offset = 0;
  while (offset + 512 <= tar.length) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) break;
    const name = header.subarray(0, 100).toString("utf8").replace(/\0.*$/, "");
    const sizeText = header.subarray(124, 136).toString("utf8").replace(/\0.*$/, "").trim();
    const size = Number.parseInt(sizeText || "0", 8);
    offset += 512;
    const data = tar.subarray(offset, offset + size);
    offset += Math.ceil(size / 512) * 512;
    if (name.split("/").pop() === wantedName) return Buffer.from(data);
  }
  throw new Error(`archive has no ${wantedName}`);
}

function extractZip(buffer, wantedName) {
  let offset = 0;
  while (offset + 30 <= buffer.length) {
    const signature = buffer.readUInt32LE(offset);
    if (signature !== 0x04034b50) break;
    const flags = buffer.readUInt16LE(offset + 6);
    const method = buffer.readUInt16LE(offset + 8);
    let compSize = buffer.readUInt32LE(offset + 18);
    const nameLen = buffer.readUInt16LE(offset + 26);
    const extraLen = buffer.readUInt16LE(offset + 28);
    const nameStart = offset + 30;
    const name = buffer.subarray(nameStart, nameStart + nameLen).toString("utf8");
    let dataStart = nameStart + nameLen + extraLen;
    if (flags & 0x8 && compSize === 0) {
      throw new Error("zip data descriptors are not supported");
    }
    const data = buffer.subarray(dataStart, dataStart + compSize);
    offset = dataStart + compSize;
    if (name.split("/").pop() !== wantedName) continue;
    if (method === 0) return Buffer.from(data);
    if (method === 8) return zlib.inflateRawSync(data);
    throw new Error(`unsupported zip method ${method}`);
  }
  throw new Error(`archive has no ${wantedName}`);
}

function extractArchive(buffer, spec) {
  if (spec.ext === "tar.gz") return extractTarGz(buffer, spec.binary);
  if (spec.ext === "zip") return extractZip(buffer, spec.binary);
  throw new Error(`unsupported archive ${spec.ext}`);
}

function downloadBuffer(urlString, env = process.env, redirectsLeft = 5) {
  return new Promise((resolve, reject) => {
    let url;
    try {
      url = new URL(urlString);
    } catch (error) {
      reject(error);
      return;
    }
    const proxy = proxyFor(urlString, env);
    const onResponse = (response) => {
      const status = response.statusCode || 0;
      if (status >= 300 && status < 400 && response.headers.location) {
        response.resume();
        if (redirectsLeft <= 0) {
          reject(new Error(`too many redirects for ${urlString}`));
          return;
        }
        const next = new URL(response.headers.location, url).href;
        resolve(downloadBuffer(next, env, redirectsLeft - 1));
        return;
      }
      if (status !== 200) {
        response.resume();
        reject(new Error(`GET ${urlString} returned ${status}`));
        return;
      }
      const chunks = [];
      response.on("data", (chunk) => chunks.push(chunk));
      response.on("end", () => resolve(Buffer.concat(chunks)));
      response.on("error", reject);
    };
    const headers = {
      "User-Agent": "gtfs-mcp",
      Accept: "application/octet-stream",
    };
    if (!proxy) {
      const lib = url.protocol === "https:" ? https : http;
      const request = lib.get(url, { headers }, onResponse);
      request.setTimeout(120000, () => request.destroy(new Error(`timeout ${urlString}`)));
      request.on("error", reject);
      return;
    }
    let proxyUrl;
    try {
      proxyUrl = new URL(proxy);
    } catch (error) {
      reject(error);
      return;
    }
    const port = url.port || (url.protocol === "https:" ? 443 : 80);
    const connectHeaders = {};
    if (proxyUrl.username) {
      const user = decodeURIComponent(proxyUrl.username);
      const password = decodeURIComponent(proxyUrl.password);
      connectHeaders["Proxy-Authorization"] =
        "Basic " + Buffer.from(`${user}:${password}`).toString("base64");
    }
    const connect = http.request({
      host: proxyUrl.hostname,
      port: proxyUrl.port || 80,
      method: "CONNECT",
      path: `${url.hostname}:${port}`,
      headers: connectHeaders,
    });
    connect.setTimeout(120000, () => connect.destroy(new Error("proxy timeout")));
    connect.on("connect", (response, socket) => {
      if (response.statusCode !== 200) {
        socket.destroy();
        reject(new Error(`proxy CONNECT returned ${response.statusCode}`));
        return;
      }
      const request = https.request(
        {
          host: url.hostname,
          servername: url.hostname,
          path: `${url.pathname}${url.search}`,
          method: "GET",
          headers: { ...headers, Host: url.host },
          socket,
          agent: false,
        },
        onResponse,
      );
      request.on("error", reject);
      request.end();
    });
    connect.on("error", reject);
    connect.end();
  });
}

async function ensureBinary(options) {
  const version = options.version;
  const spec = targetForPlatform(options.platform, options.arch);
  if (!spec) {
    throw new Error(`unsupported platform ${options.platform}-${options.arch}`);
  }
  const dir = cacheDir(version, options.env, options.platform);
  const dest = path.join(dir, spec.binary);
  if (fs.existsSync(dest)) return dest;
  const archive = archiveName(spec);
  const assetUrl = releaseAssetUrl(version, archive);
  const sumsUrl = releaseAssetUrl(version, "sha256sums.txt");
  const [archiveBytes, sumsBytes] = await Promise.all([
    downloadBuffer(assetUrl, options.env),
    downloadBuffer(sumsUrl, options.env),
  ]);
  const expected = parseChecksum(sumsBytes.toString("utf8"), archive);
  if (!expected) {
    throw new Error(`sha256sums.txt has no entry for ${archive}`);
  }
  if (!checksumMatches(archiveBytes, expected)) {
    throw new Error(`checksum mismatch for ${archive}`);
  }
  const binary = extractArchive(archiveBytes, spec);
  fs.mkdirSync(dir, { recursive: true });
  const temporary = `${dest}.partial`;
  fs.writeFileSync(temporary, binary);
  if (options.platform !== "win32") fs.chmodSync(temporary, 0o755);
  fs.renameSync(temporary, dest);
  return dest;
}

module.exports = {
  TARGETS,
  targetForPlatform,
  archiveName,
  releaseAssetUrl,
  parseChecksum,
  sha256,
  checksumMatches,
  cacheDir,
  proxyFor,
  extractTarGz,
  extractZip,
  extractArchive,
  downloadBuffer,
  ensureBinary,
};
