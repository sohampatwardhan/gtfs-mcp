//! Static GTFS schedules in an in-memory SQLite database.
//!
//! This module loads a GTFS static zip into an in-memory SQLite database and
//! queries stops, routes, trips, stop times, and calendars. [`StaticDb::load_zip`]
//! reads a zip, or gzip around a zip, including when the `.txt` files sit in a
//! subdirectory. Shapes, frequencies, transfers, and other GTFS files are ignored.
//!
//! Weekday rows and `calendar_dates.txt` exceptions stay beside the database and
//! answer [`StaticDb::service_active`]. Text search folds case for names and
//! codes. An exact `stop_id` or `route_id` keeps the feed's casing.
//! [`parse_gtfs_time`] stores arrival and departure as seconds after midnight, so
//! the hour may be 24 or greater. [`route_type_name`] names the basic GTFS route
//! types.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};

use chrono::{Datelike, NaiveDate, Weekday};
use chrono_tz::Tz;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use zip::ZipArchive;

use crate::error::{err, AppError};

/// Row counts for one loaded GTFS static feed.
///
/// Each value is a count of records kept after this module's filters, not a
/// raw line count from the CSV.
#[derive(Debug, Clone)]
pub struct Counts {
    /// Agencies kept from `agency.txt`. A row needs a name or a timezone.
    pub agencies: i64,
    /// Stops stored from `stops.txt`.
    pub stops: i64,
    /// Routes stored from `routes.txt`.
    pub routes: i64,
    /// Trips stored from `trips.txt`.
    pub trips: i64,
    /// Stop times stored from `stop_times.txt`.
    pub stop_times: i64,
    /// `service_id` values kept from `calendar.txt`.
    pub calendar: i64,
    /// Added dates plus removed dates from `calendar_dates.txt`.
    ///
    /// A date repeated for one service and one exception type counts once.
    pub calendar_dates: i64,
}

/// One location from `stops.txt`.
///
/// `location_type` follows GTFS: `0` is a stop or platform, `1` a station, `2` an
/// entrance, `3` a generic node, and `4` a boarding area.
#[derive(Debug, Clone)]
pub struct StopRow {
    /// GTFS `stop_id`. Primary key of the stop.
    pub stop_id: String,
    /// Public `stop_code`, when the feed has one.
    pub stop_code: Option<String>,
    /// Rider-facing `stop_name`, when the feed has one.
    pub stop_name: Option<String>,
    /// WGS84 latitude from `stop_lat`. `None` when the cell is empty or not a number.
    pub lat: Option<f64>,
    /// WGS84 longitude from `stop_lon`. `None` when the cell is empty or not a number.
    pub lon: Option<f64>,
    /// GTFS `location_type`. Missing or non-numeric cells are stored as `0`.
    pub location_type: i64,
    /// Parent station `stop_id`, when this location has one.
    pub parent_station: Option<String>,
    /// Platform identifier within the station, when the feed has one.
    pub platform_code: Option<String>,
}

/// One route from `routes.txt`.
#[derive(Debug, Clone)]
pub struct RouteRow {
    /// GTFS `route_id`. Primary key of the route.
    pub route_id: String,
    /// `route_short_name`, such as a line number.
    pub short_name: Option<String>,
    /// `route_long_name`.
    pub long_name: Option<String>,
    /// GTFS `route_type`. Missing or non-numeric cells are stored as `0`. See [`route_type_name`].
    pub route_type: i64,
    /// `route_color` as stored in the feed. Empty cells are `None`.
    pub color: Option<String>,
}

/// One trip from `trips.txt`, with names and `route_type` taken from its route.
#[derive(Debug, Clone)]
pub struct TripRow {
    /// GTFS `trip_id`. Primary key of the trip.
    pub trip_id: String,
    /// `route_id` from the trip. Empty when that cell was empty.
    pub route_id: String,
    /// `service_id` from the trip. Pass it to [`StaticDb::service_active`].
    pub service_id: String,
    /// `trip_headsign`.
    pub headsign: Option<String>,
    /// `trip_short_name`.
    pub short_name: Option<String>,
    /// `direction_id` when the cell is an integer.
    pub direction_id: Option<i64>,
    /// Short name of the joined route. `None` when that route is missing.
    pub route_short_name: Option<String>,
    /// Long name of the joined route. `None` when that route is missing.
    pub route_long_name: Option<String>,
    /// Joined route's `route_type`, or `0` when the route is missing. See [`route_type_name`].
    pub route_type: i64,
}

/// One scheduled stop on a trip.
///
/// Stop, trip, and route columns are joined onto the `stop_times.txt` row. A
/// missing stop or route leaves those fields empty; a missing trip drops the row.
#[derive(Debug, Clone)]
pub struct StopTimeRow {
    /// Trip this stop time belongs to.
    pub trip_id: String,
    /// `stop_sequence` along the trip. This is an order, not a clock time.
    pub stop_sequence: i64,
    /// Stop where this call is scheduled.
    pub stop_id: String,
    /// Name of this row's `stop_id`. `None` when the stop is missing or unnamed.
    pub stop_name: Option<String>,
    /// Platform code of this row's `stop_id`.
    pub platform_code: Option<String>,
    /// Arrival as seconds after midnight. See [`parse_gtfs_time`].
    ///
    /// `None` when `arrival_time` is empty or not `HH:MM:SS`. The hour may be 24 or greater.
    pub arrival_sec: Option<i64>,
    /// Departure as seconds after midnight. See [`parse_gtfs_time`].
    ///
    /// `None` when `departure_time` is empty or not `HH:MM:SS`. The hour may be 24 or greater.
    pub departure_sec: Option<i64>,
    /// `pickup_type`. `None` when the cell is empty or not an integer.
    #[allow(dead_code)]
    pub pickup: Option<i64>,
    /// `drop_off_type`. `None` when the cell is empty or not an integer.
    #[allow(dead_code)]
    pub drop_off: Option<i64>,
    /// `stop_headsign` for this call, not the trip headsign.
    pub headsign: Option<String>,
    /// `service_id` of the trip. Pass it to [`StaticDb::service_active`].
    pub service_id: String,
    /// `trip_headsign` of the trip.
    pub trip_headsign: Option<String>,
    /// `trip_short_name` of the trip.
    pub trip_short_name: Option<String>,
    /// `direction_id` of the trip when the cell is an integer.
    #[allow(dead_code)]
    pub direction_id: Option<i64>,
    /// `route_id` of the trip.
    pub route_id: String,
    /// Short name of the trip's route. `None` when that route is missing.
    pub route_short_name: Option<String>,
    /// Long name of the trip's route. `None` when that route is missing.
    pub route_long_name: Option<String>,
    /// The trip's `route_type`, or `0` when that route is missing. See [`route_type_name`].
    pub route_type: i64,
}

/// Weekday service window from one `calendar.txt` row.
///
/// A weekday flag is `true` only when that column is the text `1`. Dates stay in
/// the `YYYYMMDD` form from the file and are compared as text.
#[derive(Debug, Clone)]
struct CalendarRow {
    /// `true` when `monday` is `1`.
    monday: bool,
    /// `true` when `tuesday` is `1`.
    tuesday: bool,
    /// `true` when `wednesday` is `1`.
    wednesday: bool,
    /// `true` when `thursday` is `1`.
    thursday: bool,
    /// `true` when `friday` is `1`.
    friday: bool,
    /// `true` when `saturday` is `1`.
    saturday: bool,
    /// `true` when `sunday` is `1`.
    sunday: bool,
    /// Inclusive first service day, as `YYYYMMDD` text.
    start_date: String,
    /// Inclusive last service day, as `YYYYMMDD` text.
    end_date: String,
}

/// GTFS static feed loaded into memory.
///
/// Build one with [`StaticDb::load_zip`]. Stops, routes, trips, and stop times
/// live in SQLite. Calendars live in the maps on this struct.
#[derive(Debug)]
pub struct StaticDb {
    /// In-memory SQLite database of stops, routes, trips, and stop times.
    conn: Connection,
    /// IANA timezone name from the chosen agency, or `"UTC"` when that agency sets none.
    pub tz_name: String,
    /// `tz_name` parsed as a `chrono_tz::Tz`.
    ///
    /// An unknown name becomes UTC. `tz_name` still holds the feed's text.
    pub tz: Tz,
    /// Name of the chosen agency. `None` when no agency name was loaded.
    pub agency_name: Option<String>,
    /// `feed_version` from the first `feed_info.txt` row.
    pub feed_version: Option<String>,
    /// `feed_publisher_name` from the first `feed_info.txt` row.
    pub publisher: Option<String>,
    /// Table and calendar counts for this load. See [`Counts`].
    pub counts: Counts,
    /// Weekday rows from `calendar.txt`, keyed by `service_id`.
    calendar: HashMap<String, CalendarRow>,
    /// Dates added for a `service_id` (`calendar_dates.txt` exception type 1).
    ///
    /// Each date is `YYYYMMDD`.
    added: HashMap<String, HashSet<String>>,
    /// Dates removed for a `service_id` (`calendar_dates.txt` exception type 2).
    ///
    /// Each date is `YYYYMMDD`.
    removed: HashMap<String, HashSet<String>>,
}

impl StaticDb {
    /// Loads a GTFS static zip into memory.
    ///
    /// `bytes` may be a zip, or gzip around a zip. The feed must produce at least one
    /// stop and one stop time. The agency used by the most routes supplies
    /// `agency_name` and the timezone. An unknown timezone name is kept on `tz_name`,
    /// and `tz` becomes UTC. `feed_info.txt`, when present, supplies version and
    /// publisher.
    ///
    /// # Arguments
    ///
    /// * `bytes` - GTFS zip bytes, or gzip of those bytes.
    ///
    /// # Returns
    ///
    /// A [`StaticDb`] ready for stop, route, trip, stop-time, and calendar queries.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when the archive cannot be opened, the stops, routes,
    /// trips, stop times, or calendar CSV cannot be read, SQLite fails, the feed
    /// has no stops, or the feed has no stop times. A missing or unreadable
    /// `agency.txt` or `feed_info.txt` is not an error.
    pub fn load_zip(bytes: &[u8]) -> Result<Self, AppError> {
        let files = read_zip(bytes)?;
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "
            PRAGMA foreign_keys=OFF;
            CREATE TABLE stops (
                stop_id TEXT PRIMARY KEY,
                stop_code TEXT,
                stop_name TEXT,
                name_fold TEXT,
                code_fold TEXT,
                lat REAL,
                lon REAL,
                location_type INTEGER NOT NULL DEFAULT 0,
                parent_station TEXT,
                platform_code TEXT
            );
            CREATE TABLE routes (
                route_id TEXT PRIMARY KEY,
                agency_id TEXT,
                short_name TEXT,
                long_name TEXT,
                name_fold TEXT,
                short_fold TEXT,
                route_type INTEGER NOT NULL DEFAULT 0,
                color TEXT
            );
            CREATE TABLE trips (
                trip_id TEXT PRIMARY KEY,
                route_id TEXT,
                service_id TEXT,
                headsign TEXT,
                short_name TEXT,
                short_fold TEXT,
                direction_id INTEGER
            );
            CREATE TABLE stop_times (
                trip_id TEXT NOT NULL,
                stop_sequence INTEGER NOT NULL,
                stop_id TEXT NOT NULL,
                arrival_sec INTEGER,
                departure_sec INTEGER,
                pickup INTEGER,
                drop_off INTEGER,
                headsign TEXT,
                PRIMARY KEY (trip_id, stop_sequence)
            );
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
            ",
        )?;

        let agencies = load_agencies(&files);
        let feed = load_feed_info(&files);
        load_stops(&conn, files.get("stops.txt"))?;
        load_routes(&conn, files.get("routes.txt"))?;
        load_trips(&conn, files.get("trips.txt"))?;
        load_stop_times(&conn, files.get("stop_times.txt"))?;
        let (calendar, added, removed) = load_services(&files)?;
        conn.execute_batch(
            "
            CREATE INDEX idx_stops_parent ON stops(parent_station);
            CREATE INDEX idx_stops_name ON stops(name_fold);
            CREATE INDEX idx_stops_code ON stops(code_fold);
            CREATE INDEX idx_trips_short ON trips(short_fold);
            CREATE INDEX idx_trips_route ON trips(route_id);
            CREATE INDEX idx_st_stop ON stop_times(stop_id);
            CREATE INDEX idx_st_trip ON stop_times(trip_id);
            ",
        )?;

        if conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM stops", [], |r| r.get(0))? == 0 {
            return Err(err("GTFS archive has no stops").hint("expected stops.txt inside the zip"));
        }
        if conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM stop_times", [], |r| r.get(0))? == 0 {
            return Err(
                err("GTFS archive has no stop_times").hint("departure boards need stop_times.txt")
            );
        }

        let agency = choose_agency(&conn, &agencies)?;
        let tz_name = agency.timezone.clone().unwrap_or_else(|| "UTC".to_string());
        let tz = tz_name.parse::<Tz>().unwrap_or(chrono_tz::UTC);
        let counts = Counts {
            agencies: agencies.len() as i64,
            stops: count(&conn, "stops")?,
            routes: count(&conn, "routes")?,
            trips: count(&conn, "trips")?,
            stop_times: count(&conn, "stop_times")?,
            calendar: calendar.len() as i64,
            calendar_dates: (added.values().map(|s| s.len()).sum::<usize>()
                + removed.values().map(|s| s.len()).sum::<usize>())
                as i64,
        };
        let _ = conn.execute(
            "INSERT INTO meta (key, value) VALUES ('timezone', ?1)",
            params![tz_name],
        );

        Ok(Self {
            conn,
            tz,
            tz_name,
            agency_name: agency.name,
            feed_version: feed.version,
            publisher: feed.publisher,
            counts,
            calendar,
            added,
            removed,
        })
    }

    /// Reports whether `service_id` operates on `date`.
    ///
    /// A `calendar_dates.txt` removal (exception type 2) wins over an addition
    /// (exception type 1). Otherwise the weekday row must include that day, and
    /// `date` must fall inside its inclusive `YYYYMMDD` range. A service with neither
    /// a calendar row nor an added date is inactive.
    ///
    /// # Arguments
    ///
    /// * `service_id` - GTFS service id from the trip or calendar.
    /// * `date` - Civil date to test. The caller chooses the date; this method does not convert time zones.
    ///
    /// # Returns
    ///
    /// `true` when that service runs on `date`.
    pub fn service_active(&self, service_id: &str, date: NaiveDate) -> bool {
        let key = date.format("%Y%m%d").to_string();
        if self
            .removed
            .get(service_id)
            .is_some_and(|days| days.contains(&key))
        {
            return false;
        }
        if self
            .added
            .get(service_id)
            .is_some_and(|days| days.contains(&key))
        {
            return true;
        }
        let Some(row) = self.calendar.get(service_id) else {
            return false;
        };
        if key < row.start_date || key > row.end_date {
            return false;
        }
        match date.weekday() {
            Weekday::Mon => row.monday,
            Weekday::Tue => row.tuesday,
            Weekday::Wed => row.wednesday,
            Weekday::Thu => row.thursday,
            Weekday::Fri => row.friday,
            Weekday::Sat => row.saturday,
            Weekday::Sun => row.sunday,
        }
    }

    /// Loads the stop with this exact `stop_id`.
    ///
    /// The match is case-sensitive.
    ///
    /// # Arguments
    ///
    /// * `stop_id` - Primary key from `stops.txt`.
    ///
    /// # Returns
    ///
    /// The [`StopRow`] when that id was loaded, or `None` when it was not.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when the SQLite query fails.
    pub fn stop_by_id(&self, stop_id: &str) -> Result<Option<StopRow>, AppError> {
        Ok(self
            .conn
            .query_row(
                "SELECT stop_id, stop_code, stop_name, lat, lon, location_type, parent_station, platform_code
                 FROM stops WHERE stop_id = ?1",
                params![stop_id],
                map_stop,
            )
            .optional()?)
    }

    /// Resolves a stop id, stop code, or stop name to one [`StopRow`].
    ///
    /// An exact `stop_id` wins and is case-sensitive. Otherwise the search matches a
    /// code or a name, ignoring case, and a name may match as a substring. `%`, `_`,
    /// and backslash in the query are escaped. When several rows match, an exact code
    /// beats an exact name, a stop that has stop times beats one that does not, and a
    /// plain stop (`location_type` 0 and no platform code) beats other kinds. A
    /// failure while checking for stop times counts as no stop times.
    ///
    /// # Arguments
    ///
    /// * `query` - `stop_id`, `stop_code`, or stop name. Leading and trailing whitespace is ignored.
    ///
    /// # Returns
    ///
    /// The single matching [`StopRow`].
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when `query` is empty, no stop matches, more than one
    /// stop remains after those tie-breaks, or SQLite fails. An ambiguous match
    /// includes up to eight `candidates`, each with `stop_id`, `stop_name`,
    /// `location_type`, and `platform_code`.
    pub fn resolve_stop(&self, query: &str) -> Result<StopRow, AppError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(err("stop is empty").hint("pass a stop_id, stop_code, or stop name"));
        }
        if let Some(stop) = self.stop_by_id(query)? {
            return Ok(stop);
        }
        let fold = query.to_lowercase();
        let mut stmt = self.conn.prepare(
            "SELECT stop_id, stop_code, stop_name, lat, lon, location_type, parent_station, platform_code
             FROM stops
             WHERE code_fold = ?1 OR name_fold = ?1 OR name_fold LIKE ?2 ESCAPE '\\'",
        )?;
        let like = like_contains(&fold);
        let rows = stmt
            .query_map(params![fold, like], map_stop)?
            .collect::<Result<Vec<_>, _>>()?;
        choose_stop(self, rows, query)
    }

    /// Searches stops by id, code, or name.
    ///
    /// A blank `query` matches every stop. Otherwise a row matches when `stop_id`
    /// equals the trimmed query (case-sensitive), the code equals it ignoring case,
    /// or the code or name contains it ignoring case. `%`, `_`, and backslash are
    /// escaped. Results are ordered by exact id, then exact code, then exact name,
    /// then other matches, and then by lowercased name.
    ///
    /// # Arguments
    ///
    /// * `query` - Text to match. Whitespace at the ends is ignored.
    /// * `limit` - Maximum rows to return.
    /// * `offset` - Rows to skip after that ordering.
    ///
    /// # Returns
    ///
    /// The page of [`StopRow`] values and the match count before paging.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when a SQLite query fails.
    pub fn search_stops(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<StopRow>, i64), AppError> {
        let fold = query.trim().to_lowercase();
        let (where_sql, args): (&str, Vec<String>) = if fold.is_empty() {
            ("1", vec![])
        } else {
            (
                "stop_id = ?1 OR code_fold = ?2 OR name_fold LIKE ?3 ESCAPE '\\' OR code_fold LIKE ?3 ESCAPE '\\'",
                vec![query.trim().to_string(), fold.clone(), like_contains(&fold)],
            )
        };
        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM stops WHERE {where_sql}"),
            params_from_iter(args.iter()),
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT stop_id, stop_code, stop_name, lat, lon, location_type, parent_station, platform_code
             FROM stops WHERE {where_sql}
             ORDER BY
               CASE WHEN stop_id = ?{id} THEN 0
                    WHEN code_fold = ?{fold} THEN 1
                    WHEN name_fold = ?{fold} THEN 2
                    ELSE 3 END,
               name_fold
             LIMIT ?{limit} OFFSET ?{offset}",
            id = args.len() + 1,
            fold = args.len() + 2,
            limit = args.len() + 3,
            offset = args.len() + 4,
        ))?;
        let mut bound = args.clone();
        bound.push(query.trim().to_string());
        bound.push(fold);
        let rows = stmt
            .query_map(
                params_from_iter(bound.iter().map(|s| s as &dyn rusqlite::ToSql).chain([
                    &(limit as i64) as &dyn rusqlite::ToSql,
                    &(offset as i64) as &dyn rusqlite::ToSql,
                ])),
                map_stop,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((rows, total))
    }

    /// Searches routes by id, short name, or the combined short and long name.
    ///
    /// A blank `query` matches every route. Otherwise a row matches when `route_id`
    /// equals the trimmed query (case-sensitive), the short name equals it ignoring
    /// case, or the short name or the combined name contains it ignoring case. The
    /// combined name is `short long` when both exist, otherwise whichever name is
    /// present. `%`, `_`, and backslash are escaped. Results are ordered by exact
    /// id, then exact short name, then other matches, and then by lowercased short
    /// name.
    ///
    /// # Arguments
    ///
    /// * `query` - Text to match. Whitespace at the ends is ignored.
    /// * `limit` - Maximum rows to return.
    /// * `offset` - Rows to skip after that ordering.
    ///
    /// # Returns
    ///
    /// The page of [`RouteRow`] values and the match count before paging.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when a SQLite query fails.
    pub fn search_routes(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<RouteRow>, i64), AppError> {
        let fold = query.trim().to_lowercase();
        let (where_sql, args): (&str, Vec<String>) = if fold.is_empty() {
            ("1", vec![])
        } else {
            (
                "route_id = ?1 OR short_fold = ?2 OR name_fold LIKE ?3 ESCAPE '\\' OR short_fold LIKE ?3 ESCAPE '\\'",
                vec![query.trim().to_string(), fold.clone(), like_contains(&fold)],
            )
        };
        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM routes WHERE {where_sql}"),
            params_from_iter(args.iter()),
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT route_id, short_name, long_name, route_type, color
             FROM routes WHERE {where_sql}
             ORDER BY CASE WHEN route_id = ?{id} THEN 0 WHEN short_fold = ?{fold} THEN 1 ELSE 2 END, short_fold
             LIMIT ?{limit} OFFSET ?{offset}",
            id = args.len() + 1,
            fold = args.len() + 2,
            limit = args.len() + 3,
            offset = args.len() + 4,
        ))?;
        let mut bound = args.clone();
        bound.push(query.trim().to_string());
        bound.push(fold);
        let rows = stmt
            .query_map(
                params_from_iter(bound.iter().map(|s| s as &dyn rusqlite::ToSql).chain([
                    &(limit as i64) as &dyn rusqlite::ToSql,
                    &(offset as i64) as &dyn rusqlite::ToSql,
                ])),
                |r| {
                    Ok(RouteRow {
                        route_id: r.get(0)?,
                        short_name: r.get(1)?,
                        long_name: r.get(2)?,
                        route_type: r.get(3)?,
                        color: r.get(4)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((rows, total))
    }

    /// Resolves a route query to route ids.
    ///
    /// An exact `route_id` returns that one id and is case-sensitive. Otherwise every
    /// route whose short name or combined name equals the query, ignoring case, is
    /// returned. The combined name is `short long` when both exist. If none match,
    /// up to eight routes whose short name or combined name contains the query are
    /// returned. `%`, `_`, and backslash are escaped in that substring search. A
    /// blank query does not match a route id. Routes with an empty short or
    /// combined name are returned when any exist; otherwise up to eight routes
    /// are returned.
    ///
    /// # Arguments
    ///
    /// * `query` - `route_id`, short name, or combined name. Whitespace at the ends is ignored.
    ///
    /// # Returns
    ///
    /// Matching `route_id` values, or an empty list when nothing matches.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when a SQLite query fails.
    pub fn resolve_route_ids(&self, query: &str) -> Result<Vec<String>, AppError> {
        let query = query.trim();
        let fold = query.to_lowercase();
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT route_id FROM routes WHERE route_id = ?1",
                params![query],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(vec![id]);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT route_id FROM routes WHERE short_fold = ?1 OR name_fold = ?1")?;
        let exact = stmt
            .query_map(params![fold], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !exact.is_empty() {
            return Ok(exact);
        }
        let mut stmt = self.conn.prepare(
            "SELECT route_id FROM routes WHERE short_fold LIKE ?1 ESCAPE '\\' OR name_fold LIKE ?1 ESCAPE '\\' LIMIT 8",
        )?;
        let fuzzy = stmt
            .query_map(params![like_contains(&fold)], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(fuzzy)
    }

    /// Loads one route by exact `route_id`.
    ///
    /// The match is case-sensitive.
    ///
    /// # Arguments
    ///
    /// * `route_id` - Primary key from `routes.txt`.
    ///
    /// # Returns
    ///
    /// The [`RouteRow`], or `None` when the id is absent.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when the SQLite query fails.
    pub fn route(&self, route_id: &str) -> Result<Option<RouteRow>, AppError> {
        Ok(self
            .conn
            .query_row(
                "SELECT route_id, short_name, long_name, route_type, color FROM routes WHERE route_id = ?1",
                params![route_id],
                |r| {
                    Ok(RouteRow {
                        route_id: r.get(0)?,
                        short_name: r.get(1)?,
                        long_name: r.get(2)?,
                        route_type: r.get(3)?,
                        color: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// Loads one trip by exact `trip_id`, with its route joined on.
    ///
    /// The match is case-sensitive. A missing route leaves the route names empty and
    /// `route_type` at `0`.
    ///
    /// # Arguments
    ///
    /// * `trip_id` - Primary key from `trips.txt`.
    ///
    /// # Returns
    ///
    /// The [`TripRow`], or `None` when the id is absent.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when the SQLite query fails.
    pub fn trip_by_id(&self, trip_id: &str) -> Result<Option<TripRow>, AppError> {
        Ok(self
            .conn
            .query_row(
                "SELECT t.trip_id, t.route_id, t.service_id, t.headsign, t.short_name, t.direction_id,
                        r.short_name, r.long_name, r.route_type
                 FROM trips t LEFT JOIN routes r ON r.route_id = t.route_id
                 WHERE t.trip_id = ?1",
                params![trip_id],
                map_trip,
            )
            .optional()?)
    }

    /// Loads every trip whose `trip_short_name` equals `short_name`, ignoring case.
    ///
    /// Rows are ordered by `trip_id`. Route names are joined the same way as
    /// [`StaticDb::trip_by_id`].
    ///
    /// # Arguments
    ///
    /// * `short_name` - Trip short name to match. It is lowercased before the lookup.
    ///
    /// # Returns
    ///
    /// Matching [`TripRow`] values, or an empty list when none match.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when the SQLite query fails.
    pub fn trips_by_short_name(&self, short_name: &str) -> Result<Vec<TripRow>, AppError> {
        let mut stmt = self.conn.prepare(
            "SELECT t.trip_id, t.route_id, t.service_id, t.headsign, t.short_name, t.direction_id,
                    r.short_name, r.long_name, r.route_type
             FROM trips t LEFT JOIN routes r ON r.route_id = t.route_id
             WHERE t.short_fold = ?1
             ORDER BY t.trip_id",
        )?;
        let rows = stmt
            .query_map(params![short_name.to_lowercase()], map_trip)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Returns the `route_id` stored on a trip.
    ///
    /// # Arguments
    ///
    /// * `trip_id` - Primary key from `trips.txt`. The match is case-sensitive.
    ///
    /// # Returns
    ///
    /// The trip's `route_id`, which may be empty, or `None` when the trip is absent.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when the SQLite query fails.
    pub fn trip_route_id(&self, trip_id: &str) -> Result<Option<String>, AppError> {
        Ok(self
            .conn
            .query_row(
                "SELECT route_id FROM trips WHERE trip_id = ?1",
                params![trip_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Expands a stop to the ids used for boards and alerts.
    ///
    /// The result always includes `stop_id`, and its `parent_station` when it has
    /// one, even when that id was not loaded. It then adds every stop, station, or
    /// boarding area (`location_type` 0, 1, or 4) whose id or parent is in that set.
    /// Entrances and generic nodes are not added unless they were the original id or
    /// its parent.
    ///
    /// # Arguments
    ///
    /// * `stop_id` - Stop to expand. It does not have to exist in `stops.txt`.
    ///
    /// # Returns
    ///
    /// Unique ids, sorted by their text.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when a SQLite query fails.
    pub fn expand_stop_ids(&self, stop_id: &str) -> Result<Vec<String>, AppError> {
        let mut ids = HashSet::new();
        ids.insert(stop_id.to_string());
        if let Some(stop) = self.stop_by_id(stop_id)? {
            if let Some(parent) = stop.parent_station {
                ids.insert(parent);
            }
        }
        let seeds: Vec<String> = ids.iter().cloned().collect();
        let placeholders = vec!["?"; seeds.len()].join(",");
        // Stations in a full GTFS graph also parent entrances and pathway nodes.
        // Boards and alerts only need the station, its platforms, and boarding areas.
        let mut stmt = self.conn.prepare(&format!(
            "SELECT stop_id FROM stops
             WHERE location_type IN (0, 1, 4)
               AND (stop_id IN ({placeholders}) OR parent_station IN ({placeholders}))"
        ))?;
        let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::new();
        for id in seeds.iter().chain(seeds.iter()) {
            bound.push(id);
        }
        let extras = stmt
            .query_map(params_from_iter(bound.iter().copied()), |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for id in extras {
            ids.insert(id);
        }
        let mut out: Vec<_> = ids.into_iter().collect();
        out.sort();
        Ok(out)
    }

    /// Loads stops for the given ids.
    ///
    /// Unknown ids are left out. Duplicate ids become one map entry.
    ///
    /// # Arguments
    ///
    /// * `ids` - `stop_id` values to load.
    ///
    /// # Returns
    ///
    /// A map from `stop_id` to [`StopRow`]. An empty `ids` slice yields an empty map.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when the SQLite query fails.
    pub fn stops_for_ids(&self, ids: &[String]) -> Result<HashMap<String, StopRow>, AppError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT stop_id, stop_code, stop_name, lat, lon, location_type, parent_station, platform_code
             FROM stops WHERE stop_id IN ({placeholders})"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(ids.iter()), map_stop)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(|s| (s.stop_id.clone(), s)).collect())
    }

    /// Loads every stop time scheduled at the given stops.
    ///
    /// The trip must exist or the row is dropped. Stop and route columns are left
    /// joined. Service is not filtered here; call [`StaticDb::service_active`] for
    /// that. Row order is whatever SQLite returns.
    ///
    /// # Arguments
    ///
    /// * `stop_ids` - Stops whose `stop_times.txt` rows to load.
    ///
    /// # Returns
    ///
    /// Joined [`StopTimeRow`] values. An empty `stop_ids` slice yields an empty list.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when the SQLite query fails.
    pub fn stop_times_for_stops(&self, stop_ids: &[String]) -> Result<Vec<StopTimeRow>, AppError> {
        if stop_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; stop_ids.len()].join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT st.trip_id, st.stop_sequence, st.stop_id, s.stop_name, s.platform_code,
                    st.arrival_sec, st.departure_sec, st.pickup, st.drop_off, st.headsign,
                    t.service_id, t.headsign, t.short_name, t.direction_id, t.route_id,
                    r.short_name, r.long_name, r.route_type
             FROM stop_times st
             JOIN trips t ON t.trip_id = st.trip_id
             LEFT JOIN stops s ON s.stop_id = st.stop_id
             LEFT JOIN routes r ON r.route_id = t.route_id
             WHERE st.stop_id IN ({placeholders})"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(stop_ids.iter()), map_stop_time)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Counts stop times stored for one trip.
    ///
    /// # Arguments
    ///
    /// * `trip_id` - Trip to count. The match is case-sensitive.
    ///
    /// # Returns
    ///
    /// The number of `stop_times.txt` rows for that trip, or `0` when it has none.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when the SQLite query fails.
    pub fn trip_stop_count(&self, trip_id: &str) -> Result<i64, AppError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM stop_times WHERE trip_id = ?1",
            params![trip_id],
            |r| r.get(0),
        )?)
    }

    /// Loads one page of stop times for a trip, in `stop_sequence` order.
    ///
    /// # Arguments
    ///
    /// * `trip_id` - Trip to read. The match is case-sensitive.
    /// * `limit` - Maximum rows to return.
    /// * `offset` - Rows to skip after ordering by `stop_sequence`.
    ///
    /// # Returns
    ///
    /// Joined [`StopTimeRow`] values. A trip with no stop times yields an empty list.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when the SQLite query fails.
    pub fn trip_stop_times(
        &self,
        trip_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<StopTimeRow>, AppError> {
        let mut stmt = self.conn.prepare(
            "SELECT st.trip_id, st.stop_sequence, st.stop_id, s.stop_name, s.platform_code,
                    st.arrival_sec, st.departure_sec, st.pickup, st.drop_off, st.headsign,
                    t.service_id, t.headsign, t.short_name, t.direction_id, t.route_id,
                    r.short_name, r.long_name, r.route_type
             FROM stop_times st
             JOIN trips t ON t.trip_id = st.trip_id
             LEFT JOIN stops s ON s.stop_id = st.stop_id
             LEFT JOIN routes r ON r.route_id = t.route_id
             WHERE st.trip_id = ?1
             ORDER BY st.stop_sequence
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt
            .query_map(params![trip_id, limit as i64, offset as i64], map_stop_time)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Reports whether any stop time is scheduled at `stop_id`.
    ///
    /// # Arguments
    ///
    /// * `stop_id` - Stop to test. The match is case-sensitive.
    ///
    /// # Returns
    ///
    /// `true` when at least one `stop_times.txt` row uses that id.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] when the SQLite query fails.
    pub fn stop_has_times(&self, stop_id: &str) -> Result<bool, AppError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM stop_times WHERE stop_id = ?1 LIMIT 1",
                params![stop_id],
                |_| Ok(1i64),
            )
            .optional()?
            .is_some())
    }
}

/// Picks one stop from search hits, or reports that the query is ambiguous.
///
/// An empty list is an error. One hit is returned as-is. Otherwise an exact
/// `stop_code` (ASCII case-insensitive) narrows the list, then an exact
/// `stop_name` (Unicode lowercase). Hits that have stop times are preferred next.
/// A SQLite error from that check counts as no stop times. A single plain stop
/// (`location_type` 0 and no platform code) wins over a mixed remainder.
///
/// # Arguments
///
/// * `db` - Feed used to see which candidates have stop times.
/// * `rows` - Candidates already matched by id, code, or name.
/// * `query` - Caller's original stop text, used in messages and exact comparisons.
///
/// # Returns
///
/// The chosen [`StopRow`].
///
/// # Errors
///
/// Returns [`AppError`] when `rows` is empty or more than one candidate remains.
/// The ambiguous error includes up to eight `candidates`.
///
/// # Panics
///
/// Panics if a candidate list reports one element and then yields none. Each
/// `unwrap` follows a `len() == 1` check on that same `Vec`.
fn choose_stop(db: &StaticDb, rows: Vec<StopRow>, query: &str) -> Result<StopRow, AppError> {
    if rows.is_empty() {
        return Err(
            err(format!("no stop matches '{query}'")).hint("call search_stops to see identifiers")
        );
    }
    if rows.len() == 1 {
        return Ok(rows.into_iter().next().unwrap());
    }
    let fold = query.to_lowercase();
    let code_exact: Vec<_> = rows
        .iter()
        .filter(|s| {
            s.stop_code
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(query))
        })
        .cloned()
        .collect();
    let pool = if code_exact.is_empty() {
        let name_exact: Vec<_> = rows
            .iter()
            .filter(|s| {
                s.stop_name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase() == fold)
            })
            .cloned()
            .collect();
        if name_exact.is_empty() {
            rows
        } else {
            name_exact
        }
    } else {
        code_exact
    };
    if pool.len() == 1 {
        return Ok(pool.into_iter().next().unwrap());
    }
    let with_times: Vec<_> = pool
        .iter()
        .filter(|s| db.stop_has_times(&s.stop_id).unwrap_or(false))
        .cloned()
        .collect();
    if with_times.len() == 1 {
        return Ok(with_times.into_iter().next().unwrap());
    }
    let narrowed = if with_times.is_empty() {
        pool
    } else {
        with_times
    };
    let plain: Vec<_> = narrowed
        .iter()
        .filter(|s| s.location_type == 0 && s.platform_code.is_none())
        .cloned()
        .collect();
    if plain.len() == 1 {
        return Ok(plain.into_iter().next().unwrap());
    }
    let shown: Vec<_> = narrowed
        .iter()
        .take(8)
        .map(|s| {
            serde_json::json!({
                "stop_id": s.stop_id,
                "stop_name": s.stop_name,
                "location_type": s.location_type,
                "platform_code": s.platform_code,
            })
        })
        .collect();
    Err(
        err(format!("stop '{query}' matches {} places", narrowed.len()))
            .hint("pass an exact stop_id")
            .extra("candidates", serde_json::Value::Array(shown)),
    )
}

/// Maps one `stops` row onto a [`StopRow`].
///
/// Column order is `stop_id`, `stop_code`, `stop_name`, `lat`, `lon`,
/// `location_type`, `parent_station`, `platform_code`.
///
/// # Arguments
///
/// * `r` - Current SQLite row.
///
/// # Returns
///
/// The mapped [`StopRow`].
///
/// # Errors
///
/// Returns `rusqlite::Error` when a column is missing or has the wrong type.
fn map_stop(r: &rusqlite::Row<'_>) -> rusqlite::Result<StopRow> {
    Ok(StopRow {
        stop_id: r.get(0)?,
        stop_code: r.get(1)?,
        stop_name: r.get(2)?,
        lat: r.get(3)?,
        lon: r.get(4)?,
        location_type: r.get(5)?,
        parent_station: r.get(6)?,
        platform_code: r.get(7)?,
    })
}

/// Maps one joined trip row onto a [`TripRow`].
///
/// A null joined `route_type` becomes `0`.
///
/// # Arguments
///
/// * `r` - Current SQLite row from the trip and route join.
///
/// # Returns
///
/// The mapped [`TripRow`].
///
/// # Errors
///
/// Returns `rusqlite::Error` when a required column is missing or has the wrong type.
fn map_trip(r: &rusqlite::Row<'_>) -> rusqlite::Result<TripRow> {
    Ok(TripRow {
        trip_id: r.get(0)?,
        route_id: r.get(1)?,
        service_id: r.get(2)?,
        headsign: r.get(3)?,
        short_name: r.get(4)?,
        direction_id: r.get(5)?,
        route_short_name: r.get(6)?,
        route_long_name: r.get(7)?,
        route_type: r.get::<_, Option<i64>>(8)?.unwrap_or(0),
    })
}

/// Maps one joined stop-time row onto a [`StopTimeRow`].
///
/// A null joined `route_type` becomes `0`.
///
/// # Arguments
///
/// * `r` - Current SQLite row from the stop-time, trip, stop, and route join.
///
/// # Returns
///
/// The mapped [`StopTimeRow`].
///
/// # Errors
///
/// Returns `rusqlite::Error` when a required column is missing or has the wrong type.
fn map_stop_time(r: &rusqlite::Row<'_>) -> rusqlite::Result<StopTimeRow> {
    Ok(StopTimeRow {
        trip_id: r.get(0)?,
        stop_sequence: r.get(1)?,
        stop_id: r.get(2)?,
        stop_name: r.get(3)?,
        platform_code: r.get(4)?,
        arrival_sec: r.get(5)?,
        departure_sec: r.get(6)?,
        pickup: r.get(7)?,
        drop_off: r.get(8)?,
        headsign: r.get(9)?,
        service_id: r.get(10)?,
        trip_headsign: r.get(11)?,
        trip_short_name: r.get(12)?,
        direction_id: r.get(13)?,
        route_id: r.get(14)?,
        route_short_name: r.get(15)?,
        route_long_name: r.get(16)?,
        route_type: r.get::<_, Option<i64>>(17)?.unwrap_or(0),
    })
}

/// Counts rows in one table of the loaded feed.
///
/// # Arguments
///
/// * `conn` - In-memory feed database.
/// * `table` - Table name created by [`StaticDb::load_zip`], such as `stops`. It is interpolated into SQL.
///
/// # Returns
///
/// `COUNT(*)` for that table.
///
/// # Errors
///
/// Returns [`AppError`] when SQLite rejects the query.
fn count(conn: &Connection, table: &str) -> Result<i64, AppError> {
    Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
}

/// Builds a `LIKE` pattern that matches `fold` as a substring.
///
/// Backslash, `%`, and `_` are escaped so they match themselves. The caller must
/// use a backslash `ESCAPE` clause.
///
/// # Arguments
///
/// * `fold` - Already-lowercased search text.
///
/// # Returns
///
/// `fold` wrapped in `%` after those escapes. An empty `fold` returns `%%`.
fn like_contains(fold: &str) -> String {
    let escaped = fold
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// One usable row from `agency.txt`.
///
/// Rows with neither a name nor a timezone are dropped while loading.
#[derive(Clone, Default)]
struct Agency {
    /// `agency_id`. `None` when the cell is empty.
    id: Option<String>,
    /// `agency_name`. `None` when the cell is empty.
    name: Option<String>,
    /// `agency_timezone`. `None` when the cell is empty.
    timezone: Option<String>,
}

/// Publisher and version from the first data row of `feed_info.txt`.
struct FeedInfo {
    /// `feed_version`. `None` when the file, header, or cell is missing.
    version: Option<String>,
    /// `feed_publisher_name`. `None` when the file, header, or cell is missing.
    publisher: Option<String>,
}

/// Parses `agency.txt` into [`Agency`] values.
///
/// A missing file, an unreadable header, or a file with no usable row yields an
/// empty list. A record that fails to parse is skipped. A row is kept when it has
/// a name or a timezone.
///
/// # Arguments
///
/// * `files` - Canonical GTFS file name to file bytes.
///
/// # Returns
///
/// Agencies in file order.
fn load_agencies(files: &HashMap<String, Vec<u8>>) -> Vec<Agency> {
    let Some(bytes) = files.get("agency.txt") else {
        return Vec::new();
    };
    let mut reader = csv_reader(bytes);
    let Ok(headers) = reader.headers() else {
        return Vec::new();
    };
    let headers = headers.clone();
    let idx = header_index(&headers);
    reader
        .records()
        .flatten()
        .map(|record| Agency {
            id: nonempty(col(&record, &idx, "agency_id")),
            name: nonempty(col(&record, &idx, "agency_name")),
            timezone: nonempty(col(&record, &idx, "agency_timezone")),
        })
        .filter(|agency| agency.name.is_some() || agency.timezone.is_some())
        .collect()
}

/// Picks the agency whose name and timezone represent the feed.
///
/// The `agency_id` with the most routes wins. Ties break by `agency_id`
/// ascending. Routes with a blank `agency_id` are ignored. When that id is not in
/// `agencies`, or no route has an agency id, the first parsed [`Agency`] is used.
/// No agencies yields an empty default agency.
///
/// # Arguments
///
/// * `conn` - Database whose `routes` table supplies the dominant `agency_id`.
/// * `agencies` - Agencies parsed from `agency.txt`.
///
/// # Returns
///
/// The chosen [`Agency`].
///
/// # Errors
///
/// Returns [`AppError`] when the route tally query fails.
fn choose_agency(conn: &Connection, agencies: &[Agency]) -> Result<Agency, AppError> {
    if agencies.is_empty() {
        return Ok(Agency::default());
    }
    let dominant: Option<String> = conn
        .query_row(
            "SELECT agency_id FROM routes
             WHERE agency_id IS NOT NULL AND agency_id != ''
             GROUP BY agency_id
             ORDER BY COUNT(*) DESC, agency_id
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = dominant {
        if let Some(agency) = agencies
            .iter()
            .find(|agency| agency.id.as_deref() == Some(id.as_str()))
        {
            return Ok(agency.clone());
        }
    }
    Ok(agencies[0].clone())
}

/// Reads version and publisher from the first `feed_info.txt` row.
///
/// A missing file or an unreadable header yields empty fields. A later row is
/// ignored. A first record that fails to parse leaves the fields empty.
///
/// # Arguments
///
/// * `files` - Canonical GTFS file name to file bytes.
///
/// # Returns
///
/// The parsed [`FeedInfo`].
fn load_feed_info(files: &HashMap<String, Vec<u8>>) -> FeedInfo {
    let Some(bytes) = files.get("feed_info.txt") else {
        return FeedInfo {
            version: None,
            publisher: None,
        };
    };
    let mut reader = csv_reader(bytes);
    let Ok(headers) = reader.headers() else {
        return FeedInfo {
            version: None,
            publisher: None,
        };
    };
    let idx = header_index(headers);
    let mut info = FeedInfo {
        version: None,
        publisher: None,
    };
    if let Some(Ok(record)) = reader.records().next() {
        info.version = nonempty(col(&record, &idx, "feed_version"));
        info.publisher = nonempty(col(&record, &idx, "feed_publisher_name"));
    }
    info
}

/// Inserts `stops.txt` into `stops`.
///
/// An absent file stores nothing. Rows with an empty `stop_id` are skipped.
/// `location_type` defaults to `0`. Names and codes are also stored lowercased
/// for search. Duplicate ids keep the later row.
///
/// # Arguments
///
/// * `conn` - Feed database.
/// * `bytes` - `stops.txt` bytes, or `None` when the archive has no such file.
///
/// # Returns
///
/// `Ok(())` when the file is absent or its rows have been stored.
///
/// # Errors
///
/// Returns [`AppError`] when the CSV cannot be read or a SQLite insert fails.
fn load_stops(conn: &Connection, bytes: Option<&Vec<u8>>) -> Result<(), AppError> {
    let Some(bytes) = bytes else {
        return Ok(());
    };
    let mut reader = csv_reader(bytes);
    let headers = reader.headers()?.clone();
    let idx = header_index(&headers);
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO stops
             (stop_id, stop_code, stop_name, name_fold, code_fold, lat, lon, location_type, parent_station, platform_code)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        for record in reader.records() {
            let record = record?;
            let stop_id = col(&record, &idx, "stop_id");
            if stop_id.is_empty() {
                continue;
            }
            let name = nonempty(col(&record, &idx, "stop_name"));
            let code = nonempty(col(&record, &idx, "stop_code"));
            stmt.execute(params![
                stop_id,
                code.clone(),
                name.clone(),
                name.as_deref().map(|s| s.to_lowercase()),
                code.as_deref().map(|s| s.to_lowercase()),
                parse_f64(col(&record, &idx, "stop_lat")),
                parse_f64(col(&record, &idx, "stop_lon")),
                parse_i64(col(&record, &idx, "location_type")).unwrap_or(0),
                nonempty(col(&record, &idx, "parent_station")),
                nonempty(col(&record, &idx, "platform_code")),
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Inserts `routes.txt` into `routes`.
///
/// An absent file stores nothing. Rows with an empty `route_id` are skipped.
/// `route_type` defaults to `0`. The search name is the lowercased short name,
/// long name, or `short long` when both exist. Duplicate ids keep the later row.
///
/// # Arguments
///
/// * `conn` - Feed database.
/// * `bytes` - `routes.txt` bytes, or `None` when the archive has no such file.
///
/// # Returns
///
/// `Ok(())` when the file is absent or its rows have been stored.
///
/// # Errors
///
/// Returns [`AppError`] when the CSV cannot be read or a SQLite insert fails.
fn load_routes(conn: &Connection, bytes: Option<&Vec<u8>>) -> Result<(), AppError> {
    let Some(bytes) = bytes else {
        return Ok(());
    };
    let mut reader = csv_reader(bytes);
    let headers = reader.headers()?.clone();
    let idx = header_index(&headers);
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO routes
             (route_id, agency_id, short_name, long_name, name_fold, short_fold, route_type, color)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for record in reader.records() {
            let record = record?;
            let route_id = col(&record, &idx, "route_id");
            if route_id.is_empty() {
                continue;
            }
            let short = nonempty(col(&record, &idx, "route_short_name"));
            let long = nonempty(col(&record, &idx, "route_long_name"));
            let fold = match (&short, &long) {
                (Some(s), Some(l)) => Some(format!("{s} {l}").to_lowercase()),
                (Some(s), None) => Some(s.to_lowercase()),
                (None, Some(l)) => Some(l.to_lowercase()),
                _ => None,
            };
            stmt.execute(params![
                route_id,
                nonempty(col(&record, &idx, "agency_id")),
                short.clone(),
                long,
                fold,
                short.as_deref().map(|s| s.to_lowercase()),
                parse_i64(col(&record, &idx, "route_type")).unwrap_or(0),
                nonempty(col(&record, &idx, "route_color")),
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Inserts `trips.txt` into `trips`.
///
/// An absent file stores nothing. Rows with an empty `trip_id` are skipped. The
/// short name is also stored lowercased. `direction_id` is kept only when it
/// parses as an integer. Duplicate ids keep the later row.
///
/// # Arguments
///
/// * `conn` - Feed database.
/// * `bytes` - `trips.txt` bytes, or `None` when the archive has no such file.
///
/// # Returns
///
/// `Ok(())` when the file is absent or its rows have been stored.
///
/// # Errors
///
/// Returns [`AppError`] when the CSV cannot be read or a SQLite insert fails.
fn load_trips(conn: &Connection, bytes: Option<&Vec<u8>>) -> Result<(), AppError> {
    let Some(bytes) = bytes else {
        return Ok(());
    };
    let mut reader = csv_reader(bytes);
    let headers = reader.headers()?.clone();
    let idx = header_index(&headers);
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO trips
             (trip_id, route_id, service_id, headsign, short_name, short_fold, direction_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for record in reader.records() {
            let record = record?;
            let trip_id = col(&record, &idx, "trip_id");
            if trip_id.is_empty() {
                continue;
            }
            let short = nonempty(col(&record, &idx, "trip_short_name"));
            stmt.execute(params![
                trip_id,
                col(&record, &idx, "route_id"),
                col(&record, &idx, "service_id"),
                nonempty(col(&record, &idx, "trip_headsign")),
                short.clone(),
                short.as_deref().map(|s| s.to_lowercase()),
                parse_i64(col(&record, &idx, "direction_id")),
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Inserts `stop_times.txt` into `stop_times`.
///
/// An absent file stores nothing. A row needs a `trip_id`, a `stop_id`, and an
/// integer `stop_sequence`. Arrival and departure pass through [`parse_gtfs_time`].
/// Duplicate `(trip_id, stop_sequence)` pairs keep the later row.
///
/// # Arguments
///
/// * `conn` - Feed database.
/// * `bytes` - `stop_times.txt` bytes, or `None` when the archive has no such file.
///
/// # Returns
///
/// `Ok(())` when the file is absent or its rows have been stored.
///
/// # Errors
///
/// Returns [`AppError`] when the CSV cannot be read or a SQLite insert fails.
fn load_stop_times(conn: &Connection, bytes: Option<&Vec<u8>>) -> Result<(), AppError> {
    let Some(bytes) = bytes else {
        return Ok(());
    };
    let mut reader = csv_reader(bytes);
    let headers = reader.headers()?.clone();
    let idx = header_index(&headers);
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO stop_times
             (trip_id, stop_sequence, stop_id, arrival_sec, departure_sec, pickup, drop_off, headsign)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for record in reader.records() {
            let record = record?;
            let trip_id = col(&record, &idx, "trip_id");
            let stop_id = col(&record, &idx, "stop_id");
            let Some(seq) = parse_i64(col(&record, &idx, "stop_sequence")) else {
                continue;
            };
            if trip_id.is_empty() || stop_id.is_empty() {
                continue;
            }
            stmt.execute(params![
                trip_id,
                seq,
                stop_id,
                parse_gtfs_time(col(&record, &idx, "arrival_time")),
                parse_gtfs_time(col(&record, &idx, "departure_time")),
                parse_i64(col(&record, &idx, "pickup_type")),
                parse_i64(col(&record, &idx, "drop_off_type")),
                nonempty(col(&record, &idx, "stop_headsign")),
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Calendar rows plus the dates added to or removed from each `service_id`.
///
/// The tuple is `(calendar, added, removed)`. Dates are `YYYYMMDD` strings.
/// `added` holds exception type 1 and `removed` holds exception type 2.
type ServiceMaps = (
    HashMap<String, CalendarRow>,
    HashMap<String, HashSet<String>>,
    HashMap<String, HashSet<String>>,
);

/// Parses `calendar.txt` and `calendar_dates.txt`.
///
/// Empty `service_id` rows are skipped. Exception rows also need a `date`.
/// Exception type `1` adds that date, type `2` removes it, and any other value is
/// ignored. A missing file contributes an empty map.
///
/// # Arguments
///
/// * `files` - Canonical GTFS file name to file bytes.
///
/// # Returns
///
/// The [`ServiceMaps`] for [`StaticDb::service_active`].
///
/// # Errors
///
/// Returns [`AppError`] when either CSV header or record cannot be read.
fn load_services(files: &HashMap<String, Vec<u8>>) -> Result<ServiceMaps, AppError> {
    let mut calendar = HashMap::new();
    if let Some(bytes) = files.get("calendar.txt") {
        let mut reader = csv_reader(bytes);
        let headers = reader.headers()?.clone();
        let idx = header_index(&headers);
        for record in reader.records() {
            let record = record?;
            let service_id = col(&record, &idx, "service_id");
            if service_id.is_empty() {
                continue;
            }
            calendar.insert(
                service_id.to_string(),
                CalendarRow {
                    monday: flag(col(&record, &idx, "monday")),
                    tuesday: flag(col(&record, &idx, "tuesday")),
                    wednesday: flag(col(&record, &idx, "wednesday")),
                    thursday: flag(col(&record, &idx, "thursday")),
                    friday: flag(col(&record, &idx, "friday")),
                    saturday: flag(col(&record, &idx, "saturday")),
                    sunday: flag(col(&record, &idx, "sunday")),
                    start_date: col(&record, &idx, "start_date").to_string(),
                    end_date: col(&record, &idx, "end_date").to_string(),
                },
            );
        }
    }
    let mut added: HashMap<String, HashSet<String>> = HashMap::new();
    let mut removed: HashMap<String, HashSet<String>> = HashMap::new();
    if let Some(bytes) = files.get("calendar_dates.txt") {
        let mut reader = csv_reader(bytes);
        let headers = reader.headers()?.clone();
        let idx = header_index(&headers);
        for record in reader.records() {
            let record = record?;
            let service_id = col(&record, &idx, "service_id");
            let date = col(&record, &idx, "date");
            if service_id.is_empty() || date.is_empty() {
                continue;
            }
            match col(&record, &idx, "exception_type") {
                "1" => {
                    added
                        .entry(service_id.to_string())
                        .or_default()
                        .insert(date.to_string());
                }
                "2" => {
                    removed
                        .entry(service_id.to_string())
                        .or_default()
                        .insert(date.to_string());
                }
                _ => {}
            }
        }
    }
    Ok((calendar, added, removed))
}

/// Reads the GTFS text files this module keeps from a zip, or from gzip around a zip.
///
/// Bytes starting with gzip magic `1F 8B` are inflated and read again, so a gzip
/// of a gzip of a zip still works. Entry paths are reduced to their file name by
/// [`canonical_gtfs_name`]. Paths containing `..` are skipped. When the same
/// canonical name appears twice, the shallower path wins; equal depth keeps the
/// earlier zip entry.
///
/// # Arguments
///
/// * `bytes` - Zip bytes, or gzip whose payload is zip bytes.
///
/// # Returns
///
/// Canonical file name, such as `stops.txt`, to that file's bytes.
///
/// # Errors
///
/// Returns [`AppError`] when gzip inflation fails, the bytes are not a zip, or a
/// zip entry cannot be read.
fn read_zip(bytes: &[u8]) -> Result<HashMap<String, Vec<u8>>, AppError> {
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut inflated = Vec::new();
        decoder.read_to_end(&mut inflated)?;
        return read_zip(&inflated);
    }
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut best: HashMap<String, (usize, Vec<u8>)> = HashMap::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        if !file.is_file() {
            continue;
        }
        let name = file.name().to_string();
        if name.contains("..") {
            continue;
        }
        let Some(key) = canonical_gtfs_name(&name) else {
            continue;
        };
        let depth = name.matches(['/', '\\']).count();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        match best.get(key) {
            Some((existing, _)) if *existing <= depth => {}
            _ => {
                best.insert(key.to_string(), (depth, buf));
            }
        }
    }
    Ok(best.into_iter().map(|(k, (_, v))| (k, v)).collect())
}

/// Maps a zip entry path to a GTFS file name this module loads.
///
/// The match uses the entry's base name, ignoring case. Recognized names are
/// `agency.txt`, `stops.txt`, `routes.txt`, `trips.txt`, `stop_times.txt`,
/// `calendar.txt`, `calendar_dates.txt`, and `feed_info.txt`.
///
/// # Arguments
///
/// * `path` - Zip entry path. Separators may be `/` or a backslash.
///
/// # Returns
///
/// The canonical lowercase name, or `None` when the base name is not one of those files.
fn canonical_gtfs_name(path: &str) -> Option<&'static str> {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match base.to_ascii_lowercase().as_str() {
        "agency.txt" => Some("agency.txt"),
        "stops.txt" => Some("stops.txt"),
        "routes.txt" => Some("routes.txt"),
        "trips.txt" => Some("trips.txt"),
        "stop_times.txt" => Some("stop_times.txt"),
        "calendar.txt" => Some("calendar.txt"),
        "calendar_dates.txt" => Some("calendar_dates.txt"),
        "feed_info.txt" => Some("feed_info.txt"),
        _ => None,
    }
}

/// Builds a CSV reader for one GTFS text file.
///
/// A leading UTF-8 BOM is removed. Records may have fewer columns than the
/// header. Cells are not trimmed here; [`col`] trims them.
///
/// # Arguments
///
/// * `bytes` - File contents, optionally starting with a UTF-8 BOM.
///
/// # Returns
///
/// A `csv::Reader` over `bytes`.
fn csv_reader(bytes: &[u8]) -> csv::Reader<&[u8]> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    csv::ReaderBuilder::new()
        .flexible(true)
        .trim(csv::Trim::None)
        .from_reader(bytes)
}

/// Maps lowercased header names to column indexes.
///
/// Each header is trimmed and a leading `U+FEFF` is stripped. Duplicate names
/// keep the later column.
///
/// # Arguments
///
/// * `headers` - Header record from a GTFS file.
///
/// # Returns
///
/// Lowercased header text to its column index.
fn header_index(headers: &csv::StringRecord) -> HashMap<String, usize> {
    headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            (
                h.trim().trim_start_matches('\u{feff}').to_ascii_lowercase(),
                i,
            )
        })
        .collect()
}

/// Returns one trimmed cell, or an empty string when the column is absent.
///
/// # Arguments
///
/// * `record` - Data row.
/// * `idx` - Header map from [`header_index`].
/// * `name` - Lowercased GTFS column name.
///
/// # Returns
///
/// The trimmed cell. The borrow lasts as long as `record`.
fn col<'a>(record: &'a csv::StringRecord, idx: &HashMap<String, usize>, name: &str) -> &'a str {
    idx.get(name)
        .and_then(|i| record.get(*i))
        .map(str::trim)
        .unwrap_or("")
}

/// Drops an empty string.
///
/// # Arguments
///
/// * `value` - Cell text. Callers usually pass the trimmed result of [`col`].
///
/// # Returns
///
/// `Some` owned text, or `None` when `value` is empty. Whitespace is not trimmed here.
fn nonempty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Parses a GTFS decimal cell.
///
/// # Arguments
///
/// * `value` - Trimmed cell text.
///
/// # Returns
///
/// The `f64`, or `None` when `value` is empty or not a float.
fn parse_f64(value: &str) -> Option<f64> {
    if value.is_empty() {
        None
    } else {
        value.parse().ok()
    }
}

/// Parses a GTFS integer cell.
///
/// # Arguments
///
/// * `value` - Trimmed cell text.
///
/// # Returns
///
/// The `i64`, or `None` when `value` is empty or not an integer. `1.0` is rejected.
fn parse_i64(value: &str) -> Option<i64> {
    if value.is_empty() {
        None
    } else {
        value.parse().ok()
    }
}

/// Reads a GTFS calendar weekday cell.
///
/// # Arguments
///
/// * `value` - Trimmed cell text, usually `0` or `1`.
///
/// # Returns
///
/// `true` only when `value` is `"1"`.
fn flag(value: &str) -> bool {
    value == "1"
}

/// Parses a GTFS `HH:MM:SS` clock time into seconds after midnight.
///
/// The hour is any integer that parses, including values above 23 and negative
/// values, so a trip can run past midnight. Minutes and seconds must be `0..=59`.
/// A value with other than three colon-separated fields is rejected. `08:30:00`
/// is `30600`. `25:10:00` is `90600`. Empty text, `08:60:00`, and `08:30` are
/// `None`.
///
/// # Arguments
///
/// * `value` - An `arrival_time` or `departure_time` cell. Ends are trimmed.
///
/// # Returns
///
/// Seconds after midnight, or `None` when `value` is empty or not `HH:MM:SS`.
pub fn parse_gtfs_time(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let mut parts = value.split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next()?.parse().ok()?;
    let seconds: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(0..=59).contains(&minutes) || !(0..=59).contains(&seconds) {
        return None;
    }
    Some(hours * 3600 + minutes * 60 + seconds)
}

/// Maps a basic GTFS `route_type` code to a short name.
///
/// Extended codes, such as `100` for railway, and every other integer return
/// `"other"`. `3` is `"bus"` and `11` is `"trolleybus"`.
///
/// The names are:
///
/// * `0` - `tram`
/// * `1` - `subway`
/// * `2` - `rail`
/// * `3` - `bus`
/// * `4` - `ferry`
/// * `5` - `cable_tram`
/// * `6` - `aerial`
/// * `7` - `funicular`
/// * `11` - `trolleybus`
/// * `12` - `monorail`
///
/// # Arguments
///
/// * `route_type` - Value from `routes.route_type` or a joined row.
///
/// # Returns
///
/// One of the names above, or `"other"`.
pub fn route_type_name(route_type: i64) -> &'static str {
    match route_type {
        0 => "tram",
        1 => "subway",
        2 => "rail",
        3 => "bus",
        4 => "ferry",
        5 => "cable_tram",
        6 => "aerial",
        7 => "funicular",
        11 => "trolleybus",
        12 => "monorail",
        _ => "other",
    }
}
