# gtfs-mcp

MCP server for GTFS static schedules and optional GTFS-Realtime feeds.

```bash
npx -y gtfs-mcp --version
```

The first run downloads the binary for this package version from GitHub Releases, checks it against `sha256sums.txt`, and caches it. `GTFS_MCP_BINARY` points at a local binary instead. `HTTPS_PROXY` is used when it is set.

`GTFS_MCP_STATIC` is required. The URLs below are placeholders.

```json
{
  "mcpServers": {
    "gtfs": {
      "command": "npx",
      "args": ["-y", "gtfs-mcp", "stdio"],
      "env": {
        "GTFS_MCP_STATIC": "https://example.com/gtfs.zip",
        "GTFS_MCP_TRIP_UPDATES": "https://example.com/realtime/trip-updates.pb",
        "GTFS_MCP_VEHICLE_POSITIONS": "https://example.com/realtime/vehicle-positions.pb",
        "GTFS_MCP_ALERTS": "https://example.com/realtime/alerts.pb"
      }
    }
  }
}
```

Source: https://github.com/sohampatwardhan/gtfs-mcp
