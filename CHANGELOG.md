# Changelog

## Unreleased

- No container image is published.
- npmjs.org publishes one package, unscoped `gtfs-mcp`. On first run it downloads the matching GitHub Release archive, verifies `sha256sums.txt`, and caches the binary. `GTFS_MCP_BINARY` selects a local binary instead.
- The same launcher is also published to GitHub Packages as `@sohampatwardhan/gtfs-mcp`, from a temporary copy. The npmjs.org package name stays `gtfs-mcp`.

## 0.2.0

This version renames the project from `gtfs-rt-mcp` to `gtfs-mcp` and simplifies startup configuration. There is no compatibility alias for the old crate, npm package, binary, or `GTFS_RT_MCP_*` environment variables.

- The crate, binary, MCP server name, and npm package are `gtfs-mcp`. The library crate is `gtfs_mcp`.
- Environment variables use the `GTFS_MCP_` prefix. `GTFS_MCP_STATIC` is required. Realtime URLs are optional.
- The repository is https://github.com/sohampatwardhan/gtfs-mcp

Startup configuration is one static GTFS feed. `GTFS_MCP_STATIC` (or `--static`) is required. Optional realtime feeds are `GTFS_MCP_TRIP_UPDATES`, `GTFS_MCP_VEHICLE_POSITIONS`, and `GTFS_MCP_ALERTS`.

Removed from the unpublished 0.1.0 configuration (those variables used the old `GTFS_RT_MCP_` prefix):

- `GTFS_RT_MCP_FEED_SET` and `--feed-set`
- numbered `GTFS_RT_MCP_FEED_<n>_*` variables
- `GTFS_RT_MCP_FEEDS`
- the `feed_set` argument on `register_feed`, and feed-set loading

Kept, under the new prefix: `GTFS_MCP_HTTP_HEADERS`, `GTFS_MCP_FEED_ID` (default `default`), `GTFS_MCP_RT_TTL`, `GTFS_MCP_STATIC_TTL`, and the server settings `GTFS_MCP_BIND`, `GTFS_MCP_ALLOWED_HOST`, and `GTFS_MCP_LOG` / `RUST_LOG`. Matching CLI flags still override the feed variables. `register_feed` still adds feeds at runtime from a static zip plus optional realtime URLs.

A missing or empty static location exits with status 1 and names `GTFS_MCP_STATIC`, including a placeholder URL. Malformed optional values (a non-integer or out-of-range TTL, an invalid feed id, or HTTP headers that are not a JSON object of strings) also exit with status 1. Header values are not included in that error. A configured static zip that fails to download still lets the process start and reports the feed as not ready.

No agency endpoint is hardcoded. Live checks read their URLs from the environment and skip when `GTFS_MCP_STATIC` is unset.

## 0.1.0

Initial MCP server for GTFS static schedules and GTFS-Realtime, including feed-set documents, numbered feeds, and a JSON feed list. This version was not published. The crate name at that point was `gtfs-rt-mcp`.
