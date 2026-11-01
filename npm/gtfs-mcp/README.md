# gtfs-mcp

MCP server for GTFS static schedules and optional GTFS-Realtime feeds.

```bash
npx -y gtfs-mcp --version
```

`GTFS_MCP_STATIC` is required. Realtime URLs are optional. The values below are placeholders.

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

Source and the other install methods: https://github.com/sohampatwardhan/gtfs-mcp
