//! MCP tool router, plus stdio and streamable HTTP serving.
//!
//! [`GtfsServer`] answers GTFS schedule and realtime questions from a
//! [`crate::Catalog`]. Each tool returns compact JSON text so an agent spends
//! tokens on facts.
//!
//! [`GtfsServer::register_feed`], [`GtfsServer::list_feeds`],
//! [`GtfsServer::unregister_feed`], [`GtfsServer::refresh_feed`], and
//! [`GtfsServer::feed_health`] manage feeds. [`GtfsServer::search_stops`],
//! [`GtfsServer::search_routes`], [`GtfsServer::trip_status`],
//! [`GtfsServer::trip_schedule`], [`GtfsServer::departure_board`],
//! [`GtfsServer::vehicle_positions`], [`GtfsServer::trip_updates`], and
//! [`GtfsServer::alerts`] answer questions. [`GtfsServer::decode_protobuf`]
//! turns one GTFS-Realtime message into JSON without registering a feed.
//!
//! [`serve_stdio`] speaks MCP on standard input and output. [`serve_http`]
//! listens until Ctrl-C. [`spawn_http`] binds the same routes on a background
//! task. HTTP serving mounts MCP at `/mcp` and answers `GET /healthz` with `ok`.
//!
//! Header values sent when fetching a feed are stored for later requests and are
//! not copied into tool results.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::catalog::{Catalog, RegisterRequest};
use crate::error::{err, AppError};
use crate::query::{AlertQuery, TripUpdateQuery, VehicleQuery};

/// MCP server that answers GTFS tools from one [`crate::Catalog`].
///
/// Build one with [`GtfsServer::new`] and run it through [`serve_stdio`],
/// [`serve_http`], or [`spawn_http`]. Responses are compact JSON text.
#[derive(Clone)]
pub struct GtfsServer {
    /// Shared feed catalog the tools read and update.
    catalog: Arc<Catalog>,
    /// Router that dispatches the GTFS tools.
    tool_router: ToolRouter<Self>,
}

impl GtfsServer {
    /// Builds a server that answers tools from `catalog`.
    ///
    /// # Arguments
    ///
    /// * `catalog` - Shared [`crate::Catalog`]. The server keeps a clone of the `Arc`.
    ///
    /// # Returns
    ///
    /// A [`GtfsServer`] with the GTFS tool router installed.
    pub fn new(catalog: Arc<Catalog>) -> Self {
        Self {
            catalog,
            tool_router: Self::tool_router(),
        }
    }
}

/// Sources, cache intervals, and headers for [`GtfsServer::register_feed`].
#[derive(Debug, Deserialize, JsonSchema)]
struct RegisterArgs {
    /// Stable id, 1-64 characters [A-Za-z0-9_.:-]. Defaults to "default".
    #[serde(default)]
    feed_id: Option<String>,
    /// HTTP(S) URL of a GTFS static zip. Mutually exclusive with static_path.
    #[serde(default)]
    static_url: Option<String>,
    /// Filesystem path of a GTFS static zip.
    #[serde(default)]
    static_path: Option<String>,
    /// HTTP(S) URL of a GTFS-Realtime TripUpdates protobuf.
    #[serde(default)]
    trip_updates_url: Option<String>,
    /// Filesystem path of a TripUpdates protobuf.
    #[serde(default)]
    trip_updates_path: Option<String>,
    /// HTTP(S) URL of a VehiclePositions protobuf.
    #[serde(default)]
    vehicle_positions_url: Option<String>,
    /// Filesystem path of a VehiclePositions protobuf.
    #[serde(default)]
    vehicle_positions_path: Option<String>,
    /// HTTP(S) URL of an Alerts protobuf.
    #[serde(default)]
    alerts_url: Option<String>,
    /// Filesystem path of an Alerts protobuf.
    #[serde(default)]
    alerts_path: Option<String>,
    /// Realtime cache TTL in seconds. Default 15. Range 1-3600.
    #[serde(default)]
    rt_ttl_secs: Option<u64>,
    /// Static zip refresh interval in seconds. 0 disables the timer. Default 21600. Max 604800.
    #[serde(default)]
    static_ttl_secs: Option<u64>,
    /// Extra request headers. Names are reported by feed_health; values are never returned.
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
}

/// Feed selector for tools that act on one registered feed.
#[derive(Debug, Deserialize, JsonSchema)]
struct FeedIdArgs {
    /// Feed id. Omit when only one feed is registered.
    #[serde(default)]
    feed_id: Option<String>,
}

/// Feed, text query, and page window for stop and route search.
#[derive(Debug, Deserialize, JsonSchema)]
struct SearchArgs {
    /// Feed id. Omit when only one feed is registered.
    #[serde(default)]
    feed_id: Option<String>,
    /// stop_id, stop_code, or name substring. Empty lists the first page.
    #[serde(default)]
    query: Option<String>,
    /// Page size. Default 20, max 100.
    #[serde(default)]
    limit: Option<u32>,
    /// Rows to skip. Use next_offset from the previous page.
    #[serde(default)]
    offset: Option<u32>,
}

/// Trip, optional stop, clock, and upcoming-stop limit for [`GtfsServer::trip_status`].
#[derive(Debug, Deserialize, JsonSchema)]
struct TripStatusArgs {
    /// Feed id. Omit when only one feed is registered.
    #[serde(default)]
    feed_id: Option<String>,
    /// trip_id or trip_short_name (train number), for example "177".
    trip: String,
    /// Optional stop_id, stop_code, or stop name. Fills at_stop with delay and track.
    #[serde(default)]
    stop: Option<String>,
    /// Unix seconds. Omit to use the current time.
    #[serde(default)]
    at_unix: Option<i64>,
    /// Upcoming stops to include. Default 4, max 20.
    #[serde(default)]
    upcoming_limit: Option<u32>,
}

/// Trip, clock, and page window for [`GtfsServer::trip_schedule`].
#[derive(Debug, Deserialize, JsonSchema)]
struct ScheduleArgs {
    /// Feed id. Omit when only one feed is registered.
    #[serde(default)]
    feed_id: Option<String>,
    /// trip_id or trip_short_name.
    trip: String,
    /// Unix seconds used to pick the service date and overlay realtime. Omit for now.
    #[serde(default)]
    at_unix: Option<i64>,
    /// Page size. Default 20, max 100.
    #[serde(default)]
    limit: Option<u32>,
    /// Rows to skip.
    #[serde(default)]
    offset: Option<u32>,
}

/// Station, route filter, time window, and page window for [`GtfsServer::departure_board`].
#[derive(Debug, Deserialize, JsonSchema)]
struct BoardArgs {
    /// Feed id. Omit when only one feed is registered.
    #[serde(default)]
    feed_id: Option<String>,
    /// stop_id, stop_code, stop name, or a platform stop such as NHV:track:12.
    stop: String,
    /// Optional route_id or route_short_name filter.
    #[serde(default)]
    route: Option<String>,
    /// Unix seconds. Omit for now.
    #[serde(default)]
    at_unix: Option<i64>,
    /// How far ahead to look, in minutes. Default 120, max 1440.
    #[serde(default)]
    window_minutes: Option<u32>,
    /// How far back to include recent departures, in minutes. Default 5, max 180.
    #[serde(default)]
    past_minutes: Option<u32>,
    /// Page size. Default 20, max 100.
    #[serde(default)]
    limit: Option<u32>,
    /// Rows to skip.
    #[serde(default)]
    offset: Option<u32>,
    /// Include SKIPPED stops. Default false.
    #[serde(default)]
    include_skipped: Option<bool>,
}

/// Filters and page window for [`GtfsServer::vehicle_positions`].
#[derive(Debug, Deserialize, JsonSchema)]
struct VehicleArgs {
    /// Feed id. Omit when only one feed is registered.
    #[serde(default)]
    feed_id: Option<String>,
    /// route_id or route_short_name.
    #[serde(default)]
    route: Option<String>,
    /// trip_id or trip_short_name.
    #[serde(default)]
    trip: Option<String>,
    /// Vehicle id or label substring.
    #[serde(default)]
    vehicle: Option<String>,
    /// stop_id the vehicle currently reports.
    #[serde(default)]
    stop: Option<String>,
    /// Page size. Default 20, max 100.
    #[serde(default)]
    limit: Option<u32>,
    /// Rows to skip.
    #[serde(default)]
    offset: Option<u32>,
}

/// Filters, page window, and per-trip stop cap for [`GtfsServer::trip_updates`].
#[derive(Debug, Deserialize, JsonSchema)]
struct TripUpdateArgs {
    /// Feed id. Omit when only one feed is registered.
    #[serde(default)]
    feed_id: Option<String>,
    /// route_id or route_short_name.
    #[serde(default)]
    route: Option<String>,
    /// trip_id or trip_short_name.
    #[serde(default)]
    trip: Option<String>,
    /// Keep updates that mention this stop, by stop_id or stop_sequence.
    #[serde(default)]
    stop: Option<String>,
    /// Page size of trips. Default 20, max 100.
    #[serde(default)]
    limit: Option<u32>,
    /// Rows to skip.
    #[serde(default)]
    offset: Option<u32>,
    /// StopTimeUpdates to keep per trip. Default 8, max 40.
    #[serde(default)]
    stop_limit: Option<u32>,
}

/// Route, stop, text, and activity filters for [`GtfsServer::alerts`].
#[derive(Debug, Deserialize, JsonSchema)]
struct AlertArgs {
    /// Feed id. Omit when only one feed is registered.
    #[serde(default)]
    feed_id: Option<String>,
    /// route_id or route_short_name. Matches informed_entity.
    #[serde(default)]
    route: Option<String>,
    /// stop_id, stop_code, or stop name. Matches informed_entity, including parent and platform stops.
    #[serde(default)]
    stop: Option<String>,
    /// Case-insensitive substring of the header or description.
    #[serde(default)]
    text: Option<String>,
    /// Keep alerts whose active or impact period contains at_unix. Default true.
    #[serde(default)]
    active_only: Option<bool>,
    /// Unix seconds. Omit for now.
    #[serde(default)]
    at_unix: Option<i64>,
    /// Page size. Default 20, max 100.
    #[serde(default)]
    limit: Option<u32>,
    /// Rows to skip.
    #[serde(default)]
    offset: Option<u32>,
    /// Max characters of header and description. Default 400, max 2000.
    #[serde(default)]
    max_chars: Option<u32>,
}

/// One protobuf source, plus page and text limits, for [`GtfsServer::decode_protobuf`].
#[derive(Debug, Deserialize, JsonSchema)]
struct DecodeArgs {
    /// Filesystem path of a GTFS-Realtime protobuf. Pass exactly one of path, url, or base64.
    #[serde(default)]
    path: Option<String>,
    /// HTTP(S) URL of a GTFS-Realtime protobuf.
    #[serde(default)]
    url: Option<String>,
    /// Standard base64 of the protobuf bytes.
    #[serde(default)]
    base64: Option<String>,
    /// Entity offset. Default 0.
    #[serde(default)]
    offset: Option<u32>,
    /// Entities to return. Default 20, max 100.
    #[serde(default)]
    limit: Option<u32>,
    /// Max characters of alert text. Default 800, max 4000.
    #[serde(default)]
    max_chars: Option<u32>,
    /// Extra request headers when url is set. Values are not returned.
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
}

#[tool_router]
impl GtfsServer {
    /// Registers a static GTFS zip and optional realtime protobufs.
    ///
    /// Pass `static_url` or `static_path`, plus optional TripUpdates,
    /// VehiclePositions, and Alerts URLs or files. An existing feed with the same id
    /// is replaced, including one that failed to load from the environment. The
    /// success body is that feed's health. Header values are stored and are not
    /// echoed.
    ///
    /// # Arguments
    ///
    /// * `args` - Id, sources, TTLs, and headers. See [`RegisterArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON health for the registered feed.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when:
    ///
    /// * `feed_id` is outside 1-64 characters of ASCII letters, digits, `_`, `.`, `:`, or `-`
    /// * the static URL and path are both set, or neither is set
    /// * a realtime source sets both a URL and a path
    /// * a source uses an unsupported URL scheme
    /// * a header name is empty, contains CR, LF, or `:`, or a value contains CR or LF
    /// * the id is new and 16 feeds are already registered
    /// * a static zip or realtime protobuf cannot be fetched or parsed
    #[tool(
        description = "Register another GTFS source at runtime. Pass static_url or static_path, plus optional realtime URLs. Replaces an existing feed with the same feed_id, including one that failed to load from the environment. Returns feed health. Header values are stored and not echoed."
    )]
    async fn register_feed(
        &self,
        Parameters(args): Parameters<RegisterArgs>,
    ) -> Result<String, String> {
        let request = RegisterRequest {
            feed_id: args.feed_id,
            static_url: args.static_url,
            static_path: args.static_path,
            trip_updates_url: args.trip_updates_url,
            trip_updates_path: args.trip_updates_path,
            vehicle_positions_url: args.vehicle_positions_url,
            vehicle_positions_path: args.vehicle_positions_path,
            alerts_url: args.alerts_url,
            alerts_path: args.alerts_path,
            rt_ttl_secs: args.rt_ttl_secs,
            static_ttl_secs: args.static_ttl_secs,
            headers: args.headers.unwrap_or_default(),
        };
        finish(self.catalog.register(request).await)
    }

    /// Lists the startup feed and feeds added with [`GtfsServer::register_feed`].
    ///
    /// A feed that failed to load has `ready` set to false and an error string.
    /// Header values are omitted. Rows are sorted by feed id.
    ///
    /// # Returns
    ///
    /// Compact JSON object whose `feeds` array is the catalog listing.
    ///
    /// # Errors
    ///
    /// Does not return `Err`. The `Result` matches the other tools.
    #[tool(
        description = "List the startup feed and feeds added with register_feed. A feed that failed to load has ready:false and an error. Header values are omitted."
    )]
    async fn list_feeds(&self) -> Result<String, String> {
        Ok(compact(&self.catalog.list()))
    }

    /// Drops a registered feed and its caches.
    ///
    /// Removes a loaded feed or one that failed to load.
    ///
    /// # Arguments
    ///
    /// * `args` - Feed id. Omit it when only one feed is registered. See [`FeedIdArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON naming the removed feed.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when no feed has that
    /// id, when no feed is registered, or when several feeds are registered and
    /// `feed_id` is omitted.
    #[tool(description = "Drop a registered feed and its caches.")]
    async fn unregister_feed(
        &self,
        Parameters(args): Parameters<FeedIdArgs>,
    ) -> Result<String, String> {
        finish(self.catalog.unregister(args.feed_id))
    }

    /// Fetches the static zip and realtime protobufs again.
    ///
    /// Retries a feed that failed to load at startup from its saved request. A loaded
    /// feed is fetched again even when its TTL has not elapsed.
    ///
    /// # Arguments
    ///
    /// * `args` - Feed id. Omit it when only one feed is registered. See [`FeedIdArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON health after the refresh.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when the id does not
    /// resolve, or when a feed that has not loaded cannot be fetched or parsed. A
    /// loaded feed that fails a later refresh keeps its previous data and records the
    /// failure on the health object.
    #[tool(
        description = "Fetch the static zip and realtime protobufs again. Retries a feed that failed to load at startup."
    )]
    async fn refresh_feed(
        &self,
        Parameters(args): Parameters<FeedIdArgs>,
    ) -> Result<String, String> {
        finish(self.catalog.refresh(args.feed_id).await)
    }

    /// Reports entity counts, header timestamps, age, TTL, and the last error.
    ///
    /// Refreshes a loaded feed first when the realtime TTL has elapsed. A feed that
    /// failed at startup returns `ready` set to false and does not retry until
    /// [`GtfsServer::refresh_feed`].
    ///
    /// # Arguments
    ///
    /// * `args` - Feed id. Omit it when only one feed is registered. See [`FeedIdArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON health for the feed.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when the id does not
    /// resolve to a feed. A startup failure is a successful body with `ready` false,
    /// not an `Err`.
    #[tool(
        description = "Feed health: entity counts, header timestamps, age, TTL, and last error. Refreshes a loaded feed first when the realtime TTL has elapsed. A feed that failed at startup returns ready:false and does not retry until refresh_feed."
    )]
    async fn feed_health(
        &self,
        Parameters(args): Parameters<FeedIdArgs>,
    ) -> Result<String, String> {
        finish(self.catalog.health(args.feed_id).await)
    }

    /// Searches stops by `stop_id`, `stop_code`, or name.
    ///
    /// Exact ids and codes rank first. Each hit includes `stop_id`, name,
    /// coordinates, `location_type`, `parent_station`, and `platform_code`. An empty
    /// query lists the first page. Page with `offset` until `next_offset` is null.
    ///
    /// # Arguments
    ///
    /// * `args` - Feed id, query, and page window. See [`SearchArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON page of stops, including `total`, `offset`, and `next_offset`.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when the feed id does
    /// not name a loaded feed or the catalog cannot answer the search.
    #[tool(
        description = "Search stops by stop_id, stop_code, or name. Ranked exact ids and codes first. Returns stop_id, name, coordinates, location_type, parent_station, and platform_code."
    )]
    async fn search_stops(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<String, String> {
        finish(
            self.catalog
                .search_stops(
                    args.feed_id,
                    args.query.as_deref().unwrap_or(""),
                    limit_of(args.limit, 20, 100),
                    offset_of(args.offset),
                )
                .await,
        )
    }

    /// Searches routes by `route_id`, short name, or long name.
    ///
    /// Each hit includes `route_type` and `route_type_name`. An empty query lists the
    /// first page. Page with `offset` until `next_offset` is null.
    ///
    /// # Arguments
    ///
    /// * `args` - Feed id, query, and page window. See [`SearchArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON page of routes, including `total`, `offset`, and `next_offset`.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when the feed id does
    /// not name a loaded feed or the catalog cannot answer the search.
    #[tool(
        description = "Search routes by route_id, short name, or long name. Returns route_type and route_type_name."
    )]
    async fn search_routes(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<String, String> {
        finish(
            self.catalog
                .search_routes(
                    args.feed_id,
                    args.query.as_deref().unwrap_or(""),
                    limit_of(args.limit, 20, 100),
                    offset_of(args.offset),
                )
                .await,
        )
    }

    /// Reports where a trip is at a chosen clock time.
    ///
    /// Matches `trip_id` or `trip_short_name` and prefers the trip that has a live
    /// vehicle. The body includes `delay_label`, the vehicle position and status, and
    /// the next stops. When `stop` is set, `at_stop` carries scheduled and estimated
    /// local `HH:MM`, delay seconds, `assigned_stop_id`, `platform_code`, and track.
    ///
    /// # Arguments
    ///
    /// * `args` - Trip, optional stop, clock, and upcoming-stop limit. See
    ///   [`TripStatusArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON for the chosen trip, plus alternatives when several trips match.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when `trip` is empty,
    /// no trip matches, the optional stop does not resolve, or the feed id does not
    /// name a loaded feed.
    #[tool(
        description = "Where a trip is now. Matches trip_id or trip_short_name and prefers the trip that has a live vehicle. Returns delay_label, vehicle position and status, the next stops, and at_stop (scheduled and estimated local HH:MM, delay seconds, assigned_stop_id, platform_code, track) when stop is set."
    )]
    async fn trip_status(
        &self,
        Parameters(args): Parameters<TripStatusArgs>,
    ) -> Result<String, String> {
        if args.trip.trim().is_empty() {
            return finish(Err(err("trip is empty")));
        }
        finish(
            self.catalog
                .trip_status(
                    args.feed_id,
                    &args.trip,
                    args.stop,
                    args.at_unix,
                    limit_of(args.upcoming_limit, 4, 20),
                )
                .await,
        )
    }

    /// Returns the stop sequence for one trip.
    ///
    /// Each row has scheduled and estimated times and any realtime platform
    /// assignment. Page with `offset` until `next_offset` is null.
    ///
    /// # Arguments
    ///
    /// * `args` - Trip, clock, and page window. See [`ScheduleArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON for one trip and one page of its stops.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when `trip` is empty,
    /// no trip matches, several trips share that short name, or the feed id does not
    /// name a loaded feed.
    #[tool(
        description = "Stop sequence for one trip, with scheduled and estimated times and any realtime platform assignment. Page with offset."
    )]
    async fn trip_schedule(
        &self,
        Parameters(args): Parameters<ScheduleArgs>,
    ) -> Result<String, String> {
        if args.trip.trim().is_empty() {
            return finish(Err(err("trip is empty")));
        }
        finish(
            self.catalog
                .trip_schedule(
                    args.feed_id,
                    &args.trip,
                    args.at_unix,
                    limit_of(args.limit, 20, 100),
                    offset_of(args.offset),
                )
                .await,
        )
    }

    /// Builds a departure board for a station.
    ///
    /// Combines today's and yesterday's static schedule with realtime delays,
    /// cancellations, and assigned tracks. A platform stop expands to the parent
    /// station and its sibling platforms. Rows are sorted by estimated departure.
    ///
    /// # Arguments
    ///
    /// * `args` - Stop, optional route, clock, windows, and page. See [`BoardArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON page of departures. `next_offset` is null on the last page.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when `stop` is empty,
    /// the stop does not resolve, or the feed id does not name a loaded feed.
    #[tool(
        description = "Departure board for a station. Combines today's and yesterday's static schedule with realtime delays, cancellations, and assigned tracks. Platform stops expand to the parent station and sibling platforms. Sorted by estimated departure."
    )]
    async fn departure_board(
        &self,
        Parameters(args): Parameters<BoardArgs>,
    ) -> Result<String, String> {
        if args.stop.trim().is_empty() {
            return finish(Err(err("stop is empty")));
        }
        finish(
            self.catalog
                .departure_board(
                    args.feed_id,
                    &args.stop,
                    args.route,
                    args.at_unix,
                    minutes_of(args.window_minutes, 120, 1440),
                    minutes_of(args.past_minutes, 5, 180),
                    limit_of(args.limit, 20, 100),
                    offset_of(args.offset),
                    args.include_skipped.unwrap_or(false),
                )
                .await,
        )
    }

    /// Lists live vehicle positions.
    ///
    /// Filter by route, trip, vehicle id or label, or the stop the vehicle currently
    /// reports. Coordinates are rounded to five decimal places.
    ///
    /// # Arguments
    ///
    /// * `args` - Optional filters and page window. See [`VehicleArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON page of vehicles.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when the feed id does
    /// not name a loaded feed or the catalog cannot answer the query.
    #[tool(
        description = "Live vehicle positions. Filter by route, trip, vehicle id/label, or current stop. Coordinates are rounded to 5 decimals."
    )]
    async fn vehicle_positions(
        &self,
        Parameters(args): Parameters<VehicleArgs>,
    ) -> Result<String, String> {
        let feed_id = args.feed_id;
        finish(
            self.catalog
                .vehicles(
                    feed_id,
                    VehicleQuery {
                        route: args.route,
                        trip: args.trip,
                        vehicle: args.vehicle,
                        stop: args.stop,
                        limit: limit_of(args.limit, 20, 100),
                        offset: offset_of(args.offset),
                    },
                )
                .await,
        )
    }

    /// Lists trip updates with delay seconds and platform assignments.
    ///
    /// Each stop update can carry `assigned_stop_id` and `platform_code`. A stop
    /// filter matches `stop_id` or the static stop at that `stop_sequence`, so an
    /// update that omits `stop_id` still matches.
    ///
    /// # Arguments
    ///
    /// * `args` - Optional route, trip, and stop filters, plus page and per-trip stop
    ///   limits. See [`TripUpdateArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON page of trips and their stop-time updates.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when the feed id does
    /// not name a loaded feed or the catalog cannot answer the query.
    #[tool(
        description = "Trip updates with delay seconds and assigned_stop_id / platform_code. A stop filter matches stop_id or the static stop at that stop_sequence, so updates that omit stop_id still match."
    )]
    async fn trip_updates(
        &self,
        Parameters(args): Parameters<TripUpdateArgs>,
    ) -> Result<String, String> {
        let feed_id = args.feed_id;
        finish(
            self.catalog
                .trip_updates(
                    feed_id,
                    TripUpdateQuery {
                        route: args.route,
                        trip: args.trip,
                        stop: args.stop,
                        limit: limit_of(args.limit, 20, 100),
                        offset: offset_of(args.offset),
                        stop_limit: limit_of(args.stop_limit, 8, 40),
                    },
                )
                .await,
        )
    }

    /// Lists service alerts with cause, effect, and clipped text.
    ///
    /// Each alert includes severity, the active period, informed entities, and header
    /// and description text clipped to `max_chars`. `active_only` keeps alerts in
    /// effect at `at_unix`. Route and stop selectors are OR within one alert and AND
    /// across the two filters.
    ///
    /// # Arguments
    ///
    /// * `args` - Route, stop, text, activity, clock, and page. See [`AlertArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON page of alerts.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when the feed id does
    /// not name a loaded feed or the catalog cannot answer the query.
    #[tool(
        description = "Service alerts with cause, effect, severity, active period, informed entities, and clipped header/description text. active_only keeps alerts in effect at at_unix. Route and stop selectors are OR within an alert and AND across the two filters."
    )]
    async fn alerts(&self, Parameters(args): Parameters<AlertArgs>) -> Result<String, String> {
        let feed_id = args.feed_id;
        let at_unix = args.at_unix.unwrap_or_else(now_unix);
        finish(
            self.catalog
                .alerts(
                    feed_id,
                    AlertQuery {
                        route: args.route,
                        stop: args.stop,
                        text: args.text,
                        active_only: args.active_only.unwrap_or(true),
                        at_unix,
                        limit: limit_of(args.limit, 20, 100),
                        offset: offset_of(args.offset),
                        max_chars: limit_of(args.max_chars, 400, 2000),
                    },
                )
                .await,
        )
    }

    /// Decodes one GTFS-Realtime `FeedMessage` into compact JSON.
    ///
    /// The bytes come from a filesystem path, an `http` or `https` URL, or standard
    /// base64. This does not register a feed. Use it when a protobuf looks wrong.
    ///
    /// # Arguments
    ///
    /// * `args` - Exactly one of path, URL, or base64, plus page limits and optional
    ///   headers. See [`DecodeArgs`].
    ///
    /// # Returns
    ///
    /// Compact JSON for one page of entities.
    ///
    /// # Errors
    ///
    /// Returns `Err` with compact JSON of a [`crate::AppError`] when the caller does
    /// not pass exactly one source, the bytes cannot be read or decoded, or a header
    /// is rejected. Header values are not copied into the error.
    #[tool(
        description = "Decode one GTFS-Realtime FeedMessage from a path, http(s) URL, or base64 string into compact JSON. Does not register a feed. Use this when a protobuf looks wrong."
    )]
    async fn decode_protobuf(
        &self,
        Parameters(args): Parameters<DecodeArgs>,
    ) -> Result<String, String> {
        finish(
            self.catalog
                .decode(
                    args.path,
                    args.url,
                    args.base64,
                    offset_of(args.offset),
                    limit_of(args.limit, 20, 100),
                    limit_of(args.max_chars, 800, 4000),
                    args.headers.unwrap_or_default(),
                )
                .await,
        )
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "gtfs-mcp",
    instructions = "Explore GTFS static schedules and GTFS-Realtime for any agency. The startup feed comes from GTFS_MCP_STATIC (required) and the optional GTFS_MCP_TRIP_UPDATES, GTFS_MCP_VEHICLE_POSITIONS, and GTFS_MCP_ALERTS variables. Call list_feeds before register_feed. register_feed adds another static GTFS zip plus optional realtime protobuf URLs. A row with ready:false failed to load; refresh_feed retries it. Answer with trip_status (trip id or trip_short_name / train number), departure_board, trip_schedule, vehicle_positions, trip_updates, and alerts. Delays are seconds and positive means late. Track or platform assignments are assigned_stop_id and platform_code. Pass at_unix to pin the clock. Responses are compact JSON. Page with offset until next_offset is null. feed_health reports freshness. decode_protobuf turns a GTFS-Realtime message into JSON without registering a feed. HTTP header values are kept for fetches and are never echoed back."
)]
impl ServerHandler for GtfsServer {}

/// Serves `server` as MCP on standard input and output until the client closes the stream.
///
/// # Arguments
///
/// * `server` - Tool server consumed by the stdio transport.
///
/// # Returns
///
/// `Ok(())` when the session ends.
///
/// # Errors
///
/// Returns [`crate::AppError`] when the stdio transport fails to start or the
/// session ends with an error. The message is prefixed with `stdio:`.
pub async fn serve_stdio(server: GtfsServer) -> Result<(), AppError> {
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| err(format!("stdio: {e}")))?;
    running
        .waiting()
        .await
        .map_err(|e| err(format!("stdio: {e}")))?;
    Ok(())
}

/// Binds `bind` and serves streamable HTTP MCP until Ctrl-C.
///
/// MCP is mounted at `/mcp`. `GET /healthz` returns `ok`. The bound address is
/// written to the tracing log.
///
/// # Arguments
///
/// * `catalog` - Feeds shared by every HTTP session. Each session builds its own
///   [`GtfsServer`].
/// * `bind` - Local socket address to listen on.
/// * `allowed_hosts` - Hostnames the streamable HTTP transport accepts. An empty
///   list does not set an allowlist.
///
/// # Returns
///
/// `Ok(())` after the Ctrl-C shutdown finishes.
///
/// # Errors
///
/// Returns [`crate::AppError`] when the socket cannot be bound, the bound address
/// cannot be read, or the HTTP server exits with an error.
pub async fn serve_http(
    catalog: Arc<Catalog>,
    bind: SocketAddr,
    allowed_hosts: Vec<String>,
) -> Result<(), AppError> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| err(format!("bind {bind}: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| err(format!("local address: {e}")))?;
    tracing::info!("gtfs-mcp listening on http://{addr}/mcp");
    let router = http_router(catalog, allowed_hosts);
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| err(format!("http: {e}")))?;
    Ok(())
}

/// Binds an HTTP server and runs it on a background task.
///
/// Used by tests and embedders. MCP is mounted at `/mcp` and `GET /healthz`
/// returns `ok`. The task ignores an error from the server. Abort the returned
/// task to stop the listener.
///
/// # Arguments
///
/// * `catalog` - Feeds shared by every HTTP session. Each session builds its own
///   [`GtfsServer`].
/// * `bind` - Local socket address to listen on. Port `0` selects a free port.
/// * `allowed_hosts` - Hostnames the streamable HTTP transport accepts. An empty
///   list does not set an allowlist.
///
/// # Returns
///
/// The address that was bound, and the task serving it.
///
/// # Errors
///
/// Returns [`crate::AppError`] when the socket cannot be bound or the bound
/// address cannot be read.
pub async fn spawn_http(
    catalog: Arc<Catalog>,
    bind: SocketAddr,
    allowed_hosts: Vec<String>,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), AppError> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| err(format!("bind {bind}: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| err(format!("local address: {e}")))?;
    let router = http_router(catalog, allowed_hosts);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok((addr, handle))
}

/// Builds the Axum router for `/healthz` and streamable HTTP MCP at `/mcp`.
///
/// # Arguments
///
/// * `catalog` - Feeds cloned into each new [`GtfsServer`].
/// * `allowed_hosts` - Host allowlist. Applied only when the list is not empty.
///
/// # Returns
///
/// A router whose `/healthz` route returns `ok` and whose `/mcp` route speaks
/// streamable HTTP MCP.
fn http_router(catalog: Arc<Catalog>, allowed_hosts: Vec<String>) -> axum::Router {
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    if !allowed_hosts.is_empty() {
        config = config.with_allowed_hosts(allowed_hosts);
    }
    let service = StreamableHttpService::new(
        {
            let catalog = Arc::clone(&catalog);
            move || Ok(GtfsServer::new(Arc::clone(&catalog)))
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );
    axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .nest_service("/mcp", service)
}

/// Turns a catalog result into the compact JSON string a tool returns.
///
/// # Arguments
///
/// * `result` - JSON value, or a [`crate::AppError`] from a catalog call.
///
/// # Returns
///
/// Compact JSON text of the value.
///
/// # Errors
///
/// Returns `Err` with the compact JSON of the error object when `result` is `Err`.
fn finish(result: Result<Value, AppError>) -> Result<String, String> {
    match result {
        Ok(value) => Ok(compact(&value)),
        Err(error) => Err(compact(&error.to_json())),
    }
}

/// Serializes `value` as compact JSON.
///
/// # Arguments
///
/// * `value` - JSON value to write without extra whitespace.
///
/// # Returns
///
/// The JSON text, or `{"error":"failed to serialize"}` when serialization fails.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{\"error\":\"failed to serialize\"}".into())
}

/// Clamps an optional page size into `1..=max`.
///
/// # Arguments
///
/// * `value` - Requested size. `None` uses `default`.
/// * `default` - Size used when `value` is omitted.
/// * `max` - Inclusive upper bound.
///
/// # Returns
///
/// The clamped size as `usize`. Values below 1 become 1.
///
/// # Panics
///
/// Panics when `max` is below 1. [`u32::clamp`] requires the lower bound to be
/// at most the upper bound.
fn limit_of(value: Option<u32>, default: u32, max: u32) -> usize {
    value.unwrap_or(default).clamp(1, max) as usize
}

/// Caps an optional duration in minutes at `max`.
///
/// # Arguments
///
/// * `value` - Requested minutes. `None` uses `default`. Zero is preserved.
/// * `default` - Minutes used when `value` is omitted.
/// * `max` - Inclusive upper bound. Larger values become `max`.
///
/// # Returns
///
/// The capped duration as `u64`. Zero stays zero.
fn minutes_of(value: Option<u32>, default: u32, max: u32) -> u64 {
    u64::from(value.unwrap_or(default).min(max))
}

/// Converts an optional row offset to `usize`.
///
/// # Arguments
///
/// * `value` - Rows to skip. `None` starts at the first row.
///
/// # Returns
///
/// The offset, or zero when `value` is omitted.
fn offset_of(value: Option<u32>) -> usize {
    value.unwrap_or(0) as usize
}

/// Returns the current Unix time in seconds.
///
/// # Returns
///
/// Seconds since the Unix epoch, or zero when the clock is before the epoch.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
