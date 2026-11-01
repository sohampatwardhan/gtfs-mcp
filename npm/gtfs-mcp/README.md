# gtfs-mcp

[![npm](https://img.shields.io/npm/v/gtfs-mcp)](https://www.npmjs.com/package/gtfs-mcp)
[![crates.io](https://img.shields.io/crates/v/gtfs-mcp)](https://crates.io/crates/gtfs-mcp)
[![docs.rs](https://docs.rs/gtfs-mcp/badge.svg)](https://docs.rs/gtfs-mcp)

MCP server for GTFS static schedules and optional GTFS-Realtime feeds. The current release is 0.3.0. Source and the full guide: https://github.com/sohampatwardhan/gtfs-mcp

## Install as an MCP server

Quick start:

```bash
npx -y gtfs-mcp --version
```

The package is a launcher with no install scripts. The first run downloads the binary for this version from GitHub Releases, checks `sha256sums.txt`, and caches it. `GTFS_MCP_BINARY` points at a local binary and skips that download. `HTTPS_PROXY` (or `https_proxy`, then `HTTP_PROXY`) is used when set. `NO_PROXY` skips the proxy for matching hosts.

Serving MCP needs a transport. `stdio` speaks MCP on stdin and stdout. `http` serves streamable HTTP. `npx -y gtfs-mcp` with no subcommand exits.

```bash
GTFS_MCP_STATIC=https://example.com/gtfs.zip npx -y gtfs-mcp stdio
```

`GTFS_MCP_STATIC` is required. The other variables are optional. URLs are placeholders on `example.com`. `replace-me` is not a real token. Header values are never logged.

| Variable | Required | Meaning |
| --- | --- | --- |
| `GTFS_MCP_STATIC` | yes | GTFS static zip URL or path. |
| `GTFS_MCP_TRIP_UPDATES` | no | TripUpdates protobuf URL or path. |
| `GTFS_MCP_VEHICLE_POSITIONS` | no | VehiclePositions protobuf URL or path. |
| `GTFS_MCP_ALERTS` | no | Alerts protobuf URL or path. |
| `GTFS_MCP_HTTP_HEADERS` | no | JSON object of HTTP headers, for example `{"x-api-key":"replace-me"}`. |

Open feeds leave `GTFS_MCP_HTTP_HEADERS` unset. The server does not exchange a username and password for a token. One header map is sent on every HTTP fetch for the startup feed (the static zip and each realtime protobuf). A key in the query string is sent as part of the URL and shows up in `feed_health` and `list_feeds` under `source`.

### Claude Desktop

`claude_desktop_config.json` (`~/Library/Application Support/Claude/claude_desktop_config.json` on macOS, `%APPDATA%\Claude\claude_desktop_config.json` on Windows):

```json
{
  "mcpServers": {
    "gtfs-mcp": {
      "command": "npx",
      "args": ["-y", "gtfs-mcp", "stdio"],
      "env": {
        "GTFS_MCP_STATIC": "https://example.com/gtfs.zip",
        "GTFS_MCP_TRIP_UPDATES": "https://example.com/realtime/trip-updates.pb",
        "GTFS_MCP_VEHICLE_POSITIONS": "https://example.com/realtime/vehicle-positions.pb",
        "GTFS_MCP_ALERTS": "https://example.com/realtime/alerts.pb",
        "GTFS_MCP_HTTP_HEADERS": "{\"x-api-key\":\"replace-me\"}"
      }
    }
  }
}
```

### Claude Code

```bash
claude mcp add gtfs-mcp \
  -e GTFS_MCP_STATIC=https://example.com/gtfs.zip \
  -e GTFS_MCP_TRIP_UPDATES=https://example.com/realtime/trip-updates.pb \
  -e GTFS_MCP_VEHICLE_POSITIONS=https://example.com/realtime/vehicle-positions.pb \
  -e GTFS_MCP_ALERTS=https://example.com/realtime/alerts.pb \
  -e 'GTFS_MCP_HTTP_HEADERS={"x-api-key":"replace-me"}' \
  -- npx -y gtfs-mcp stdio
```

Required variable only:

```bash
claude mcp add gtfs-mcp -e GTFS_MCP_STATIC=https://example.com/gtfs.zip -- npx -y gtfs-mcp stdio
```

### Cursor

`.cursor/mcp.json` or `~/.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "gtfs-mcp": {
      "command": "npx",
      "args": ["-y", "gtfs-mcp", "stdio"],
      "env": {
        "GTFS_MCP_STATIC": "https://example.com/gtfs.zip",
        "GTFS_MCP_TRIP_UPDATES": "https://example.com/realtime/trip-updates.pb",
        "GTFS_MCP_VEHICLE_POSITIONS": "https://example.com/realtime/vehicle-positions.pb",
        "GTFS_MCP_ALERTS": "https://example.com/realtime/alerts.pb",
        "GTFS_MCP_HTTP_HEADERS": "{\"x-api-key\":\"replace-me\"}"
      }
    }
  }
}
```

### VS Code

`.vscode/mcp.json` uses `servers` and `"type": "stdio"`:

```json
{
  "servers": {
    "gtfs-mcp": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "gtfs-mcp", "stdio"],
      "env": {
        "GTFS_MCP_STATIC": "https://example.com/gtfs.zip",
        "GTFS_MCP_TRIP_UPDATES": "https://example.com/realtime/trip-updates.pb",
        "GTFS_MCP_VEHICLE_POSITIONS": "https://example.com/realtime/vehicle-positions.pb",
        "GTFS_MCP_ALERTS": "https://example.com/realtime/alerts.pb",
        "GTFS_MCP_HTTP_HEADERS": "{\"x-api-key\":\"replace-me\"}"
      }
    }
  }
}
```

### Windsurf

`~/.codeium/windsurf/mcp_config.json`:

```json
{
  "mcpServers": {
    "gtfs-mcp": {
      "command": "npx",
      "args": ["-y", "gtfs-mcp", "stdio"],
      "env": {
        "GTFS_MCP_STATIC": "https://example.com/gtfs.zip",
        "GTFS_MCP_TRIP_UPDATES": "https://example.com/realtime/trip-updates.pb",
        "GTFS_MCP_VEHICLE_POSITIONS": "https://example.com/realtime/vehicle-positions.pb",
        "GTFS_MCP_ALERTS": "https://example.com/realtime/alerts.pb",
        "GTFS_MCP_HTTP_HEADERS": "{\"x-api-key\":\"replace-me\"}"
      }
    }
  }
}
```

### Codex CLI

`~/.codex/config.toml`:

```toml
[mcp_servers.gtfs-mcp]
command = "npx"
args = ["-y", "gtfs-mcp", "stdio"]

[mcp_servers.gtfs-mcp.env]
GTFS_MCP_STATIC = "https://example.com/gtfs.zip"
GTFS_MCP_TRIP_UPDATES = "https://example.com/realtime/trip-updates.pb"
GTFS_MCP_VEHICLE_POSITIONS = "https://example.com/realtime/vehicle-positions.pb"
GTFS_MCP_ALERTS = "https://example.com/realtime/alerts.pb"
GTFS_MCP_HTTP_HEADERS = '{"x-api-key":"replace-me"}'
```

### Streamable HTTP

`http` listens on `127.0.0.1:8091`. MCP is `http://127.0.0.1:8091/mcp`. Liveness is `GET /healthz`. Put the feed variables on this process. `Host` must be `localhost`, `127.0.0.1`, or `::1` unless you pass `--allowed-host`.

```bash
GTFS_MCP_STATIC=https://example.com/gtfs.zip \
GTFS_MCP_TRIP_UPDATES=https://example.com/realtime/trip-updates.pb \
GTFS_MCP_VEHICLE_POSITIONS=https://example.com/realtime/vehicle-positions.pb \
GTFS_MCP_ALERTS=https://example.com/realtime/alerts.pb \
GTFS_MCP_HTTP_HEADERS='{"x-api-key":"replace-me"}' \
npx -y gtfs-mcp http
```

Claude Desktop and Cursor:

```json
{
  "mcpServers": {
    "gtfs-mcp": {
      "url": "http://127.0.0.1:8091/mcp"
    }
  }
}
```

Claude Code:

```bash
claude mcp add --transport http gtfs-mcp http://127.0.0.1:8091/mcp
```

VS Code `.vscode/mcp.json`:

```json
{
  "servers": {
    "gtfs-mcp": {
      "type": "http",
      "url": "http://127.0.0.1:8091/mcp"
    }
  }
}
```

Windsurf:

```json
{
  "mcpServers": {
    "gtfs-mcp": {
      "serverUrl": "http://127.0.0.1:8091/mcp"
    }
  }
}
```

Codex CLI:

```toml
[mcp_servers.gtfs-mcp]
url = "http://127.0.0.1:8091/mcp"
```

## Other ways to install

```bash
cargo install gtfs-mcp --locked
```

Release archives and installers are on the [GitHub Releases](https://github.com/sohampatwardhan/gtfs-mcp/releases) page. `GTFS_MCP_BINARY=/path/to/gtfs-mcp` uses that binary instead of the downloaded one.

The same launcher is published as `@sohampatwardhan/gtfs-mcp` on GitHub Packages. `npx -y gtfs-mcp` on npmjs.org is the usual install. To use the scoped package, add this to `.npmrc` (the token needs `read:packages`):

```text
@sohampatwardhan:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=YOUR_GITHUB_TOKEN
```

```bash
npx -y @sohampatwardhan/gtfs-mcp --version
```
