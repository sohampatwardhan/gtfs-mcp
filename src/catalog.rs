//! Feed registry: register feeds, refresh them on a TTL, report health, and run queries.
//!
//! [`Catalog`] stores each loaded feed. [`RegisterRequest`] is the input. Headers on
//! the request are stored with the feed and sent on every HTTP fetch of that feed.
//! [`Catalog::note_load_failure`] records a startup load that failed and replaces
//! header values of length 4 or more in the error text with `[redacted]`.
//!
//! Realtime protobufs are downloaded again when they are older than the realtime TTL.
//! The static zip is checked again when the static TTL elapses. Health and the query
//! methods refresh on that schedule before they read the cache. [`Catalog::list`]
//! reads the cache as it stands.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};

use crate::error::{err, AppError};
#[cfg(test)]
use crate::fixtures;
use crate::gtfs::StaticDb;
use crate::gtfs_rt::FeedMessage;
use crate::query::{
    self, AlertQuery, BoardQuery, RtView, ScheduleQuery, TripStatusQuery, TripUpdateQuery,
    VehicleQuery,
};
use crate::realtime::{self, incrementality};
use crate::source::{
    self, fetch_bytes, loc_label, validate_headers, FeedSpec, Loc, MAX_RT_BYTES, MAX_STATIC_BYTES,
};

/// Maximum number of feeds stored at once, including feeds that failed to load.
const MAX_FEEDS: usize = 16;

/// Locations, TTLs, and HTTP headers for one feed.
///
/// Set the static GTFS zip with [`RegisterRequest::static_url`] or
/// [`RegisterRequest::static_path`]. Each realtime product is optional and uses the
/// same pair of fields. Whitespace-only strings are ignored. [`Catalog::register`]
/// downloads the zip before the feed is stored.
///
/// # Examples
///
/// ```
/// use gtfs_mcp::RegisterRequest;
///
/// let request = RegisterRequest {
///     feed_id: Some("harbor".into()),
///     static_url: Some("https://example.com/gtfs.zip".into()),
///     ..Default::default()
/// };
/// assert_eq!(request.feed_id.as_deref(), Some("harbor"));
/// ```
#[derive(Debug, Clone, Default)]
pub struct RegisterRequest {
    /// Id for this feed.
    ///
    /// Empty or missing becomes `default` when the feed is registered. A non-empty id
    /// is 1-64 characters: ASCII letters, digits, `_`, `.`, `:`, or `-`.
    pub feed_id: Option<String>,
    /// URL of the static GTFS zip.
    ///
    /// `http://` and `https://` are fetched over HTTP. `file://` is percent-decoded
    /// into a filesystem path, and a value with no scheme is a path too. Any other
    /// scheme is rejected. Leave this empty when [`RegisterRequest::static_path`] is set.
    pub static_url: Option<String>,
    /// Filesystem path of the static GTFS zip.
    ///
    /// Leave this empty when [`RegisterRequest::static_url`] is set.
    pub static_path: Option<String>,
    /// URL of the TripUpdates protobuf.
    ///
    /// Optional. Uses the URL rules of [`RegisterRequest::static_url`]. Leave this
    /// empty when [`RegisterRequest::trip_updates_path`] is set.
    pub trip_updates_url: Option<String>,
    /// Filesystem path of the TripUpdates protobuf.
    ///
    /// Optional. Leave this empty when [`RegisterRequest::trip_updates_url`] is set.
    pub trip_updates_path: Option<String>,
    /// URL of the VehiclePositions protobuf.
    ///
    /// Optional. Uses the URL rules of [`RegisterRequest::static_url`]. Leave this
    /// empty when [`RegisterRequest::vehicle_positions_path`] is set.
    pub vehicle_positions_url: Option<String>,
    /// Filesystem path of the VehiclePositions protobuf.
    ///
    /// Optional. Leave this empty when [`RegisterRequest::vehicle_positions_url`] is set.
    pub vehicle_positions_path: Option<String>,
    /// URL of the Alerts protobuf.
    ///
    /// Optional. Uses the URL rules of [`RegisterRequest::static_url`]. Leave this
    /// empty when [`RegisterRequest::alerts_path`] is set.
    pub alerts_url: Option<String>,
    /// Filesystem path of the Alerts protobuf.
    ///
    /// Optional. Leave this empty when [`RegisterRequest::alerts_url`] is set.
    pub alerts_path: Option<String>,
    /// Realtime refresh interval in seconds.
    ///
    /// [`Catalog::register`] treats a missing value as 15 and clamps any other value
    /// to 1-3600. Startup validation rejects a value outside 1-3600.
    pub rt_ttl_secs: Option<u64>,
    /// How often the static zip is checked again, in seconds.
    ///
    /// [`Catalog::register`] treats a missing value as 6 hours and caps any other
    /// value at 7 days. `0` disables that timer. Startup validation rejects a value
    /// above 7 days.
    pub static_ttl_secs: Option<u64>,
    /// HTTP headers sent on every fetch of this feed.
    ///
    /// A name is non-empty and contains neither CR, LF, nor `:`. A value contains
    /// neither CR nor LF. Health output lists the names. [`Catalog::note_load_failure`]
    /// replaces values of length 4 or more in the stored error text with `[redacted]`.
    pub headers: BTreeMap<String, String>,
}

/// One downloaded GTFS-Realtime message kept in memory.
struct CachedFeed {
    /// Decoded [`FeedMessage`].
    message: FeedMessage,
    /// Size of the downloaded payload, in bytes.
    bytes: usize,
    /// URL or filesystem path the bytes were read from.
    source: String,
}

/// Loaded state of one feed: static database, realtime cache, and TTLs.
struct FeedData {
    /// Feed id, after [`normalize_id`].
    id: String,
    /// Static and realtime locations to fetch.
    spec: FeedSpec,
    /// Headers applied to every fetch of this feed.
    headers: BTreeMap<String, String>,
    /// Age at which realtime protobufs are downloaded again.
    rt_ttl: Duration,
    /// Age at which the static zip is checked again. Zero disables that timer.
    static_ttl: Duration,
    /// Static GTFS database loaded from the zip.
    db: StaticDb,
    /// Instant of the last static download. Health reports age from this value.
    static_fetched: Instant,
    /// Unix seconds of [`FeedData::static_fetched`].
    static_fetched_unix: u64,
    /// When the static zip was last downloaded.
    ///
    /// The static TTL runs from here.
    static_checked: Instant,
    /// Cached TripUpdates message. `None` when that product is unset or not yet loaded.
    trip_updates: Option<CachedFeed>,
    /// Cached VehiclePositions message. `None` when that product is unset or not yet loaded.
    vehicles: Option<CachedFeed>,
    /// Cached Alerts message. `None` when that product is unset or not yet loaded.
    alerts: Option<CachedFeed>,
    /// Instant of the last realtime download. `None` when realtime has not been fetched.
    rt_fetched: Option<Instant>,
    /// Unix seconds of the last realtime download.
    rt_fetched_unix: u64,
    /// Text of the last refresh error kept while the previous static database stays loaded.
    ///
    /// Cleared after a successful load or refresh.
    last_error: Option<String>,
}

/// One registered feed, with a lock that serializes its refresh.
struct Slot {
    /// Held for the whole refresh so overlapping callers wait instead of downloading twice.
    refresh: tokio::sync::Mutex<()>,
    /// Feed state. [`lock`] recovers the value when this mutex is poisoned.
    data: Mutex<FeedData>,
}

/// A [`RegisterRequest`] that failed to load and is still visible in the registry.
struct FailedFeed {
    /// Input to try again on [`Catalog::refresh`].
    request: RegisterRequest,
    /// Error text shown for this feed. Header values of length 4 or more are `[redacted]`.
    error: String,
    /// Hint copied from the [`crate::AppError`], with header values left as stored there.
    hint: Option<String>,
}

/// In-memory registry of GTFS feeds.
///
/// Loaded feeds and feeds that failed at startup are both visible from
/// [`Catalog::list`]. [`Catalog::register`] or [`Catalog::refresh`] replaces a
/// failed feed. Startup config errors are copied onto list and health JSON.
pub struct Catalog {
    /// Client used for every HTTP fetch. Timeouts and the redirect limit are fixed at construction.
    http: reqwest::Client,
    /// Loaded feeds keyed by id.
    feeds: Mutex<HashMap<String, std::sync::Arc<Slot>>>,
    /// Feeds that failed to load, keyed by id.
    failed: Mutex<HashMap<String, FailedFeed>>,
    /// Startup problems attached to successful list and health objects and to some errors.
    config_errors: Mutex<Vec<Value>>,
}

impl Catalog {
    /// Builds an empty catalog and its HTTP client.
    ///
    /// # Returns
    ///
    /// A catalog with no feeds and no config errors.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be built.
    ///
    /// # Examples
    ///
    /// ```
    /// use gtfs_mcp::Catalog;
    ///
    /// let catalog = Catalog::new().expect("client");
    /// assert_eq!(catalog.list()["feeds"].as_array().map(Vec::len), Some(0));
    /// ```
    pub fn new() -> Result<Self, AppError> {
        Ok(Self {
            http: source::http_client()?,
            feeds: Mutex::new(HashMap::new()),
            failed: Mutex::new(HashMap::new()),
            config_errors: Mutex::new(Vec::new()),
        })
    }

    /// Appends startup config errors that later responses can show.
    ///
    /// # Arguments
    ///
    /// * `errors` - JSON values to store. An empty vector leaves the catalog unchanged.
    pub fn add_config_errors(&self, errors: Vec<Value>) {
        if errors.is_empty() {
            return;
        }
        lock(&self.config_errors).extend(errors);
    }

    /// Records a feed that failed to load.
    ///
    /// Each header value of length 4 or more is replaced with `[redacted]` everywhere
    /// it occurs in the error text. Shorter values stay as written. The hint is stored
    /// unchanged.
    ///
    /// A missing or empty `feed_id` appends a JSON object with `error` and `hint` to
    /// the config-error list. An id that already names a loaded feed leaves that feed
    /// unchanged.
    ///
    /// # Arguments
    ///
    /// * `request` - Registration input that failed. When the id is kept, the stored
    ///   copy uses that id.
    /// * `error` - Failure to show from [`Catalog::list`] and [`Catalog::health`].
    pub fn note_load_failure(&self, mut request: RegisterRequest, error: &AppError) {
        let Some(id) = request.feed_id.clone().filter(|id| !id.is_empty()) else {
            self.add_config_errors(vec![json!({
                "error": error.error,
                "hint": error.hint,
            })]);
            return;
        };
        if lock(&self.feeds).contains_key(&id) {
            return;
        }
        let mut message = error.error.clone();
        for value in request.headers.values() {
            if value.len() >= 4 {
                message = message.replace(value, "[redacted]");
            }
        }
        request.feed_id = Some(id.clone());
        lock(&self.failed).insert(
            id,
            FailedFeed {
                request,
                error: message,
                hint: error.hint.clone(),
            },
        );
    }

    /// Downloads `request` and stores the feed under its id.
    ///
    /// An id that is already loaded, or already listed as failed, is replaced. The
    /// feed cap is checked after the download. A new id that would exceed the cap
    /// returns an error and stores nothing.
    ///
    /// # Arguments
    ///
    /// * `request` - Static location, optional realtime locations, TTLs, and headers.
    ///
    /// # Returns
    ///
    /// Health JSON for the loaded feed: `feed_id`, `ready` true, `source`, header
    /// names, static counts, and realtime ages. `config_errors` is present when any
    /// have been recorded.
    ///
    /// # Errors
    ///
    /// * A header name is empty or contains CR, LF, or `:`, or a header value contains
    ///   CR or LF.
    /// * `feed_id` is non-empty and is longer than 64 characters or contains a
    ///   character other than ASCII alphanumeric, `_`, `.`, `:`, or `-`.
    /// * The static URL and path are both missing or blank.
    /// * Static GTFS, trip updates, vehicle positions, or alerts sets both a URL and a path.
    /// * A URL uses a scheme other than `http`, `https`, or `file`.
    /// * The static zip or a realtime file cannot be read.
    /// * An HTTP request fails, or the status is not successful.
    /// * A file or HTTP body exceeds its byte cap, or a response declares a length over that cap.
    /// * Gzip decompression of a realtime body fails, the body is empty, or the bytes
    ///   are not a GTFS-Realtime FeedMessage.
    /// * The static bytes are not a GTFS zip, a CSV or SQLite step fails, or `stops.txt` is missing.
    /// * The blocking task that parses the zip or decodes a protobuf fails to join.
    /// * The id is new and the registry already holds 16 feeds, counting feeds that
    ///   failed to load.
    pub async fn register(&self, request: RegisterRequest) -> Result<Value, AppError> {
        validate_headers(&request.headers)?;
        let id = normalize_id(request.feed_id.as_deref())?;
        let spec = spec_from_request(&request)?;
        let rt_ttl = Duration::from_secs(request.rt_ttl_secs.unwrap_or(15).clamp(1, 3600));
        let static_ttl = Duration::from_secs(
            request
                .static_ttl_secs
                .unwrap_or(6 * 3600)
                .min(7 * 24 * 3600),
        );
        let data = load_feed(
            &self.http,
            id.clone(),
            spec,
            request.headers.clone(),
            rt_ttl,
            static_ttl,
        )
        .await?;
        let summary = health_json(&data, now_unix());
        {
            let replacing_failure = lock(&self.failed).contains_key(&id);
            let failed_count = lock(&self.failed).len();
            let mut feeds = lock(&self.feeds);
            let occupied = feeds.len() + failed_count - usize::from(replacing_failure);
            if !feeds.contains_key(&id) && occupied >= MAX_FEEDS {
                return Err(err(format!("at most {MAX_FEEDS} feeds can be registered")));
            }
            feeds.insert(
                id.clone(),
                std::sync::Arc::new(Slot {
                    refresh: tokio::sync::Mutex::new(()),
                    data: Mutex::new(data),
                }),
            );
        }
        lock(&self.failed).remove(&id);
        Ok(self.decorate(summary))
    }

    /// Returns cached health for every loaded feed and every feed that failed to load.
    ///
    /// # Returns
    ///
    /// A JSON object with `feeds`, sorted by `feed_id`. A loaded row has `ready` true.
    /// A failed row has `ready` false, plus `error` and `hint`. `config_errors` is
    /// present when any have been recorded.
    pub fn list(&self) -> Value {
        let feeds = lock(&self.feeds);
        let mut rows: Vec<Value> = feeds
            .values()
            .map(|slot| health_json(&lock(&slot.data), now_unix()))
            .collect();
        drop(feeds);
        for (id, failure) in lock(&self.failed).iter() {
            if rows
                .iter()
                .any(|row| row.get("feed_id").and_then(|v| v.as_str()) == Some(id))
            {
                continue;
            }
            rows.push(failure_json(failure));
        }
        rows.sort_by(|a, b| {
            a.get("feed_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("feed_id").and_then(|v| v.as_str()).unwrap_or(""))
        });
        self.decorate(json!({ "feeds": rows }))
    }

    /// Removes one loaded feed or one failed feed.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Id to remove. Omit it when exactly one feed is registered,
    ///   including feeds that failed to load.
    ///
    /// # Returns
    ///
    /// A JSON object `removed` set to the id.
    ///
    /// # Errors
    ///
    /// * No feed is registered. The message is `no feed registered`, or
    ///   `feed configuration is invalid` when startup config errors were recorded.
    /// * `feed_id` is omitted or blank while more than one feed is registered.
    /// * `feed_id` matches neither a loaded feed nor a failed feed. Known ids are
    ///   included. Recorded config errors are attached when any exist.
    /// * The id was removed before this call finished (`no feed named '<id>'`).
    pub fn unregister(&self, feed_id: Option<String>) -> Result<Value, AppError> {
        let id = self.resolve_id(feed_id.as_deref())?;
        let removed_live = lock(&self.feeds).remove(&id).is_some();
        let removed_failed = lock(&self.failed).remove(&id).is_some();
        if !removed_live && !removed_failed {
            return Err(err(format!("no feed named '{id}'")));
        }
        Ok(json!({ "removed": id }))
    }

    /// Reloads one feed and returns its health.
    ///
    /// A feed that failed to load is registered again from the stored request. A loaded
    /// feed downloads the static zip and the realtime protobufs again. When that
    /// download fails and the static database still has stops, the previous database
    /// stays and the error text is stored on the feed.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Id to reload. Omit it when exactly one feed is registered.
    ///
    /// # Returns
    ///
    /// Health JSON for the feed after a successful reload, or cached health with
    /// `realtime.last_error` set when a loaded feed's reload failed and its stops
    /// were kept. `config_errors` is present when any have been recorded.
    ///
    /// # Errors
    ///
    /// * No feed is registered, several feeds are registered and `feed_id` was omitted,
    ///   or `feed_id` is unknown. Recorded config errors are attached when any exist.
    /// * The failed feed is registered again and that registration fails. The failure
    ///   is stored again, with header values of length 4 or more redacted. Registration
    ///   errors are the same as [`Catalog::register`].
    /// * The loaded feed's reload fails and the static database has no stops.
    /// * The loaded feed is gone before health is read.
    pub async fn refresh(&self, feed_id: Option<String>) -> Result<Value, AppError> {
        let id = self.resolve_id(feed_id.as_deref())?;
        let failure = lock(&self.failed).remove(&id);
        if let Some(failure) = failure {
            return match self.register(failure.request.clone()).await {
                Ok(value) => Ok(value),
                Err(error) => {
                    self.note_load_failure(failure.request, &error);
                    Err(error)
                }
            };
        }
        self.ensure(&id, true).await?;
        self.health_cached(&id)
    }

    /// Returns health for one feed, refreshing it when a TTL has elapsed.
    ///
    /// A feed that failed to load returns its stored failure object.
    /// A loaded feed whose refresh fails while stops are already loaded returns health
    /// with `realtime.last_error` set.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Id to describe. Omit it when exactly one feed is registered.
    ///
    /// # Returns
    ///
    /// Health JSON (`ready` true) for a loaded feed, or failure JSON (`ready` false)
    /// for a feed that failed to load. `config_errors` is included when any have been
    /// recorded. When `feed_id` is omitted or blank and startup config is invalid, the
    /// returned object is that error JSON (`error`, `hint`, and `config_errors`).
    ///
    /// # Errors
    ///
    /// * No feed is registered and no config errors were recorded.
    /// * Several feeds are registered and `feed_id` was omitted or blank.
    /// * `feed_id` is unknown. Known ids are included. Recorded config errors are
    ///   attached when any exist.
    /// * A TTL refresh fails and the static database has no stops.
    /// * The loaded feed is gone before health is read.
    pub async fn health(&self, feed_id: Option<String>) -> Result<Value, AppError> {
        let id = match self.resolve_id(feed_id.as_deref()) {
            Ok(id) => id,
            Err(error)
                if feed_id.as_deref().unwrap_or("").trim().is_empty()
                    && error.error == "feed configuration is invalid" =>
            {
                return Ok(error.to_json());
            }
            Err(error) => return Err(error),
        };
        if let Some(value) = self.failure_document(&id) {
            return Ok(value);
        }
        self.ensure(&id, false).await?;
        self.health_cached(&id)
    }

    /// Searches stops in one feed.
    ///
    /// Refreshes the feed first when a TTL has elapsed.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Feed to search. Omit it when exactly one feed is loaded.
    /// * `query` - `stop_id`, stop code, or part of a stop name. Empty lists every stop.
    /// * `limit` - Maximum rows in this page.
    /// * `offset` - Number of matching rows to skip.
    ///
    /// # Returns
    ///
    /// A page object with `feed_id`, `total`, `offset`, `returned`, `next_offset`,
    /// and `stops`. Each stop has `stop_id`, `stop_code`, `stop_name`, `lat`, `lon`,
    /// `location_type`, `parent_station`, and `platform_code`.
    ///
    /// # Errors
    ///
    /// * No feed is registered. The message is `no feed registered`, or
    ///   `feed configuration is invalid` when startup config errors were recorded.
    /// * `feed_id` is omitted or blank while more than one feed is registered.
    /// * `feed_id` matches neither a loaded feed nor a failed feed.
    /// * The id names a feed that failed to load. Header values of length 4 or more
    ///   in that error were replaced with `[redacted]`.
    /// * A TTL refresh fails and the static database has no stops.
    /// * The loaded feed is gone before the query runs.
    /// * The static database query fails.
    pub async fn search_stops(
        &self,
        feed_id: Option<String>,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Value, AppError> {
        let id = self.require_loaded(feed_id.as_deref())?;
        self.ensure(&id, false).await?;
        let slot = self.slot(&id)?;
        let data = lock(&slot.data);
        let (rows, total) = data.db.search_stops(query, limit, offset)?;
        let stops: Vec<Value> = rows
            .iter()
            .map(|stop| {
                realtime::obj([
                    ("stop_id", json!(stop.stop_id)),
                    ("stop_code", realtime::json_opt(stop.stop_code.clone())),
                    ("stop_name", realtime::json_opt(stop.stop_name.clone())),
                    ("lat", realtime::json_f64(stop.lat)),
                    ("lon", realtime::json_f64(stop.lon)),
                    ("location_type", json!(stop.location_type)),
                    (
                        "parent_station",
                        realtime::json_opt(stop.parent_station.clone()),
                    ),
                    (
                        "platform_code",
                        realtime::json_opt(stop.platform_code.clone()),
                    ),
                ])
            })
            .collect();
        Ok(page_list(&id, "stops", stops, total as usize, offset))
    }

    /// Searches routes in one feed.
    ///
    /// Refreshes the feed first when a TTL has elapsed.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Feed to search. Omit it when exactly one feed is loaded.
    /// * `query` - `route_id`, short name, or part of a route name. Empty lists every route.
    /// * `limit` - Maximum rows in this page.
    /// * `offset` - Number of matching rows to skip.
    ///
    /// # Returns
    ///
    /// A page object with `feed_id`, `total`, `offset`, `returned`, `next_offset`,
    /// and `routes`. Each route has `route_id`, `route_short_name`, `route_long_name`,
    /// `route_type`, `route_type_name`, and `route_color`.
    ///
    /// # Errors
    ///
    /// * No feed is registered. The message is `no feed registered`, or
    ///   `feed configuration is invalid` when startup config errors were recorded.
    /// * `feed_id` is omitted or blank while more than one feed is registered.
    /// * `feed_id` matches neither a loaded feed nor a failed feed.
    /// * The id names a feed that failed to load. Header values of length 4 or more
    ///   in that error were replaced with `[redacted]`.
    /// * A TTL refresh fails and the static database has no stops.
    /// * The loaded feed is gone before the query runs.
    /// * The static database query fails.
    pub async fn search_routes(
        &self,
        feed_id: Option<String>,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Value, AppError> {
        let id = self.require_loaded(feed_id.as_deref())?;
        self.ensure(&id, false).await?;
        let slot = self.slot(&id)?;
        let data = lock(&slot.data);
        let (rows, total) = data.db.search_routes(query, limit, offset)?;
        let routes: Vec<Value> = rows
            .iter()
            .map(|route| {
                realtime::obj([
                    ("route_id", json!(route.route_id)),
                    (
                        "route_short_name",
                        realtime::json_opt(route.short_name.clone()),
                    ),
                    (
                        "route_long_name",
                        realtime::json_opt(route.long_name.clone()),
                    ),
                    ("route_type", json!(route.route_type)),
                    (
                        "route_type_name",
                        json!(crate::gtfs::route_type_name(route.route_type)),
                    ),
                    ("route_color", realtime::json_opt(route.color.clone())),
                ])
            })
            .collect();
        Ok(page_list(&id, "routes", routes, total as usize, offset))
    }

    /// Returns live and scheduled status for a trip.
    ///
    /// Refreshes the feed first when a TTL has elapsed. The query object gains `feed_id`.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Feed to read. Omit it when exactly one feed is loaded.
    /// * `trip` - `trip_id` or `trip_short_name` (train number).
    /// * `stop` - Stop to describe in `at_stop`. `None` skips that object.
    /// * `at_unix` - Instant used to pick the service day. `None` means the current time.
    /// * `upcoming_limit` - Maximum number of upcoming stops included on the trip.
    ///
    /// # Returns
    ///
    /// A JSON object with `feed_id`, `matched`, `trips`, and `alternatives`.
    ///
    /// # Errors
    ///
    /// * No feed is registered. The message is `no feed registered`, or
    ///   `feed configuration is invalid` when startup config errors were recorded.
    /// * `feed_id` is omitted or blank while more than one feed is registered.
    /// * `feed_id` matches neither a loaded feed nor a failed feed.
    /// * The id names a feed that failed to load. Header values of length 4 or more
    ///   in that error were replaced with `[redacted]`.
    /// * A TTL refresh fails and the static database has no stops.
    /// * The loaded feed is gone before the query runs.
    /// * The static database query fails.
    /// * `trip` is empty or matches no trip.
    /// * `stop` is set and is empty, matches nothing, or matches several places.
    pub async fn trip_status(
        &self,
        feed_id: Option<String>,
        trip: &str,
        stop: Option<String>,
        at_unix: Option<i64>,
        upcoming_limit: usize,
    ) -> Result<Value, AppError> {
        self.query(feed_id, |db, rt| {
            query::trip_status(
                db,
                rt,
                &TripStatusQuery {
                    trip: trip.to_string(),
                    stop,
                    at_unix: at_unix.unwrap_or_else(now_unix),
                    upcoming_limit,
                },
            )
        })
        .await
    }

    /// Returns the stop times of one trip.
    ///
    /// Refreshes the feed first when a TTL has elapsed. The schedule object gains `feed_id`.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Feed to read. Omit it when exactly one feed is loaded.
    /// * `trip` - `trip_id` or `trip_short_name` (train number).
    /// * `at_unix` - Instant used to pick the service day. `None` means the current time.
    /// * `limit` - Maximum stop rows in this page.
    /// * `offset` - Number of stop rows to skip.
    ///
    /// # Returns
    ///
    /// A JSON object with `feed_id`, the trip's identity, `stop_count`, page fields
    /// (`offset`, `returned`, `next_offset`), and `stops`.
    ///
    /// # Errors
    ///
    /// * No feed is registered. The message is `no feed registered`, or
    ///   `feed configuration is invalid` when startup config errors were recorded.
    /// * `feed_id` is omitted or blank while more than one feed is registered.
    /// * `feed_id` matches neither a loaded feed nor a failed feed.
    /// * The id names a feed that failed to load. Header values of length 4 or more
    ///   in that error were replaced with `[redacted]`.
    /// * A TTL refresh fails and the static database has no stops.
    /// * The loaded feed is gone before the query runs.
    /// * The static database query fails.
    /// * `trip` is empty or matches no trip.
    /// * `trip` matches several trips and is not an exact `trip_id`.
    pub async fn trip_schedule(
        &self,
        feed_id: Option<String>,
        trip: &str,
        at_unix: Option<i64>,
        limit: usize,
        offset: usize,
    ) -> Result<Value, AppError> {
        self.query(feed_id, |db, rt| {
            query::trip_schedule(
                db,
                rt,
                &ScheduleQuery {
                    trip: trip.to_string(),
                    at_unix: at_unix.unwrap_or_else(now_unix),
                    limit,
                    offset,
                },
            )
        })
        .await
    }

    /// Returns a departure board for one stop.
    ///
    /// Refreshes the feed first when a TTL has elapsed. The board object gains `feed_id`.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Feed to read. Omit it when exactly one feed is loaded.
    /// * `stop` - `stop_id`, stop code, stop name, or platform id.
    /// * `route` - Optional `route_id` or short name. `None` keeps every route.
    /// * `at_unix` - Center of the time window. `None` means the current time.
    /// * `window_minutes` - How far ahead of `at_unix` to look, in minutes.
    /// * `past_minutes` - How far behind `at_unix` to look, in minutes.
    /// * `limit` - Maximum departures in this page.
    /// * `offset` - Number of matching departures to skip.
    /// * `include_skipped` - When true, skipped stops stay on the board.
    ///
    /// # Returns
    ///
    /// A JSON object with `feed_id`, the stop, the window, page fields, and `departures`.
    ///
    /// # Errors
    ///
    /// * No feed is registered. The message is `no feed registered`, or
    ///   `feed configuration is invalid` when startup config errors were recorded.
    /// * `feed_id` is omitted or blank while more than one feed is registered.
    /// * `feed_id` matches neither a loaded feed nor a failed feed.
    /// * The id names a feed that failed to load. Header values of length 4 or more
    ///   in that error were replaced with `[redacted]`.
    /// * A TTL refresh fails and the static database has no stops.
    /// * The loaded feed is gone before the query runs.
    /// * The static database query fails.
    /// * `stop` is empty, matches nothing, or matches several places.
    #[allow(clippy::too_many_arguments)]
    pub async fn departure_board(
        &self,
        feed_id: Option<String>,
        stop: &str,
        route: Option<String>,
        at_unix: Option<i64>,
        window_minutes: u64,
        past_minutes: u64,
        limit: usize,
        offset: usize,
        include_skipped: bool,
    ) -> Result<Value, AppError> {
        self.query(feed_id, |db, rt| {
            query::departure_board(
                db,
                rt,
                &BoardQuery {
                    stop: stop.to_string(),
                    route,
                    at_unix: at_unix.unwrap_or_else(now_unix),
                    window_minutes,
                    past_minutes,
                    limit,
                    offset,
                    include_skipped,
                },
            )
        })
        .await
    }

    /// Returns vehicle positions for one feed.
    ///
    /// Refreshes the feed first when a TTL has elapsed. The page gains `feed_id`.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Feed to read. Omit it when exactly one feed is loaded.
    /// * `query` - Route, trip, vehicle, and stop filters, plus page bounds.
    ///   See [`crate::VehicleQuery`].
    ///
    /// # Returns
    ///
    /// A page object with `feed_id`, `header_timestamp`, `feed_version`, `total`,
    /// `offset`, `returned`, `next_offset`, and `vehicles`.
    ///
    /// # Errors
    ///
    /// * No feed is registered. The message is `no feed registered`, or
    ///   `feed configuration is invalid` when startup config errors were recorded.
    /// * `feed_id` is omitted or blank while more than one feed is registered.
    /// * `feed_id` matches neither a loaded feed nor a failed feed.
    /// * The id names a feed that failed to load. Header values of length 4 or more
    ///   in that error were replaced with `[redacted]`.
    /// * A TTL refresh fails and the static database has no stops.
    /// * The loaded feed is gone before the query runs.
    /// * The feed has no VehiclePositions source (`this feed has no vehicle positions`).
    /// * The static database query fails.
    pub async fn vehicles(
        &self,
        feed_id: Option<String>,
        query: VehicleQuery,
    ) -> Result<Value, AppError> {
        self.query(feed_id, |db, rt| query::vehicle_positions(db, rt, &query))
            .await
    }

    /// Returns trip updates for one feed.
    ///
    /// Refreshes the feed first when a TTL has elapsed. The page gains `feed_id`.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Feed to read. Omit it when exactly one feed is loaded.
    /// * `query` - Route, trip, and stop filters, plus page bounds and the per-trip
    ///   stop cap. See [`crate::TripUpdateQuery`].
    ///
    /// # Returns
    ///
    /// A page object with `feed_id`, `header_timestamp`, `feed_version`, `total`,
    /// `offset`, `returned`, `next_offset`, and `trip_updates`.
    ///
    /// # Errors
    ///
    /// * No feed is registered. The message is `no feed registered`, or
    ///   `feed configuration is invalid` when startup config errors were recorded.
    /// * `feed_id` is omitted or blank while more than one feed is registered.
    /// * `feed_id` matches neither a loaded feed nor a failed feed.
    /// * The id names a feed that failed to load. Header values of length 4 or more
    ///   in that error were replaced with `[redacted]`.
    /// * A TTL refresh fails and the static database has no stops.
    /// * The loaded feed is gone before the query runs.
    /// * The feed has no TripUpdates source (`this feed has no trip updates`).
    /// * The static database query fails.
    pub async fn trip_updates(
        &self,
        feed_id: Option<String>,
        query: TripUpdateQuery,
    ) -> Result<Value, AppError> {
        self.query(feed_id, |db, rt| query::trip_updates(db, rt, &query))
            .await
    }

    /// Returns service alerts for one feed.
    ///
    /// Refreshes the feed first when a TTL has elapsed. The page gains `feed_id`.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Feed to read. Omit it when exactly one feed is loaded.
    /// * `query` - Route, stop, and text filters, the active-only flag, the instant,
    ///   page bounds, and the text cap. See [`crate::AlertQuery`].
    ///
    /// # Returns
    ///
    /// A page object with `feed_id`, `header_timestamp`, `feed_version`, `total`,
    /// `offset`, `returned`, `next_offset`, and `alerts`.
    ///
    /// # Errors
    ///
    /// * No feed is registered. The message is `no feed registered`, or
    ///   `feed configuration is invalid` when startup config errors were recorded.
    /// * `feed_id` is omitted or blank while more than one feed is registered.
    /// * `feed_id` matches neither a loaded feed nor a failed feed.
    /// * The id names a feed that failed to load. Header values of length 4 or more
    ///   in that error were replaced with `[redacted]`.
    /// * A TTL refresh fails and the static database has no stops.
    /// * The loaded feed is gone before the query runs.
    /// * The feed has no Alerts source (`this feed has no alerts`).
    /// * The static database query fails.
    pub async fn alerts(
        &self,
        feed_id: Option<String>,
        query: AlertQuery,
    ) -> Result<Value, AppError> {
        self.query(feed_id, |db, rt| query::alerts(db, rt, &query))
            .await
    }

    /// Decodes one GTFS-Realtime protobuf from a path, a URL, or base64.
    ///
    /// The bytes are not stored on a feed.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem path of a protobuf. Mutually exclusive with `url` and
    ///   `base64_body`.
    /// * `url` - `http://` or `https://` URL of a protobuf.
    /// * `base64_body` - Standard base64, with or without padding. Whitespace is ignored.
    /// * `offset` - Number of entities to skip.
    /// * `limit` - Maximum entities to decode into JSON.
    /// * `max_chars` - Maximum characters kept from each translated string.
    /// * `headers` - HTTP headers sent when `path` or `url` is read.
    ///
    /// # Returns
    ///
    /// A JSON object with `header`, `entity_count`, `offset`, `returned`,
    /// `next_offset`, `entities`, and `bytes`.
    ///
    /// # Errors
    ///
    /// * A header name is empty or contains CR, LF, or `:`, or a header value contains
    ///   CR or LF.
    /// * Zero or several of `path`, `url`, and `base64_body` are non-empty.
    /// * `url` is set and does not start with `http://` or `https://`.
    /// * The file cannot be read, the HTTP request fails, or the status is not successful.
    /// * The file or HTTP body exceeds the realtime byte cap, or the response declares
    ///   a length over that cap.
    /// * `base64_body` is not standard base64 after whitespace is removed.
    /// * The bytes are empty, gzip decompression fails, or they are not a GTFS-Realtime
    ///   FeedMessage.
    /// * The blocking decode task fails to join.
    #[allow(clippy::too_many_arguments)]
    pub async fn decode(
        &self,
        path: Option<String>,
        url: Option<String>,
        base64_body: Option<String>,
        offset: usize,
        limit: usize,
        max_chars: usize,
        headers: BTreeMap<String, String>,
    ) -> Result<Value, AppError> {
        validate_headers(&headers)?;
        let sources = [
            path.as_ref()
                .filter(|s| !s.trim().is_empty())
                .map(|_| "path"),
            url.as_ref().filter(|s| !s.trim().is_empty()).map(|_| "url"),
            base64_body
                .as_ref()
                .filter(|s| !s.trim().is_empty())
                .map(|_| "base64"),
        ]
        .into_iter()
        .flatten()
        .count();
        if sources != 1 {
            return Err(err("pass exactly one of path, url, or base64"));
        }
        let bytes = if let Some(path) = path.filter(|s| !s.trim().is_empty()) {
            fetch_bytes(
                &self.http,
                &Loc::File(PathBuf::from(path)),
                &headers,
                MAX_RT_BYTES,
            )
            .await?
        } else if let Some(url) = url.filter(|s| !s.trim().is_empty()) {
            let loc = if url.starts_with("http://") || url.starts_with("https://") {
                Loc::Http(url)
            } else {
                return Err(err("url must be http or https"));
            };
            fetch_bytes(&self.http, &loc, &headers, MAX_RT_BYTES).await?
        } else {
            decode_base64(base64_body.as_deref().unwrap_or(""))?
        };
        let bytes_len = bytes.len();
        let message = tokio::task::spawn_blocking(move || realtime::decode_feed(&bytes))
            .await
            .map_err(|e| err(format!("decode task: {e}")))??;
        let mut value = query::decode_message(&message, offset, limit, max_chars);
        if let Some(object) = value.as_object_mut() {
            object.insert("bytes".into(), json!(bytes_len));
        }
        Ok(value)
    }

    /// Runs `f` against one loaded feed.
    ///
    /// `F` is called with the loaded [`StaticDb`] and an [`RtView`] of the cached
    /// protobufs. When `f` returns a JSON object, `feed_id` is set on it. The feed is
    /// refreshed first when a TTL has elapsed.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - Feed to read. Omit it when exactly one feed is loaded.
    /// * `f` - Builds the JSON answer from the static database and realtime view.
    ///
    /// # Returns
    ///
    /// The closure's JSON, with `feed_id` set when the value is an object.
    ///
    /// # Errors
    ///
    /// * No feed is registered, several feeds are registered and `feed_id` was omitted,
    ///   or `feed_id` is unknown.
    /// * The id names a feed that failed to load.
    /// * A TTL refresh fails and the static database has no stops.
    /// * The loaded feed is gone before `f` runs.
    /// * `f` returns an error.
    async fn query<F>(&self, feed_id: Option<String>, f: F) -> Result<Value, AppError>
    where
        F: FnOnce(&StaticDb, RtView<'_>) -> Result<Value, AppError>,
    {
        let id = self.require_loaded(feed_id.as_deref())?;
        self.ensure(&id, false).await?;
        let slot = self.slot(&id)?;
        let data = lock(&slot.data);
        let view = RtView {
            trip_updates: data.trip_updates.as_ref().map(|c| &c.message),
            vehicles: data.vehicles.as_ref().map(|c| &c.message),
            alerts: data.alerts.as_ref().map(|c| &c.message),
        };
        let mut value = f(&data.db, view)?;
        if let Some(object) = value.as_object_mut() {
            object.insert("feed_id".into(), json!(id));
        }
        Ok(value)
    }

    /// Reads health for a loaded feed and attaches config errors.
    ///
    /// # Arguments
    ///
    /// * `id` - Loaded feed id.
    ///
    /// # Returns
    ///
    /// The [`health_json`] object, plus `config_errors` when any have been recorded.
    ///
    /// # Errors
    ///
    /// Returns an error when no loaded feed has this id.
    fn health_cached(&self, id: &str) -> Result<Value, AppError> {
        let value = {
            let slot = self.slot(id)?;
            let data = lock(&slot.data);
            health_json(&data, now_unix())
        };
        Ok(self.decorate(value))
    }

    /// Picks the feed id for a call that may omit it.
    ///
    /// Loaded ids and failed ids both count.
    ///
    /// # Arguments
    ///
    /// * `requested` - Caller-supplied id. Blank text is treated as omitted.
    ///
    /// # Returns
    ///
    /// The trimmed id when it is known. When `requested` is omitted and exactly one
    /// feed is registered, that id is returned.
    ///
    /// # Errors
    ///
    /// * No feed is registered. The message is `no feed registered`, or
    ///   `feed configuration is invalid` when startup config errors were recorded.
    /// * `requested` is omitted or blank while more than one feed is registered.
    /// * `requested` is non-empty and matches neither a loaded feed nor a failed feed.
    ///   Known ids are included. Recorded config errors are attached when any exist.
    ///
    /// # Panics
    ///
    /// Panics if the single known id cannot be taken from the list. That arm runs
    /// only when the list length is 1.
    fn resolve_id(&self, requested: Option<&str>) -> Result<String, AppError> {
        let feeds = lock(&self.feeds);
        let failed = lock(&self.failed);
        let mut known: Vec<String> = feeds
            .keys()
            .cloned()
            .chain(failed.keys().cloned())
            .collect();
        known.sort();
        known.dedup();
        if let Some(id) = requested.map(str::trim).filter(|id| !id.is_empty()) {
            if known.iter().any(|known| known == id) {
                return Ok(id.to_string());
            }
            return Err(self.with_config_errors(
                err(format!("no feed named '{id}'")).extra("feeds", json!(known)),
            ));
        }
        match known.len() {
            0 => {
                let errors = lock(&self.config_errors).clone();
                drop(failed);
                drop(feeds);
                if errors.is_empty() {
                    Err(err("no feed registered")
                        .hint("set GTFS_MCP_STATIC in the MCP client env, or call register_feed"))
                } else {
                    Err(err("feed configuration is invalid")
                        .hint("see config_errors")
                        .extra("config_errors", json!(errors)))
                }
            }
            1 => Ok(known.pop().unwrap()),
            _ => Err(self.with_config_errors(
                err("multiple feeds are registered; pass feed_id").extra("feeds", json!(known)),
            )),
        }
    }

    /// Resolves an id and rejects feeds that failed to load.
    ///
    /// # Arguments
    ///
    /// * `requested` - Caller-supplied id. Blank text is treated as omitted.
    ///
    /// # Returns
    ///
    /// The id of a feed that is currently loaded.
    ///
    /// # Errors
    ///
    /// * [`Catalog::resolve_id`] fails.
    /// * The id names a feed that failed to load. The hint tells the caller to fix the
    ///   configuration and call `refresh_feed`.
    fn require_loaded(&self, requested: Option<&str>) -> Result<String, AppError> {
        let id = self.resolve_id(requested)?;
        if let Some(message) = self.failure_message(&id) {
            return Err(err(message).hint(
                "this feed did not load from its startup configuration; fix it and call refresh_feed",
            ));
        }
        Ok(id)
    }

    /// Attaches recorded config errors to `error` when the list is non-empty.
    ///
    /// # Arguments
    ///
    /// * `error` - Error to return. Left unchanged when no config errors are stored.
    ///
    /// # Returns
    ///
    /// `error`, with `config_errors` set when the catalog has any.
    fn with_config_errors(&self, error: AppError) -> AppError {
        let errors = lock(&self.config_errors).clone();
        if errors.is_empty() {
            error
        } else {
            error.extra("config_errors", json!(errors))
        }
    }

    /// Returns the stored error for a feed that failed to load.
    ///
    /// # Arguments
    ///
    /// * `id` - Feed id.
    ///
    /// # Returns
    ///
    /// The redacted error text, or `None` when `id` is not in the failed map.
    fn failure_message(&self, id: &str) -> Option<String> {
        lock(&self.failed)
            .get(id)
            .map(|failure| failure.error.clone())
    }

    /// Returns the failure JSON for `id`, with config errors attached.
    ///
    /// # Arguments
    ///
    /// * `id` - Feed id.
    ///
    /// # Returns
    ///
    /// The decorated [`failure_json`] object, or `None` when `id` is not in the failed map.
    fn failure_document(&self, id: &str) -> Option<Value> {
        lock(&self.failed)
            .get(id)
            .map(|failure| self.decorate(failure_json(failure)))
    }

    /// Inserts `config_errors` into a JSON object when any are stored.
    ///
    /// # Arguments
    ///
    /// * `value` - Response to decorate. Non-objects are returned unchanged.
    ///
    /// # Returns
    ///
    /// `value`, with `config_errors` set on an object when the list is non-empty.
    fn decorate(&self, mut value: Value) -> Value {
        let errors = lock(&self.config_errors).clone();
        if errors.is_empty() {
            return value;
        }
        if let Some(object) = value.as_object_mut() {
            object.insert("config_errors".into(), json!(errors));
        }
        value
    }

    /// Returns the loaded slot for `id`.
    ///
    /// # Arguments
    ///
    /// * `id` - Feed id.
    ///
    /// # Returns
    ///
    /// A clone of the slot's `Arc`.
    ///
    /// # Errors
    ///
    /// Returns `no feed named '<id>'` when the id is not loaded.
    fn slot(&self, id: &str) -> Result<std::sync::Arc<Slot>, AppError> {
        lock(&self.feeds)
            .get(id)
            .cloned()
            .ok_or_else(|| err(format!("no feed named '{id}'")))
    }

    /// Refreshes `id` when a TTL has elapsed, or always when `force` is set.
    ///
    /// Callers of one feed wait on the slot's refresh lock. A realtime-only refresh
    /// replaces all three protobuf caches. A static refresh replaces the whole
    /// [`FeedData`]. When the download fails and the database already has stops, the
    /// previous state stays and [`FeedData::last_error`] is set.
    ///
    /// # Arguments
    ///
    /// * `id` - Loaded feed id.
    /// * `force` - When true, download the static zip and the realtime protobufs
    ///   even if their TTLs have not elapsed.
    ///
    /// # Returns
    ///
    /// `()` when the cache is already fresh, the refresh succeeds, or the refresh
    /// fails and the static database still has stops.
    ///
    /// # Errors
    ///
    /// * No loaded feed has this id.
    /// * The refresh fails and the static database has no stops. The fetch, zip, or
    ///   protobuf error is returned as-is.
    async fn ensure(&self, id: &str, force: bool) -> Result<(), AppError> {
        let slot = self.slot(id)?;
        let _guard = slot.refresh.lock().await;
        let plan = {
            let data = lock(&slot.data);
            let rt_age = data
                .rt_fetched
                .map(|t| t.elapsed())
                .unwrap_or(Duration::MAX);
            let static_age = data.static_checked.elapsed();
            let need_rt = force || rt_age >= data.rt_ttl;
            let recheck_static = data.static_ttl > Duration::ZERO && static_age >= data.static_ttl;
            if !need_rt && !recheck_static {
                return Ok(());
            }
            RefreshPlan {
                id: data.id.clone(),
                spec: data.spec.clone(),
                headers: data.headers.clone(),
                rt_ttl: data.rt_ttl,
                static_ttl: data.static_ttl,
                reload_static: force || recheck_static,
            }
        };
        let update = match refresh_loaded(&self.http, &plan).await {
            Ok(update) => update,
            Err(error) => {
                let mut data = lock(&slot.data);
                data.last_error = Some(error.error.clone());
                if data.db.counts.stops > 0 {
                    return Ok(());
                }
                return Err(error);
            }
        };
        let mut data = lock(&slot.data);
        apply_update(&mut data, update);
        Ok(())
    }
}

/// What a refresh should download, copied out of the feed lock before any await.
struct RefreshPlan {
    /// Feed id being loaded.
    id: String,
    /// Locations to fetch.
    spec: FeedSpec,
    /// Headers sent on each fetch in this plan.
    headers: BTreeMap<String, String>,
    /// Realtime TTL copied onto the loaded feed.
    rt_ttl: Duration,
    /// Static TTL copied onto the loaded feed.
    static_ttl: Duration,
    /// When true, the static zip is downloaded. When false, only realtime bytes are fetched.
    reload_static: bool,
}

/// Replacement realtime caches from a refresh that left the static database in place.
struct RealtimeUpdate {
    /// New TripUpdates cache, or `None` when that product is not configured.
    trip_updates: Option<CachedFeed>,
    /// New VehiclePositions cache, or `None` when that product is not configured.
    vehicles: Option<CachedFeed>,
    /// New Alerts cache, or `None` when that product is not configured.
    alerts: Option<CachedFeed>,
}

/// A refresh result applied to a loaded [`FeedData`].
enum FeedUpdate {
    /// Static GTFS and realtime protobufs were both loaded. Replaces the whole feed.
    Full(Box<FeedData>),
    /// Only the realtime protobufs were replaced.
    Realtime(Box<RealtimeUpdate>),
}

/// Writes `update` into `data`.
///
/// A [`FeedUpdate::Full`] replaces every field. A [`FeedUpdate::Realtime`] replaces
/// the three protobuf caches, sets the realtime timestamps, and clears
/// [`FeedData::last_error`].
///
/// # Arguments
///
/// * `data` - Feed state to update.
/// * `update` - Refresh result.
fn apply_update(data: &mut FeedData, update: FeedUpdate) {
    match update {
        FeedUpdate::Full(fresh) => *data = *fresh,
        FeedUpdate::Realtime(update) => {
            data.trip_updates = update.trip_updates;
            data.vehicles = update.vehicles;
            data.alerts = update.alerts;
            data.rt_fetched = Some(Instant::now());
            data.rt_fetched_unix = now_unix() as u64;
            data.last_error = None;
        }
    }
}

/// Downloads the pieces named by `plan`.
///
/// # Arguments
///
/// * `http` - Client used for HTTP locations.
/// * `plan` - Locations, headers, and whether the static zip is included.
///
/// # Returns
///
/// [`FeedUpdate::Realtime`] when the static zip is left in place, or
/// [`FeedUpdate::Full`] when it is downloaded again.
///
/// # Errors
///
/// * A configured file cannot be read, an HTTP request fails, or the status is not successful.
/// * A body exceeds [`MAX_STATIC_BYTES`] or [`MAX_RT_BYTES`], or a response declares a length over the cap.
/// * Realtime bytes are empty, gzip decompression fails, or they are not a FeedMessage.
/// * The static bytes are not a GTFS zip, a CSV or SQLite step fails, or `stops.txt` is missing.
/// * A blocking parse or decode task fails to join.
async fn refresh_loaded(
    http: &reqwest::Client,
    plan: &RefreshPlan,
) -> Result<FeedUpdate, AppError> {
    if !plan.reload_static {
        let (trip_updates, vehicles, alerts) = download_triplet(
            http,
            &plan.headers,
            plan.spec.trip_updates.as_ref(),
            plan.spec.vehicle_positions.as_ref(),
            plan.spec.alerts.as_ref(),
        )
        .await?;
        return Ok(FeedUpdate::Realtime(Box::new(RealtimeUpdate {
            trip_updates,
            vehicles,
            alerts,
        })));
    }
    Ok(FeedUpdate::Full(Box::new(load_direct(http, plan).await?)))
}

/// Downloads the static zip and the configured realtime protobufs.
///
/// # Arguments
///
/// * `http` - Client used for HTTP locations.
/// * `id` - Feed id stored on the result.
/// * `spec` - Locations to fetch.
/// * `headers` - Headers sent on every fetch.
/// * `rt_ttl` - Realtime TTL stored on the result.
/// * `static_ttl` - Static TTL stored on the result.
///
/// # Returns
///
/// A [`FeedData`] with both timestamps set to the load time and [`FeedData::last_error`] clear.
///
/// # Errors
///
/// Returns the same errors as [`load_direct`].
async fn load_feed(
    http: &reqwest::Client,
    id: String,
    spec: FeedSpec,
    headers: BTreeMap<String, String>,
    rt_ttl: Duration,
    static_ttl: Duration,
) -> Result<FeedData, AppError> {
    let plan = RefreshPlan {
        id,
        spec,
        headers,
        rt_ttl,
        static_ttl,
        reload_static: true,
    };
    load_direct(http, &plan).await
}

/// Downloads the static zip, parses it, then downloads the realtime triplet.
///
/// # Arguments
///
/// * `http` - Client used for HTTP locations.
/// * `plan` - Locations, headers, id, and TTLs. `reload_static` is ignored here;
///   the zip is always fetched.
///
/// # Returns
///
/// The loaded feed. Realtime fields are `None` when that product is not configured.
///
/// # Errors
///
/// * The static zip cannot be read, the HTTP request fails, the status is not
///   successful, or the body exceeds [`MAX_STATIC_BYTES`].
/// * The blocking parse task fails to join.
/// * The bytes are not a GTFS zip, a CSV or SQLite step fails, or `stops.txt` is missing.
/// * A realtime product fails the errors of [`download_feed`].
async fn load_direct(http: &reqwest::Client, plan: &RefreshPlan) -> Result<FeedData, AppError> {
    let zip = fetch_bytes(
        http,
        &plan.spec.static_gtfs,
        &plan.headers,
        MAX_STATIC_BYTES,
    )
    .await?;
    let db = tokio::task::spawn_blocking(move || StaticDb::load_zip(&zip))
        .await
        .map_err(|e| err(format!("static load: {e}")))??;
    let (trip_updates, vehicles, alerts) = download_triplet(
        http,
        &plan.headers,
        plan.spec.trip_updates.as_ref(),
        plan.spec.vehicle_positions.as_ref(),
        plan.spec.alerts.as_ref(),
    )
    .await?;
    Ok(FeedData {
        id: plan.id.clone(),
        spec: plan.spec.clone(),
        headers: plan.headers.clone(),
        rt_ttl: plan.rt_ttl,
        static_ttl: plan.static_ttl,
        db,
        static_fetched: Instant::now(),
        static_fetched_unix: now_unix() as u64,
        static_checked: Instant::now(),
        trip_updates,
        vehicles,
        alerts,
        rt_fetched: Some(Instant::now()),
        rt_fetched_unix: now_unix() as u64,
        last_error: None,
    })
}

/// Downloads TripUpdates, VehiclePositions, and Alerts, in that order.
///
/// # Arguments
///
/// * `http` - Client used for HTTP locations.
/// * `headers` - Headers sent on every fetch.
/// * `trip_updates` - TripUpdates location. `None` skips that product.
/// * `vehicles` - VehiclePositions location. `None` skips that product.
/// * `alerts` - Alerts location. `None` skips that product.
///
/// # Returns
///
/// The three caches. A `None` location becomes a `None` cache.
///
/// # Errors
///
/// Returns the first error from [`download_feed`]. Later products are not fetched
/// after that.
async fn download_triplet(
    http: &reqwest::Client,
    headers: &BTreeMap<String, String>,
    trip_updates: Option<&Loc>,
    vehicles: Option<&Loc>,
    alerts: Option<&Loc>,
) -> Result<(Option<CachedFeed>, Option<CachedFeed>, Option<CachedFeed>), AppError> {
    let trip_updates = download_feed(http, headers, trip_updates).await?;
    let vehicles = download_feed(http, headers, vehicles).await?;
    let alerts = download_feed(http, headers, alerts).await?;
    Ok((trip_updates, vehicles, alerts))
}

/// Downloads and decodes one realtime protobuf.
///
/// # Arguments
///
/// * `http` - Client used for an HTTP location.
/// * `headers` - Headers sent on the fetch.
/// * `loc` - Location to read. `None` skips the product.
///
/// # Returns
///
/// `Ok(None)` when `loc` is `None`. Otherwise the decoded cache, including the
/// payload size and the location label.
///
/// # Errors
///
/// * The file cannot be read, the HTTP request fails, or the status is not successful.
/// * The body exceeds [`MAX_RT_BYTES`], or the response declares a length over that cap.
/// * The bytes are empty, gzip decompression fails, or they are not a FeedMessage.
/// * The blocking decode task fails to join.
async fn download_feed(
    http: &reqwest::Client,
    headers: &BTreeMap<String, String>,
    loc: Option<&Loc>,
) -> Result<Option<CachedFeed>, AppError> {
    let Some(loc) = loc else {
        return Ok(None);
    };
    let source = loc_label(loc);
    let bytes = fetch_bytes(http, loc, headers, MAX_RT_BYTES).await?;
    let len = bytes.len();
    let message = tokio::task::spawn_blocking(move || realtime::decode_feed(&bytes))
        .await
        .map_err(|e| err(format!("decode task: {e}")))??;
    Ok(Some(CachedFeed {
        message,
        bytes: len,
        source,
    }))
}

/// Builds the health object for a loaded feed.
///
/// # Arguments
///
/// * `data` - Loaded feed.
/// * `now` - Current Unix time, used to age realtime header timestamps.
///
/// # Returns
///
/// JSON with `feed_id`, `ready` true, `source` (`kind` is `direct`, plus the four
/// location labels), `headers` (names only), `static` (agency, timezone, version,
/// publisher, table counts, `fetched_unix`, `fetched_age_sec`), and `realtime`
/// (`ttl_sec`, `fetched_age_sec`, `stale`, `last_error`, and the three
/// [`feed_health`] objects). A missing realtime product is JSON null. A missing
/// `rt_fetched` is reported as stale with a huge age.
fn health_json(data: &FeedData, now: i64) -> Value {
    let source = json!({
        "kind": "direct",
        "static": loc_label(&data.spec.static_gtfs),
        "trip_updates": data.spec.trip_updates.as_ref().map(loc_label),
        "vehicle_positions": data.spec.vehicle_positions.as_ref().map(loc_label),
        "alerts": data.spec.alerts.as_ref().map(loc_label),
    });
    let rt_age = data
        .rt_fetched
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(u64::MAX);
    json!({
        "feed_id": data.id,
        "ready": true,
        "source": source,
        "headers": data.headers.keys().cloned().collect::<Vec<_>>(),
        "static": {
            "agency": data.db.agency_name,
            "timezone": data.db.tz_name,
            "feed_version": data.db.feed_version,
            "publisher": data.db.publisher,
            "agencies": data.db.counts.agencies,
            "stops": data.db.counts.stops,
            "routes": data.db.counts.routes,
            "trips": data.db.counts.trips,
            "stop_times": data.db.counts.stop_times,
            "calendar": data.db.counts.calendar,
            "calendar_dates": data.db.counts.calendar_dates,
            "fetched_unix": data.static_fetched_unix,
            "fetched_age_sec": data.static_fetched.elapsed().as_secs(),
        },
        "realtime": {
            "ttl_sec": data.rt_ttl.as_secs(),
            "fetched_age_sec": rt_age.min(i64::MAX as u64),
            "stale": data.rt_fetched.map(|t| t.elapsed() > data.rt_ttl).unwrap_or(true),
            "last_error": data.last_error,
            "trip_updates": feed_health(data.trip_updates.as_ref(), now),
            "vehicle_positions": feed_health(data.vehicles.as_ref(), now),
            "alerts": feed_health(data.alerts.as_ref(), now),
        },
    })
}

/// Builds the health object for a feed that failed to load.
///
/// # Arguments
///
/// * `failure` - Stored failure.
///
/// # Returns
///
/// JSON with `feed_id`, `ready` false, `error`, `hint`, `headers` (names only),
/// and `source`. Each source field is the request's URL or path for that product.
fn failure_json(failure: &FailedFeed) -> Value {
    let request = &failure.request;
    let source = json!({
        "kind": "direct",
        "static": request.static_url.clone().or(request.static_path.clone()),
        "trip_updates": request.trip_updates_url.clone().or(request.trip_updates_path.clone()),
        "vehicle_positions": request
            .vehicle_positions_url
            .clone()
            .or(request.vehicle_positions_path.clone()),
        "alerts": request.alerts_url.clone().or(request.alerts_path.clone()),
    });
    realtime::obj([
        ("feed_id", json!(failure.request.feed_id)),
        ("ready", json!(false)),
        ("error", json!(failure.error)),
        ("hint", realtime::json_opt(failure.hint.clone())),
        (
            "headers",
            json!(request.headers.keys().cloned().collect::<Vec<_>>()),
        ),
        ("source", source),
    ])
}

/// Builds the health object for one cached protobuf.
///
/// # Arguments
///
/// * `feed` - Cached message. `None` means the product is unset.
/// * `now` - Current Unix time, subtracted from the header timestamp.
///
/// # Returns
///
/// JSON null when `feed` is `None`. Otherwise `entities`, `bytes`, `source`,
/// `header_timestamp`, `header_age_sec`, `incrementality`, and `feed_version`.
fn feed_health(feed: Option<&CachedFeed>, now: i64) -> Value {
    let Some(feed) = feed else {
        return Value::Null;
    };
    let timestamp = feed.message.header.timestamp;
    let header_age = timestamp.and_then(|ts| now.checked_sub(ts as i64));
    realtime::obj([
        ("entities", json!(feed.message.entity.len())),
        ("bytes", json!(feed.bytes)),
        ("source", json!(feed.source)),
        (
            "header_timestamp",
            realtime::json_i64(timestamp.map(|v| v as i64)),
        ),
        ("header_age_sec", realtime::json_i64(header_age)),
        (
            "incrementality",
            realtime::json_opt(incrementality(feed.message.header.incrementality)),
        ),
        (
            "feed_version",
            realtime::json_opt(feed.message.header.feed_version.clone()),
        ),
    ])
}

/// Wraps a page of rows with the usual list fields.
///
/// # Arguments
///
/// * `feed_id` - Feed that produced the rows.
/// * `key` - JSON field that holds `rows`, such as `stops` or `routes`.
/// * `rows` - This page of values.
/// * `total` - Number of matches before paging.
/// * `offset` - Number of matches skipped.
///
/// # Returns
///
/// JSON with `feed_id`, `total`, `offset`, `returned`, `next_offset`, and the
/// rows stored under the field name passed as `key`. `next_offset` is null when
/// this page reaches `total`.
fn page_list(feed_id: &str, key: &str, rows: Vec<Value>, total: usize, offset: usize) -> Value {
    let next = offset + rows.len();
    let mut value = json!({
        "feed_id": feed_id,
        "total": total,
        "offset": offset,
        "returned": rows.len(),
        "next_offset": if next < total { serde_json::Value::from(next) } else { Value::Null },
    });
    if let Some(object) = value.as_object_mut() {
        object.insert(key.to_string(), Value::Array(rows));
    }
    value
}

/// Turns a registration request into a [`FeedSpec`].
///
/// # Arguments
///
/// * `request` - Locations to read. TTLs and headers are ignored.
///
/// # Returns
///
/// The static location plus any realtime locations that were set.
///
/// # Errors
///
/// * The static URL and path are both missing or blank (`pass static_url or static_path`).
/// * Static GTFS, trip updates, vehicle positions, or alerts sets both a URL and a path.
/// * A URL uses a scheme other than `http`, `https`, or `file`.
fn spec_from_request(request: &RegisterRequest) -> Result<FeedSpec, AppError> {
    let Some(static_gtfs) = one_loc(
        request.static_url.clone(),
        request.static_path.clone(),
        "static GTFS",
    )?
    else {
        return Err(err("pass static_url or static_path"));
    };
    let trip_updates = one_loc(
        request.trip_updates_url.clone(),
        request.trip_updates_path.clone(),
        "trip updates",
    )?;
    let vehicle_positions = one_loc(
        request.vehicle_positions_url.clone(),
        request.vehicle_positions_path.clone(),
        "vehicle positions",
    )?;
    let alerts = one_loc(
        request.alerts_url.clone(),
        request.alerts_path.clone(),
        "alerts",
    )?;
    Ok(FeedSpec {
        static_gtfs,
        trip_updates,
        vehicle_positions,
        alerts,
    })
}

/// Resolves one URL-or-path pair into a location.
///
/// # Arguments
///
/// * `url` - URL field. Whitespace-only text is ignored.
/// * `path` - Filesystem path field. Whitespace-only text is ignored.
/// * `label` - Product name used in the error when both are set.
///
/// # Returns
///
/// `Ok(None)` when both sides are empty. A path becomes [`Loc::File`]. A URL is
/// parsed by [`parse_user_loc`].
///
/// # Errors
///
/// * Both sides are non-empty (`<label>: pass a url or a path, not both`).
/// * `url` uses an unsupported scheme.
fn one_loc(
    url: Option<String>,
    path: Option<String>,
    label: &str,
) -> Result<Option<Loc>, AppError> {
    match (nonempty(url), nonempty(path)) {
        (Some(_), Some(_)) => Err(err(format!("{label}: pass a url or a path, not both"))),
        (Some(url), None) => Ok(Some(parse_user_loc(&url)?)),
        (None, Some(path)) => Ok(Some(Loc::File(PathBuf::from(path)))),
        (None, None) => Ok(None),
    }
}

/// Parses a caller-supplied URL or path.
///
/// # Arguments
///
/// * `value` - Trimmed location text.
///
/// # Returns
///
/// [`Loc::Http`] for `http://` and `https://`. [`Loc::File`] for `file://` (the
/// remainder is percent-decoded) and for text with no scheme.
///
/// # Errors
///
/// Returns an error when `value` contains `://` and the scheme is not `http`,
/// `https`, or `file`. The hint says to use `http(s)` or a filesystem path.
fn parse_user_loc(value: &str) -> Result<Loc, AppError> {
    if value.starts_with("http://") || value.starts_with("https://") {
        Ok(Loc::Http(value.to_string()))
    } else if let Some(rest) = value.strip_prefix("file://") {
        Ok(Loc::File(PathBuf::from(percent_decode(rest))))
    } else if value.contains("://") {
        Err(err(format!("unsupported scheme in '{value}'"))
            .hint("use http(s) or a filesystem path"))
    } else {
        Ok(Loc::File(PathBuf::from(value)))
    }
}

/// Decodes `%HH` sequences in a `file://` path.
///
/// # Arguments
///
/// * `value` - Path after the `file://` prefix.
///
/// # Returns
///
/// The decoded path. A `%` that is not followed by two hex digits is copied
/// through. Bytes that are not UTF-8 are replaced.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            ) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Trims `value` and drops it when the result is empty.
///
/// # Arguments
///
/// * `value` - Optional field from a [`RegisterRequest`].
///
/// # Returns
///
/// The trimmed string, or `None` when `value` is missing or blank.
fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

/// Checks a startup request without downloading it.
///
/// # Arguments
///
/// * `request` - Feed the process wants to load before it serves MCP.
///
/// # Returns
///
/// The normalized feed id. A missing or blank id becomes `default`.
///
/// # Errors
///
/// * A header name is empty or contains CR, LF, or `:`, or a header value contains
///   CR or LF.
/// * `feed_id` is non-empty and fails [`normalize_id`].
/// * [`spec_from_request`] rejects the locations.
/// * `rt_ttl_secs` is set and is outside 1-3600 (`realtime TTL must be 1-3600 seconds`).
/// * `static_ttl_secs` is set and is greater than 604800
///   (`static TTL must be 0-604800 seconds`).
pub(crate) fn validate_startup_request(request: &RegisterRequest) -> Result<String, AppError> {
    validate_headers(&request.headers)?;
    let id = normalize_id(request.feed_id.as_deref())?;
    spec_from_request(request)?;
    if let Some(ttl) = request.rt_ttl_secs {
        if !(1..=3600).contains(&ttl) {
            return Err(err(format!(
                "realtime TTL must be 1-3600 seconds, got {ttl}"
            )));
        }
    }
    if let Some(ttl) = request.static_ttl_secs {
        if ttl > 7 * 24 * 3600 {
            return Err(err(format!(
                "static TTL must be 0-604800 seconds, got {ttl}"
            )));
        }
    }
    Ok(id)
}

/// Normalizes a feed id, using `default` when the caller omitted one.
///
/// # Arguments
///
/// * `id` - Raw id. `None`, empty, and whitespace-only all become `default`.
///
/// # Returns
///
/// The trimmed id.
///
/// # Errors
///
/// Returns an error when the trimmed id is longer than 64 characters or contains a
/// character other than ASCII alphanumeric, `_`, `.`, `:`, or `-`.
pub(crate) fn normalize_id(id: Option<&str>) -> Result<String, AppError> {
    let id = id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or("default");
    if id.len() > 64
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | ':' | '-'))
    {
        return Err(err(
            "feed_id must be 1-64 characters of letters, numbers, '_', '.', ':', or '-'",
        ));
    }
    Ok(id.to_string())
}

/// Decodes standard base64, with or without padding.
///
/// # Arguments
///
/// * `value` - Encoded protobuf. Whitespace is removed first.
///
/// # Returns
///
/// The decoded bytes.
///
/// # Errors
///
/// Returns `base64: ...` when both the padded and unpadded standard alphabets reject `value`.
fn decode_base64(value: &str) -> Result<Vec<u8>, AppError> {
    let cleaned: String = value.chars().filter(|ch| !ch.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(&cleaned)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned))
        .map_err(|e| err(format!("base64: {e}")))
}

/// Returns the current Unix time in seconds.
///
/// # Returns
///
/// Seconds since the Unix epoch. A clock set before the epoch yields `0`.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Locks `mutex`, recovering the value when the mutex is poisoned.
///
/// `T` is the value the mutex protects.
///
/// # Arguments
///
/// * `mutex` - Mutex to lock.
///
/// # Returns
///
/// The guard. A poisoned mutex yields its inner value.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Catalog tests against the Harbor Line fixture.
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Creates a unique temporary directory for one test.
    ///
    /// # Arguments
    ///
    /// * `label` - Short name included in the directory path.
    ///
    /// # Returns
    ///
    /// The directory path. The caller removes it.
    ///
    /// # Panics
    ///
    /// Panics if the clock is before the Unix epoch or the directory cannot be created.
    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "gtfs-mcp-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Builds a [`RegisterRequest`] that reads the Harbor fixture from disk.
    ///
    /// # Arguments
    ///
    /// * `paths` - Files written by [`crate::fixtures::materialize`].
    /// * `feed_id` - Id stored on the request.
    ///
    /// # Returns
    ///
    /// A request with the static zip and the three realtime protobuf paths set.
    fn harbor_request(paths: &fixtures::HarborPaths, feed_id: &str) -> RegisterRequest {
        RegisterRequest {
            feed_id: Some(feed_id.into()),
            static_path: Some(paths.static_zip.display().to_string()),
            trip_updates_path: Some(paths.trip_updates.display().to_string()),
            vehicle_positions_path: Some(paths.vehicle_positions.display().to_string()),
            alerts_path: Some(paths.alerts.display().to_string()),
            ..Default::default()
        }
    }

    /// Registers the Harbor fixture from disk and checks status, board, alerts, and decode.
    ///
    /// # Panics
    ///
    /// Panics if fixture setup, a catalog call, or an assertion fails.
    #[tokio::test]
    async fn harbor_answers_status_board_and_alerts() {
        let dir = temp_dir("harbor");
        let paths = fixtures::materialize(&dir).unwrap();
        let catalog = Catalog::new().unwrap();
        catalog
            .register(harbor_request(&paths, "harbor"))
            .await
            .unwrap();

        let status = catalog
            .trip_status(None, "177", Some("NHV".into()), Some(paths.at_unix), 6)
            .await
            .unwrap();
        let trip = &status["trips"][0];
        assert_eq!(trip["trip_short_name"], "177");
        assert_eq!(trip["route_short_name"], "NEC");
        assert_eq!(trip["vehicle"]["status"], "IN_TRANSIT_TO");
        assert_eq!(trip["vehicle"]["stop_id"], "NHV");
        assert_eq!(trip["at_stop"]["track"], "12");
        assert_eq!(trip["at_stop"]["assigned_stop_id"], "NHV:track:12");
        assert_eq!(trip["at_stop"]["arrival_delay_sec"], 420);
        assert_eq!(trip["at_stop"]["departure_delay_sec"], 480);
        assert_eq!(trip["at_stop"]["scheduled_departure_local"], "12:04");
        assert_eq!(trip["at_stop"]["estimated_departure_local"], "12:12");
        assert!(trip["delay_label"].as_str().unwrap().contains("late"));

        let board = catalog
            .departure_board(None, "NHV", None, Some(paths.at_unix), 120, 5, 20, 0, false)
            .await
            .unwrap();
        let short_names: Vec<_> = board["departures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["trip_short_name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(short_names, vec!["177".to_string(), "B1".to_string()]);
        assert_eq!(board["departures"][0]["track"], "12");

        let from_platform = catalog
            .departure_board(
                None,
                "NHV:track:12",
                None,
                Some(paths.at_unix),
                120,
                5,
                20,
                0,
                false,
            )
            .await
            .unwrap();
        assert_eq!(from_platform["departures"][0]["trip_short_name"], "177");

        let alerts = catalog
            .alerts(
                None,
                AlertQuery {
                    route: Some("NEC".into()),
                    stop: None,
                    text: None,
                    active_only: true,
                    at_unix: paths.at_unix,
                    limit: 10,
                    offset: 0,
                    max_chars: 200,
                },
            )
            .await
            .unwrap();
        assert_eq!(alerts["returned"], 1);
        assert_eq!(
            alerts["alerts"][0]["header"],
            "Track work between NYP and NHV"
        );
        assert_eq!(alerts["alerts"][0]["effect"], "SIGNIFICANT_DELAYS");
        assert_eq!(
            alerts["alerts"][0]["informed_entity"][0]["route_short_name"],
            "NEC"
        );

        let decoded = catalog
            .decode(
                Some(paths.trip_updates.display().to_string()),
                None,
                None,
                0,
                10,
                200,
                BTreeMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            decoded["entities"][0]["trip_update"]["stop_updates"][1]["assigned_stop_id"],
            "NHV:track:12"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Serves the Harbor fixture on localhost and registers those HTTP URLs.
    ///
    /// # Panics
    ///
    /// Panics if the fixture, the local listener, a catalog call, or an assertion fails.
    #[tokio::test]
    async fn http_direct_urls_serve_harbor_bytes() {
        let dir = temp_dir("http-feed");
        let _paths = fixtures::materialize(&dir).unwrap();
        let root = Arc::new(dir.clone());
        let app = axum::Router::new().route(
            "/{name}",
            axum::routing::get({
                let root = Arc::clone(&root);
                move |axum::extract::Path(name): axum::extract::Path<String>| {
                    let root = Arc::clone(&root);
                    async move {
                        match tokio::fs::read(root.join(&name)).await {
                            Ok(bytes) => (axum::http::StatusCode::OK, bytes),
                            Err(_) => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
                        }
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let catalog = Catalog::new().unwrap();
        let file_url = |name: &str| format!("http://{addr}/{name}");
        catalog
            .register(RegisterRequest {
                static_url: Some(file_url("static.zip")),
                trip_updates_url: Some(file_url("trip-updates.pb")),
                vehicle_positions_url: Some(file_url("vehicle-positions.pb")),
                alerts_url: Some(file_url("alerts.pb")),
                ..Default::default()
            })
            .await
            .unwrap();
        let health = catalog.health(None).await.unwrap();
        assert_eq!(health["source"]["kind"], "direct");
        assert_eq!(health["realtime"]["vehicle_positions"]["entities"], 1);
        assert_eq!(health["static"]["stops"], 6);
        assert!(health.get("feed_set").is_none(), "{health}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Shows a failed startup feed, redacts its header value, and clears it on register.
    ///
    /// The header value is longer than three characters, so the stored error contains
    /// `[redacted]` and omits the secret.
    ///
    /// # Panics
    ///
    /// Panics if catalog setup, a catalog call, or an assertion fails.
    #[tokio::test]
    async fn failed_startup_feed_is_visible_and_register_clears_it() {
        let secret = "header-secret-value";
        let catalog = Catalog::new().unwrap();
        catalog.add_config_errors(vec![json!({
            "error": "startup note",
            "var": "GTFS_MCP_STATIC"
        })]);
        let mut headers = BTreeMap::new();
        headers.insert("x-api-key".into(), secret.into());
        catalog.note_load_failure(
            RegisterRequest {
                feed_id: Some("extra".into()),
                static_path: Some("/no/such/gtfs-mcp-static.zip".into()),
                headers,
                ..Default::default()
            },
            &err(format!("connect failed token {secret}")),
        );

        let listed = catalog.list().to_string();
        assert!(listed.contains("extra"), "{listed}");
        assert!(listed.contains("\"ready\":false"), "{listed}");
        assert!(listed.contains("config_errors"), "{listed}");
        assert!(listed.contains("[redacted]"), "{listed}");
        assert!(!listed.contains(secret), "{listed}");

        let health = catalog.health(Some("extra".into())).await.unwrap();
        let health_text = health.to_string();
        assert_eq!(health["ready"], false);
        assert_eq!(health["headers"][0], "x-api-key");
        assert!(health_text.contains("config_errors"), "{health_text}");
        assert!(!health_text.contains(secret), "{health_text}");

        let query_error = catalog
            .trip_status(Some("extra".into()), "177", None, None, 6)
            .await
            .unwrap_err();
        assert!(
            query_error.error.contains("[redacted]"),
            "{}",
            query_error.error
        );
        assert!(!query_error.error.contains(secret), "{}", query_error.error);

        let refreshed = catalog.refresh(Some("extra".into())).await.unwrap_err();
        assert!(!refreshed.error.contains(secret), "{}", refreshed.error);
        assert!(catalog.list().to_string().contains("\"ready\":false"));

        let dir = temp_dir("clear-failure");
        let paths = fixtures::materialize(&dir).unwrap();
        catalog
            .register(harbor_request(&paths, "extra"))
            .await
            .unwrap();
        let health = catalog.health(Some("extra".into())).await.unwrap();
        assert_eq!(health["ready"], true);
        assert!(health.get("error").is_none(), "{health}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
