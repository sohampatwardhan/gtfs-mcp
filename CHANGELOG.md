# Changelog

## 0.2.0

Startup configuration is one static GTFS feed. `GTFS_RT_MCP_STATIC` (or `--static`) is required. Optional realtime feeds are `GTFS_RT_MCP_TRIP_UPDATES`, `GTFS_RT_MCP_VEHICLE_POSITIONS`, and `GTFS_RT_MCP_ALERTS`.

Removed:

- `GTFS_RT_MCP_FEED_SET` and `--feed-set`
- numbered `GTFS_RT_MCP_FEED_<n>_*` variables
- `GTFS_RT_MCP_FEEDS`
- the `feed_set` argument on `register_feed`, and feed-set loading

Kept: `GTFS_RT_MCP_HTTP_HEADERS`, `GTFS_RT_MCP_FEED_ID` (default `default`), `GTFS_RT_MCP_RT_TTL`, `GTFS_RT_MCP_STATIC_TTL`, and the server settings `GTFS_RT_MCP_BIND`, `GTFS_RT_MCP_ALLOWED_HOST`, and `GTFS_RT_MCP_LOG` / `RUST_LOG`. Matching CLI flags still override the feed variables. `register_feed` still adds feeds at runtime from a static zip plus optional realtime URLs.

A missing or empty static location exits with status 1 and names `GTFS_RT_MCP_STATIC`, including a placeholder URL. Malformed optional values (a non-integer or out-of-range TTL, an invalid feed id, or HTTP headers that are not a JSON object of strings) also exit with status 1. Header values are not included in that error. A configured static zip that fails to download still lets the process start and reports the feed as not ready.

No agency endpoint is hardcoded. Live checks read their URLs from the environment and skip when `GTFS_RT_MCP_STATIC` is unset.

## 0.1.0

Initial MCP server for GTFS static schedules and GTFS-Realtime, including feed-set documents, numbered feeds, and `GTFS_RT_MCP_FEEDS`.
