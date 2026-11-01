//! Turns a static GTFS database and decoded GTFS-Realtime into JSON.
//!
//! [`trip_status`], [`trip_schedule`], and [`departure_board`] join [`StaticDb`]
//! stop times to trip updates borrowed through [`RtView`]. [`vehicle_positions`],
//! [`trip_updates`], and [`alerts`] page one realtime feed and attach static names.
//! [`decode_message`] renders a [`crate::gtfs_rt::FeedMessage`] without the static
//! tables.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, NaiveDate, TimeZone, Timelike};
use chrono_tz::Tz;
use serde_json::{json, Value};

use crate::error::{err, AppError};
use crate::gtfs::{route_type_name, StaticDb, StopRow, StopTimeRow, TripRow};
use crate::gtfs_rt::trip_update::stop_time_update::ScheduleRelationship as StopRel;
use crate::gtfs_rt::{FeedEntity, FeedMessage, TripUpdate, VehiclePosition};
use crate::realtime::{
    alert_periods, cause_name, delay_label, effect_name, find_stop_update, incrementality,
    is_canceled, json_i32, json_i64, json_opt, obj, occupancy_name, period_contains, predict,
    propagated_delay, round_coord, severity_name, stop_relationship, translated, trip_relationship,
    vehicle_json, vehicle_status,
};

/// Borrowed GTFS-Realtime messages for one query.
///
/// Any feed may be absent when that URL was not configured.
#[derive(Clone)]
pub struct RtView<'a> {
    /// TripUpdates [`crate::gtfs_rt::FeedMessage`], when one was loaded.
    pub trip_updates: Option<&'a FeedMessage>,
    /// VehiclePositions [`crate::gtfs_rt::FeedMessage`], when one was loaded.
    pub vehicles: Option<&'a FeedMessage>,
    /// Alerts [`crate::gtfs_rt::FeedMessage`], when one was loaded.
    pub alerts: Option<&'a FeedMessage>,
}

/// Selector for [`trip_status`].
pub struct TripStatusQuery {
    /// Trip id or trip short name.
    pub trip: String,
    /// Stop id, stop code, or stop name. `None` leaves `at_stop` empty.
    pub stop: Option<String>,
    /// Unix seconds used as the clock for scoring and upcoming stops.
    pub at_unix: i64,
    /// Maximum upcoming stops included on each rendered trip.
    pub upcoming_limit: usize,
}

/// Selector for [`departure_board`].
pub struct BoardQuery {
    /// Stop id, stop code, or stop name.
    pub stop: String,
    /// Route id or route short name. `None` keeps every route.
    pub route: Option<String>,
    /// Unix seconds at the center of the departure window.
    pub at_unix: i64,
    /// How far ahead of `at_unix` to search, in minutes.
    pub window_minutes: u64,
    /// How far behind `at_unix` to search, in minutes.
    pub past_minutes: u64,
    /// Maximum departures to return after `offset`.
    pub limit: usize,
    /// Matching rows to skip.
    pub offset: usize,
    /// Keep stops whose realtime relationship is `SKIPPED`.
    pub include_skipped: bool,
}

/// Selector for [`trip_schedule`].
pub struct ScheduleQuery {
    /// Trip id or trip short name.
    pub trip: String,
    /// Unix seconds used to choose the service date.
    pub at_unix: i64,
    /// Maximum stops to return.
    pub limit: usize,
    /// Stops to skip.
    pub offset: usize,
}

/// Filters and page window for a vehicle-position query.
///
/// String filters are optional. `None` leaves that dimension unrestricted.
pub struct VehicleQuery {
    /// Route id or route short name.
    pub route: Option<String>,
    /// Trip id or trip short name.
    pub trip: Option<String>,
    /// Vehicle id or vehicle label.
    pub vehicle: Option<String>,
    /// Stop id the vehicle reports.
    pub stop: Option<String>,
    /// Maximum vehicles to return after `offset`.
    pub limit: usize,
    /// Matching rows to skip.
    pub offset: usize,
}

/// Filters and page window for a trip-update query.
///
/// String filters are optional. `None` leaves that dimension unrestricted.
pub struct TripUpdateQuery {
    /// Route id or route short name.
    pub route: Option<String>,
    /// Trip id or trip short name.
    pub trip: Option<String>,
    /// Stop id, stop code, or stop name.
    pub stop: Option<String>,
    /// Maximum trip updates to return after `offset`.
    pub limit: usize,
    /// Matching rows to skip.
    pub offset: usize,
    /// Maximum stop-time updates copied onto each trip.
    pub stop_limit: usize,
}

/// Filters and page window for a service-alert query.
///
/// String filters are optional. `None` leaves that dimension unrestricted.
pub struct AlertQuery {
    /// Route id or route short name.
    pub route: Option<String>,
    /// Stop id, stop code, or stop name.
    pub stop: Option<String>,
    /// Case-insensitive substring of the header or the description.
    pub text: Option<String>,
    /// When `true`, keep alerts with no period and alerts whose periods contain `at_unix`.
    pub active_only: bool,
    /// Unix seconds used when `active_only` is set.
    pub at_unix: i64,
    /// Maximum alerts to return after `offset`.
    pub limit: usize,
    /// Matching rows to skip.
    pub offset: usize,
    /// Maximum characters kept from the header and from the description.
    pub max_chars: usize,
}

/// Builds the trip-status JSON for a trip id or short name.
///
/// Each static match is scored on the active dates among yesterday and today, or on the local date when neither is active. Up to two live trips within 25 points of the best score are rendered. When no live trip qualifies, the single best candidate is rendered. Stop times are capped at 400. Other matches are listed under `alternatives`, up to five. `matched` counts trip and service-date pairs.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `rt` - Borrowed realtime feeds ([`RtView`]). A missing feed contributes no live rows.
/// * `query` - Trip selector, optional stop, clock, and upcoming-stop limit ([`TripStatusQuery`]).
///
/// # Returns
///
/// JSON with `matched` and `trips`. `alternatives` is present when other matches were not rendered.
///
/// # Errors
///
/// * `query.trip` is empty or only whitespace.
/// * No static trip has that id or short name.
/// * `query.stop` is set and is empty, matches nothing, or matches more than one stop.
/// * SQLite fails while loading the trip, its stop times, the stop, or the vehicle stop.
pub fn trip_status(
    db: &StaticDb,
    rt: RtView<'_>,
    query: &TripStatusQuery,
) -> Result<Value, AppError> {
    let trips = resolve_trips(db, &query.trip)?;
    if trips.is_empty() {
        return Err(err(format!("no trip matches '{}'", query.trip))
            .hint("pass a trip_id or trip_short_name (train number)"));
    }
    let vehicles = index_vehicles(rt.vehicles);
    let updates = index_updates(rt.trip_updates);
    let mut ranked = Vec::new();
    for trip in &trips {
        let dates = service_dates(db, &trip.service_id, query.at_unix);
        let dates = if dates.is_empty() {
            vec![local_date(query.at_unix, db.tz)]
        } else {
            dates
        };
        for date in dates {
            let stops = db.trip_stop_times(&trip.trip_id, 400, 0)?;
            let vehicle = vehicles.get(trip.trip_id.as_str()).copied();
            let update = updates.get(trip.trip_id.as_str()).copied();
            let score = score_candidate(query.at_unix, db.tz, date, &stops, vehicle, update);
            ranked.push((score, date, trip, stops));
        }
    }
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.trip_id.cmp(&b.2.trip_id)));
    let best_score = ranked.first().map(|row| row.0).unwrap_or(0);
    let mut chosen: Vec<_> = ranked
        .iter()
        .filter(|row| row.0 + 25 >= best_score && vehicles.contains_key(row.2.trip_id.as_str()))
        .take(2)
        .collect();
    if chosen.is_empty() {
        chosen.extend(ranked.iter().take(1));
    }
    let at_stop = match &query.stop {
        Some(stop) => Some(db.resolve_stop(stop)?),
        None => None,
    };
    let mut full = Vec::new();
    let mut seen = HashSet::new();
    for (_, date, trip, stops) in &chosen {
        seen.insert(trip.trip_id.clone());
        full.push(render_trip(
            db,
            query,
            trip,
            *date,
            stops,
            vehicles.get(trip.trip_id.as_str()).copied(),
            updates.get(trip.trip_id.as_str()).copied(),
            at_stop.as_ref(),
        )?);
    }
    let alternatives: Vec<Value> = ranked
        .iter()
        .filter(|(_, _, trip, _)| !seen.contains(&trip.trip_id))
        .take(5)
        .map(|(_, date, trip, _)| {
            obj([
                ("trip_id", json!(trip.trip_id)),
                ("trip_short_name", json_opt(trip.short_name.clone())),
                ("headsign", json_opt(trip.headsign.clone())),
                ("route_short_name", json_opt(trip.route_short_name.clone())),
                ("service_date", json!(date.format("%Y%m%d").to_string())),
                ("live", json!(vehicles.contains_key(trip.trip_id.as_str()))),
            ])
        })
        .collect();
    Ok(obj([
        ("matched", json!(ranked.len())),
        ("trips", Value::Array(full)),
        (
            "alternatives",
            if alternatives.is_empty() {
                Value::Null
            } else {
                Value::Array(alternatives)
            },
        ),
    ]))
}

/// Builds one page of a trip's stop schedule with realtime estimates.
///
/// A short name that matches several trips is rejected unless `query.trip` is that trip's id. The service date is the later active date among yesterday and today, or the local date of `at_unix` when neither calendar day is active.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `rt` - Borrowed realtime feeds ([`RtView`]). Trip updates overlay the scheduled times.
/// * `query` - Trip selector, clock, and page window ([`ScheduleQuery`]).
///
/// # Returns
///
/// JSON for the trip header and one page of `stops`. `next_offset` is set when more stops remain.
///
/// # Errors
///
/// * `query.trip` is empty or only whitespace.
/// * No static trip has that id or short name.
/// * The text matches more than one trip and is not itself a trip id.
/// * SQLite fails while loading the trip or its stop times.
pub fn trip_schedule(
    db: &StaticDb,
    rt: RtView<'_>,
    query: &ScheduleQuery,
) -> Result<Value, AppError> {
    let trips = resolve_trips(db, &query.trip)?;
    let Some(trip) = trips.first() else {
        return Err(err(format!("no trip matches '{}'", query.trip)));
    };
    if trips.len() > 1 && db.trip_by_id(&query.trip)?.is_none() {
        let ids: Vec<_> = trips.iter().take(8).map(|t| json!(t.trip_id)).collect();
        return Err(err(format!(
            "'{}' matches {} trips; pass a trip_id",
            query.trip,
            trips.len()
        ))
        .extra("candidates", Value::Array(ids)));
    }
    let total = db.trip_stop_count(&trip.trip_id)? as usize;
    let rows = db.trip_stop_times(&trip.trip_id, query.limit, query.offset)?;
    let date = service_dates(db, &trip.service_id, query.at_unix)
        .into_iter()
        .next_back()
        .unwrap_or_else(|| local_date(query.at_unix, db.tz));
    let update = index_updates(rt.trip_updates)
        .get(trip.trip_id.as_str())
        .copied()
        .cloned();
    let stops: Vec<Value> = rows
        .iter()
        .map(|row| stop_json(db, &update, trip_rel_of(update.as_ref()), row, date, db.tz))
        .collect();
    let next = query.offset + stops.len();
    Ok(obj([
        ("trip_id", json!(trip.trip_id)),
        ("trip_short_name", json_opt(trip.short_name.clone())),
        ("headsign", json_opt(trip.headsign.clone())),
        ("route_id", json!(trip.route_id)),
        ("route_short_name", json_opt(trip.route_short_name.clone())),
        ("route_long_name", json_opt(trip.route_long_name.clone())),
        ("service_date", json!(date.format("%Y%m%d").to_string())),
        (
            "service_active",
            json!(db.service_active(&trip.service_id, date)),
        ),
        ("timezone", json!(db.tz_name)),
        ("stop_count", json!(total)),
        ("offset", json!(query.offset)),
        ("returned", json!(stops.len())),
        (
            "next_offset",
            if next < total {
                json!(next)
            } else {
                Value::Null
            },
        ),
        ("stops", Value::Array(stops)),
    ]))
}

/// Builds a departure board for a stop.
///
/// Yesterday and today are both searched so overnight trips remain visible. A row is kept when its service runs and its scheduled departure, or arrival when departure is missing, falls from `past_minutes` before `at_unix` through `window_minutes` after it. A route filter keeps a row whose route id equals a resolved id, or whose short name equals a filter string ignoring ASCII case. Stops marked `SKIPPED` are dropped unless `include_skipped` is set. Rows sort by estimated departure, then by the scheduled instant.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `rt` - Borrowed realtime feeds ([`RtView`]). Trip updates supply delays and skip flags.
/// * `query` - Stop, optional route, clock, window, and page ([`BoardQuery`]).
///
/// # Returns
///
/// JSON for the stop header and one page of `departures`.
///
/// # Errors
///
/// * The stop text is empty, matches nothing, or matches more than one stop.
/// * SQLite fails while expanding the stop, resolving a route filter, or loading stop times.
pub fn departure_board(
    db: &StaticDb,
    rt: RtView<'_>,
    query: &BoardQuery,
) -> Result<Value, AppError> {
    let stop = db.resolve_stop(&query.stop)?;
    let stop_ids = db.expand_stop_ids(&stop.stop_id)?;
    let route_ids = match &query.route {
        Some(route) => {
            let ids = db.resolve_route_ids(route)?;
            if ids.is_empty() {
                vec![route.clone()]
            } else {
                ids
            }
        }
        None => Vec::new(),
    };
    let updates = index_updates(rt.trip_updates);
    let today = local_date(query.at_unix, db.tz);
    let mut dates = Vec::new();
    if let Some(yesterday) = today.pred_opt() {
        dates.push(yesterday);
    }
    dates.push(today);
    let rows = db.stop_times_for_stops(&stop_ids)?;
    let start = query.at_unix - (query.past_minutes as i64) * 60;
    let end = query.at_unix + (query.window_minutes as i64) * 60;
    let mut departures = Vec::new();
    for date in dates {
        for row in &rows {
            if !db.service_active(&row.service_id, date) {
                continue;
            }
            if !route_ids.is_empty()
                && !route_ids.iter().any(|id| id == &row.route_id)
                && !route_ids.iter().any(|id| {
                    row.route_short_name
                        .as_deref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(id))
                })
            {
                continue;
            }
            let event_sec = row.departure_sec.or(row.arrival_sec);
            let Some(event_sec) = event_sec else {
                continue;
            };
            let scheduled = gtfs_unix(date, event_sec, db.tz);
            if scheduled < start || scheduled > end {
                continue;
            }
            let update = updates.get(row.trip_id.as_str()).copied();
            let rendered = stop_json(db, &update.cloned(), trip_rel_of(update), row, date, db.tz);
            if !query.include_skipped
                && rendered.get("skipped").and_then(|v| v.as_bool()) == Some(true)
            {
                continue;
            }
            departures.push((scheduled, rendered));
        }
    }
    departures.sort_by(|a, b| {
        let ae =
            a.1.get("estimated_departure")
                .and_then(|v| v.as_i64())
                .unwrap_or(a.0);
        let be =
            b.1.get("estimated_departure")
                .and_then(|v| v.as_i64())
                .unwrap_or(b.0);
        ae.cmp(&be).then(a.0.cmp(&b.0))
    });
    let total = departures.len();
    let offset = query.offset.min(total);
    let end_index = (offset + query.limit).min(total);
    let page: Vec<_> = departures[offset..end_index]
        .iter()
        .map(|(_, row)| row.clone())
        .collect();
    Ok(obj([
        ("stop_id", json!(stop.stop_id)),
        ("stop_name", json_opt(stop.stop_name.clone())),
        ("timezone", json!(db.tz_name)),
        ("at_unix", json!(query.at_unix)),
        ("at_local", json!(hhmm(query.at_unix, db.tz))),
        ("window_minutes", json!(query.window_minutes)),
        ("past_minutes", json!(query.past_minutes)),
        ("expanded_stops", json!(stop_ids.len())),
        ("total", json!(total)),
        ("offset", json!(offset)),
        ("returned", json!(page.len())),
        (
            "next_offset",
            if end_index < total {
                json!(end_index)
            } else {
                Value::Null
            },
        ),
        ("departures", Value::Array(page)),
    ]))
}

/// Pages vehicle positions and joins them to static trip and stop names.
///
/// Vehicles sort by timestamp descending, then by entity id. The vehicle filter matches the id exactly or the label with ASCII case folding. The trip filter matches a resolved trip id, or applies that same label comparison to the vehicle label. The route filter matches the realtime route id or the static trip's route. The stop filter matches the reported stop id after parent and platform expansion.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `rt` - Borrowed realtime feeds ([`RtView`]).
/// * `query` - Optional filters and the page window ([`VehicleQuery`]).
///
/// # Returns
///
/// JSON with the feed header timestamp and one page under `vehicles`.
///
/// # Errors
///
/// * [`RtView::vehicles`] is absent.
/// * SQLite fails while resolving a trip, route, or stop filter, or while loading stop names.
pub fn vehicle_positions(
    db: &StaticDb,
    rt: RtView<'_>,
    query: &VehicleQuery,
) -> Result<Value, AppError> {
    let Some(feed) = rt.vehicles else {
        return Err(err("this feed has no vehicle positions"));
    };
    let trip_filter = trip_filter(db, query.trip.as_deref())?;
    let route_ids = route_filter(db, query.route.as_deref())?;
    let stop_ids: Option<HashSet<String>> = match &query.stop {
        Some(stop) => Some(HashSet::from_iter(
            db.expand_stop_ids(&db.resolve_stop(stop)?.stop_id)?,
        )),
        None => None,
    };
    let mut matched: Vec<&FeedEntity> = feed
        .entity
        .iter()
        .filter(|entity| entity.vehicle.is_some())
        .filter(|entity| {
            let vehicle = entity.vehicle.as_ref().unwrap();
            if let Some(wanted) = &query.vehicle {
                let id = vehicle.vehicle.as_ref().and_then(|v| v.id.as_deref());
                let label = vehicle.vehicle.as_ref().and_then(|v| v.label.as_deref());
                if id != Some(wanted.as_str())
                    && label.map(|l| l.eq_ignore_ascii_case(wanted)) != Some(true)
                {
                    return false;
                }
            }
            let trip_id = vehicle.trip.as_ref().and_then(|t| t.trip_id.as_deref());
            if let Some(filter) = &trip_filter {
                let label = vehicle.vehicle.as_ref().and_then(|v| v.label.as_deref());
                let id_ok = trip_id.is_some_and(|id| filter.contains(id));
                let label_ok = label.is_some_and(|label| {
                    filter.iter().any(|id| id.eq_ignore_ascii_case(label))
                        || query
                            .trip
                            .as_ref()
                            .is_some_and(|q| label.eq_ignore_ascii_case(q))
                });
                if !id_ok && !label_ok {
                    return false;
                }
            }
            if let Some(routes) = &route_ids {
                let route = vehicle.trip.as_ref().and_then(|t| t.route_id.as_deref());
                let static_route = trip_id.and_then(|id| db.trip_route_id(id).ok().flatten());
                if !route.is_some_and(|id| routes.iter().any(|r| r == id))
                    && !static_route.is_some_and(|id| routes.iter().any(|r| r == &id))
                {
                    return false;
                }
            }
            if let Some(stops) = &stop_ids {
                if !vehicle
                    .stop_id
                    .as_ref()
                    .is_some_and(|id| stops.contains(id))
                {
                    return false;
                }
            }
            true
        })
        .collect();
    matched.sort_by(|a, b| {
        let ta = a.vehicle.as_ref().and_then(|v| v.timestamp).unwrap_or(0);
        let tb = b.vehicle.as_ref().and_then(|v| v.timestamp).unwrap_or(0);
        tb.cmp(&ta).then(a.id.cmp(&b.id))
    });
    let total = matched.len();
    let offset = query.offset.min(total);
    let end = (offset + query.limit).min(total);
    let page = &matched[offset..end];
    let mut stop_ids_page = Vec::new();
    for entity in page {
        if let Some(id) = entity.vehicle.as_ref().and_then(|v| v.stop_id.clone()) {
            stop_ids_page.push(id);
        }
    }
    let stops = db.stops_for_ids(&stop_ids_page)?;
    let rows: Vec<Value> = page
        .iter()
        .map(|entity| vehicle_row(db, entity, &stops))
        .collect();
    Ok(page_object(feed, "vehicles", rows, total, offset))
}

/// Pages trip updates and joins them to static names.
///
/// Rows sort by entity id. A stop filter keeps an update that names the stop, names its assigned stop, or carries a stop sequence the static trip uses at that stop. `stop_limit` caps the stop-time updates copied onto each row.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `rt` - Borrowed realtime feeds ([`RtView`]).
/// * `query` - Optional filters, page window, and per-trip stop cap ([`TripUpdateQuery`]).
///
/// # Returns
///
/// JSON with the feed header timestamp and one page under `trip_updates`.
///
/// # Errors
///
/// * [`RtView::trip_updates`] is absent.
/// * SQLite fails while resolving a trip, route, or stop filter.
pub fn trip_updates(
    db: &StaticDb,
    rt: RtView<'_>,
    query: &TripUpdateQuery,
) -> Result<Value, AppError> {
    let Some(feed) = rt.trip_updates else {
        return Err(err("this feed has no trip updates"));
    };
    let trip_filter = trip_filter(db, query.trip.as_deref())?;
    let route_ids = route_filter(db, query.route.as_deref())?;
    let stop_scope = match &query.stop {
        Some(stop) => {
            let resolved = db.resolve_stop(stop)?;
            let ids = db.expand_stop_ids(&resolved.stop_id)?;
            let visits = db.stop_times_for_stops(&ids)?;
            let mut by_trip: HashMap<String, HashSet<i64>> = HashMap::new();
            for row in visits {
                by_trip
                    .entry(row.trip_id)
                    .or_default()
                    .insert(row.stop_sequence);
            }
            Some((HashSet::from_iter(ids), by_trip))
        }
        None => None,
    };
    let mut matched: Vec<&FeedEntity> = feed
        .entity
        .iter()
        .filter(|entity| entity.trip_update.is_some())
        .filter(|entity| {
            let update = entity.trip_update.as_ref().unwrap();
            let trip_id = update.trip.trip_id.as_deref();
            if let Some(filter) = &trip_filter {
                if !trip_id.is_some_and(|id| filter.contains(id)) {
                    return false;
                }
            }
            if let Some(routes) = &route_ids {
                let route = update.trip.route_id.as_deref();
                let static_route = trip_id.and_then(|id| db.trip_route_id(id).ok().flatten());
                if !route.is_some_and(|id| routes.iter().any(|r| r == id))
                    && !static_route.is_some_and(|id| routes.iter().any(|r| r == &id))
                {
                    return false;
                }
            }
            if let Some((stop_ids, by_trip)) = &stop_scope {
                if !update_hits_stop(update, stop_ids, by_trip) {
                    return false;
                }
            }
            true
        })
        .collect();
    matched.sort_by(|a, b| a.id.cmp(&b.id));
    let total = matched.len();
    let offset = query.offset.min(total);
    let end = (offset + query.limit).min(total);
    let rows: Vec<Value> = matched[offset..end]
        .iter()
        .map(|entity| trip_update_row(db, entity, query.stop_limit))
        .collect();
    Ok(page_object(feed, "trip_updates", rows, total, offset))
}

/// Pages service alerts and joins them to static route names.
///
/// Rows sort by entity id. When `active_only` is set, an alert that lists at least one period is kept only when one period contains `at_unix`. Alerts that list no period stay. Text matching is a case-insensitive substring of the header or the description.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `rt` - Borrowed realtime feeds ([`RtView`]).
/// * `query` - Optional filters, clock, page window, and text cap ([`AlertQuery`]).
///
/// # Returns
///
/// JSON with the feed header timestamp and one page under `alerts`.
///
/// # Errors
///
/// * [`RtView::alerts`] is absent.
/// * SQLite fails while resolving a route or stop filter.
pub fn alerts(db: &StaticDb, rt: RtView<'_>, query: &AlertQuery) -> Result<Value, AppError> {
    let Some(feed) = rt.alerts else {
        return Err(err("this feed has no alerts"));
    };
    let route_ids = route_filter(db, query.route.as_deref())?;
    let stop_ids: Option<HashSet<String>> = match &query.stop {
        Some(stop) => Some(HashSet::from_iter(
            db.expand_stop_ids(&db.resolve_stop(stop)?.stop_id)?,
        )),
        None => None,
    };
    let needle = query.text.as_ref().map(|t| t.to_lowercase());
    let mut matched: Vec<&FeedEntity> = feed
        .entity
        .iter()
        .filter(|entity| entity.alert.is_some())
        .filter(|entity| {
            let alert = entity.alert.as_ref().unwrap();
            if query.active_only {
                let periods = alert_periods(alert);
                if !periods.is_empty()
                    && !periods
                        .iter()
                        .any(|range| period_contains(range, query.at_unix))
                {
                    return false;
                }
            }
            if let Some(routes) = &route_ids {
                if !alert_matches_route(db, alert, routes) {
                    return false;
                }
            }
            if let Some(stops) = &stop_ids {
                if !alert.informed_entity.iter().any(|selector| {
                    selector
                        .stop_id
                        .as_ref()
                        .is_some_and(|id| stops.contains(id))
                }) {
                    return false;
                }
            }
            if let Some(needle) = &needle {
                let header = translated(alert.header_text.as_ref(), 2000)
                    .map(|t| t.0)
                    .unwrap_or_default()
                    .to_lowercase();
                let body = translated(alert.description_text.as_ref(), 4000)
                    .map(|t| t.0)
                    .unwrap_or_default()
                    .to_lowercase();
                if !header.contains(needle) && !body.contains(needle) {
                    return false;
                }
            }
            true
        })
        .collect();
    matched.sort_by(|a, b| a.id.cmp(&b.id));
    let total = matched.len();
    let offset = query.offset.min(total);
    let end = (offset + query.limit).min(total);
    let rows: Vec<Value> = matched[offset..end]
        .iter()
        .map(|entity| alert_row(db, entity, query.max_chars))
        .collect();
    Ok(page_object(feed, "alerts", rows, total, offset))
}

/// Renders one feed message as JSON without the static database.
///
/// # Arguments
///
/// * `message` - Decoded [`crate::gtfs_rt::FeedMessage`].
/// * `offset` - Entities to skip. Values past the end clamp to the entity count.
/// * `limit` - Maximum entities to return.
/// * `max_chars` - Maximum characters kept from each alert header and description.
///
/// # Returns
///
/// JSON for the feed header and one page of `entities`. `next_offset` is set when more entities remain.
pub fn decode_message(
    message: &FeedMessage,
    offset: usize,
    limit: usize,
    max_chars: usize,
) -> Value {
    let total = message.entity.len();
    let offset = offset.min(total);
    let end = (offset + limit).min(total);
    let entities: Vec<Value> = message.entity[offset..end]
        .iter()
        .map(|entity| decode_entity(entity, max_chars))
        .collect();
    obj([
        (
            "header",
            obj([
                (
                    "gtfs_realtime_version",
                    json!(message.header.gtfs_realtime_version),
                ),
                (
                    "incrementality",
                    json_opt(incrementality(message.header.incrementality)),
                ),
                (
                    "timestamp",
                    json_i64(message.header.timestamp.map(|v| v as i64)),
                ),
                (
                    "feed_version",
                    json_opt(message.header.feed_version.clone()),
                ),
            ]),
        ),
        ("entity_count", json!(total)),
        ("offset", json!(offset)),
        ("returned", json!(entities.len())),
        (
            "next_offset",
            if end < total { json!(end) } else { Value::Null },
        ),
        ("entities", Value::Array(entities)),
    ])
}

/// Renders one chosen trip, including the vehicle, the next stop, and upcoming stops.
///
/// Upcoming stops start at the vehicle's current stop sequence when that sequence is present. Otherwise they start at the first stop whose estimated departure, scheduled departure, or estimated arrival is at or after `query.at_unix` minus 60 seconds. `query.upcoming_limit` caps the list. The trip delay is the next stop's delay.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `query` - Clock, upcoming limit, and optional stop ([`TripStatusQuery`]).
/// * `trip` - Static trip ([`TripRow`]).
/// * `date` - Service date used to place scheduled times.
/// * `stops` - Static stop times for the trip ([`StopTimeRow`]).
/// * `vehicle` - Live [`crate::gtfs_rt::VehiclePosition`], when the vehicle feed has one.
/// * `update` - [`crate::gtfs_rt::TripUpdate`] for this trip, when the feed has one.
/// * `at_stop` - Resolved stop the caller asked about ([`StopRow`]).
///
/// # Returns
///
/// JSON for that trip. `at_stop` is omitted when the caller did not ask for a stop, and a note object when the trip does not serve the requested stop.
///
/// # Errors
///
/// * SQLite fails while expanding `at_stop` into parent and platform ids.
/// * SQLite fails while loading the vehicle's current stop.
#[allow(clippy::too_many_arguments)]
fn render_trip(
    db: &StaticDb,
    query: &TripStatusQuery,
    trip: &TripRow,
    date: NaiveDate,
    stops: &[StopTimeRow],
    vehicle: Option<&VehiclePosition>,
    update: Option<&TripUpdate>,
    at_stop: Option<&StopRow>,
) -> Result<Value, AppError> {
    let canceled = update
        .map(|update| is_canceled(update.trip.schedule_relationship))
        .unwrap_or(false);
    let relationship =
        update.and_then(|update| trip_relationship(update.trip.schedule_relationship));
    let owned = update.cloned();
    let rendered: Vec<Value> = stops
        .iter()
        .map(|row| stop_json(db, &owned, relationship.clone(), row, date, db.tz))
        .collect();
    let cursor = vehicle
        .and_then(|vehicle| vehicle.current_stop_sequence)
        .map(|seq| seq as i64);
    let upcoming_start = if let Some(seq) = cursor {
        rendered
            .iter()
            .position(|row| row.get("stop_sequence").and_then(|v| v.as_i64()) == Some(seq))
    } else {
        rendered.iter().position(|row| {
            let when = row
                .get("estimated_departure")
                .and_then(|v| v.as_i64())
                .or_else(|| row.get("scheduled_departure").and_then(|v| v.as_i64()))
                .or_else(|| row.get("estimated_arrival").and_then(|v| v.as_i64()));
            when.is_some_and(|when| when >= query.at_unix - 60)
        })
    };
    let upcoming = if let Some(start) = upcoming_start {
        rendered
            .iter()
            .skip(start)
            .take(query.upcoming_limit)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let next_stop = upcoming.first().cloned();
    let delay_sec = next_stop.as_ref().and_then(|row| {
        row.get("delay_sec")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
    });
    let at_stop_json = if let Some(wanted) = at_stop {
        let ids = db.expand_stop_ids(&wanted.stop_id)?;
        let found = rendered.into_iter().find(|row| {
            row.get("stop_id")
                .and_then(|v| v.as_str())
                .is_some_and(|id| ids.iter().any(|candidate| candidate == id))
        });
        match found {
            Some(row) => row,
            None => obj([
                ("stop_id", json!(wanted.stop_id)),
                (
                    "note",
                    json!(format!("trip {} does not stop here", trip.trip_id)),
                ),
            ]),
        }
    } else {
        Value::Null
    };
    let mut vehicle_out = Value::Null;
    if let Some(vehicle) = vehicle {
        let ids = vehicle.stop_id.clone().into_iter().collect::<Vec<_>>();
        let stops = db.stops_for_ids(&ids)?;
        vehicle_out = vehicle_position_json(db, vehicle, &stops);
    }
    Ok(obj([
        ("trip_id", json!(trip.trip_id)),
        ("trip_short_name", json_opt(trip.short_name.clone())),
        ("headsign", json_opt(trip.headsign.clone())),
        ("route_id", json!(trip.route_id)),
        ("route_short_name", json_opt(trip.route_short_name.clone())),
        ("route_long_name", json_opt(trip.route_long_name.clone())),
        ("route_type", json!(route_type_name(trip.route_type))),
        ("direction_id", json_i64(trip.direction_id)),
        ("service_date", json!(date.format("%Y%m%d").to_string())),
        ("timezone", json!(db.tz_name)),
        ("schedule_relationship", json_opt(relationship)),
        ("canceled", if canceled { json!(true) } else { Value::Null }),
        ("delay_sec", json_i32(delay_sec)),
        (
            "delay_label",
            delay_sec
                .map(|sec| json!(delay_label(sec)))
                .unwrap_or(Value::Null),
        ),
        ("vehicle", vehicle_out),
        ("next_stop", next_stop.unwrap_or(Value::Null)),
        ("upcoming_stops", Value::Array(upcoming)),
        ("at_stop", at_stop_json),
    ]))
}

/// Merges one static stop time with the trip update.
///
/// Scheduled times are Unix seconds on `date` in `tz`. An exact stop-time update supplies the estimate. When the stop has no exact update and the trip is not `CANCELED` or `DELETED`, the delay from [`crate::realtime::propagated_delay`] is applied. The platform is the assigned stop's platform, otherwise the static platform.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]), used to name an assigned stop.
/// * `update` - Trip update for this trip, when one exists.
/// * `trip_relationship_name` - Trip-level schedule relationship, already named.
/// * `row` - Static stop time ([`StopTimeRow`]).
/// * `date` - Service date.
/// * `tz` - Agency timezone.
///
/// # Returns
///
/// JSON for that stop. Null fields are omitted.
fn stop_json(
    db: &StaticDb,
    update: &Option<TripUpdate>,
    trip_relationship_name: Option<String>,
    row: &StopTimeRow,
    date: NaiveDate,
    tz: Tz,
) -> Value {
    let sched_arr = row.arrival_sec.map(|sec| gtfs_unix(date, sec, tz));
    let sched_dep = row.departure_sec.map(|sec| gtfs_unix(date, sec, tz));
    let canceled = trip_relationship_name.as_deref() == Some("CANCELED")
        || trip_relationship_name.as_deref() == Some("DELETED");
    let exact = update
        .as_ref()
        .and_then(|update| find_stop_update(update, row.stop_sequence as u32, &row.stop_id));
    let mut arrival = predict(sched_arr, exact.and_then(|row| row.arrival.as_ref()));
    let mut departure = predict(sched_dep, exact.and_then(|row| row.departure.as_ref()));
    if exact.is_none() && !canceled {
        if let Some(delay) = update
            .as_ref()
            .and_then(|update| propagated_delay(update, row.stop_sequence as u32))
        {
            if arrival.delay_sec.is_none() {
                arrival.delay_sec = Some(delay);
                arrival.estimated = sched_arr.map(|sched| sched + i64::from(delay));
            }
            if departure.delay_sec.is_none() {
                departure.delay_sec = Some(delay);
                departure.estimated = sched_dep.map(|sched| sched + i64::from(delay));
            }
        }
    }
    let relationship = exact.and_then(|row| stop_relationship(row.schedule_relationship));
    let skipped = exact.and_then(|row| row.schedule_relationship) == Some(StopRel::Skipped as i32);
    let assigned = exact.and_then(|row| {
        row.stop_time_properties
            .as_ref()
            .and_then(|props| props.assigned_stop_id.clone())
    });
    let assigned_stop = assigned
        .as_deref()
        .and_then(|id| db.stop_by_id(id).ok().flatten());
    let platform = assigned_stop
        .as_ref()
        .and_then(|stop| stop.platform_code.clone())
        .or_else(|| row.platform_code.clone());
    let delay = departure.delay_sec.or(arrival.delay_sec);
    obj([
        ("stop_sequence", json!(row.stop_sequence)),
        ("stop_id", json!(row.stop_id)),
        ("stop_name", json_opt(row.stop_name.clone())),
        ("trip_id", json!(row.trip_id)),
        ("trip_short_name", json_opt(row.trip_short_name.clone())),
        (
            "headsign",
            json_opt(row.headsign.clone().or(row.trip_headsign.clone())),
        ),
        ("route_id", json!(row.route_id)),
        ("route_short_name", json_opt(row.route_short_name.clone())),
        ("route_long_name", json_opt(row.route_long_name.clone())),
        ("route_type", json!(route_type_name(row.route_type))),
        ("scheduled_arrival", json_i64(sched_arr)),
        (
            "scheduled_arrival_local",
            sched_arr
                .map(|unix| json!(hhmm(unix, tz)))
                .unwrap_or(Value::Null),
        ),
        ("estimated_arrival", json_i64(arrival.estimated)),
        (
            "estimated_arrival_local",
            arrival
                .estimated
                .map(|unix| json!(hhmm(unix, tz)))
                .unwrap_or(Value::Null),
        ),
        ("arrival_delay_sec", json_i32(arrival.delay_sec)),
        ("scheduled_departure", json_i64(sched_dep)),
        (
            "scheduled_departure_local",
            sched_dep
                .map(|unix| json!(hhmm(unix, tz)))
                .unwrap_or(Value::Null),
        ),
        ("estimated_departure", json_i64(departure.estimated)),
        (
            "estimated_departure_local",
            departure
                .estimated
                .map(|unix| json!(hhmm(unix, tz)))
                .unwrap_or(Value::Null),
        ),
        ("departure_delay_sec", json_i32(departure.delay_sec)),
        ("delay_sec", json_i32(delay)),
        (
            "delay_label",
            delay
                .map(|sec| json!(delay_label(sec)))
                .unwrap_or(Value::Null),
        ),
        (
            "schedule_relationship",
            json_opt(relationship.or(trip_relationship_name)),
        ),
        ("skipped", if skipped { json!(true) } else { Value::Null }),
        ("canceled", if canceled { json!(true) } else { Value::Null }),
        ("assigned_stop_id", json_opt(assigned)),
        ("platform_code", json_opt(platform.clone())),
        ("track", json_opt(platform)),
    ])
}

/// Renders one vehicle feed entity.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `entity` - [`crate::gtfs_rt::FeedEntity`] that should carry a vehicle position.
/// * `stops` - Stops already loaded for this page, keyed by stop id.
///
/// # Returns
///
/// The vehicle JSON, plus `entity_id`. An entity with no vehicle returns only `id`.
fn vehicle_row(db: &StaticDb, entity: &FeedEntity, stops: &HashMap<String, StopRow>) -> Value {
    let Some(vehicle) = entity.vehicle.as_ref() else {
        return obj([("id", json!(entity.id))]);
    };
    let mut row = vehicle_position_json(db, vehicle, stops);
    if let Some(object) = row.as_object_mut() {
        object.insert("entity_id".into(), json!(entity.id));
    }
    row
}

/// Renders one [`crate::gtfs_rt::VehiclePosition`] with static trip and stop names.
///
/// Coordinates pass through [`crate::realtime::round_coord`]. Bearing and speed are rounded to one decimal place.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `vehicle` - Realtime position.
/// * `stops` - Prefetched stops keyed by stop id. A miss falls back to one stop lookup.
///
/// # Returns
///
/// JSON for the position. Null fields are omitted.
fn vehicle_position_json(
    db: &StaticDb,
    vehicle: &VehiclePosition,
    stops: &HashMap<String, StopRow>,
) -> Value {
    let stop = vehicle.stop_id.as_ref().and_then(|id| {
        stops
            .get(id)
            .cloned()
            .or_else(|| db.stop_by_id(id).ok().flatten())
    });
    let trip_id = vehicle.trip.as_ref().and_then(|t| t.trip_id.clone());
    let static_trip = trip_id
        .as_deref()
        .and_then(|id| db.trip_by_id(id).ok().flatten());
    obj([
        ("trip_id", json_opt(trip_id)),
        (
            "trip_short_name",
            json_opt(static_trip.as_ref().and_then(|t| t.short_name.clone())),
        ),
        (
            "headsign",
            json_opt(static_trip.as_ref().and_then(|t| t.headsign.clone())),
        ),
        (
            "route_id",
            json_opt(
                vehicle
                    .trip
                    .as_ref()
                    .and_then(|t| t.route_id.clone())
                    .or_else(|| static_trip.as_ref().map(|t| t.route_id.clone())),
            ),
        ),
        (
            "route_short_name",
            json_opt(
                static_trip
                    .as_ref()
                    .and_then(|t| t.route_short_name.clone()),
            ),
        ),
        ("vehicle", vehicle_json(vehicle.vehicle.as_ref())),
        (
            "lat",
            vehicle
                .position
                .as_ref()
                .map(|p| json!(round_coord(p.latitude)))
                .unwrap_or(Value::Null),
        ),
        (
            "lon",
            vehicle
                .position
                .as_ref()
                .map(|p| json!(round_coord(p.longitude)))
                .unwrap_or(Value::Null),
        ),
        (
            "bearing",
            vehicle
                .position
                .as_ref()
                .and_then(|p| p.bearing)
                .map(|v| json!((f64::from(v) * 10.0).round() / 10.0))
                .unwrap_or(Value::Null),
        ),
        (
            "speed_mps",
            vehicle
                .position
                .as_ref()
                .and_then(|p| p.speed)
                .map(|v| json!((f64::from(v) * 10.0).round() / 10.0))
                .unwrap_or(Value::Null),
        ),
        (
            "status",
            json_opt(vehicle_status(
                vehicle.current_status,
                vehicle.current_stop_sequence.is_some(),
            )),
        ),
        (
            "stop_sequence",
            json_i64(vehicle.current_stop_sequence.map(|v| v as i64)),
        ),
        ("stop_id", json_opt(vehicle.stop_id.clone())),
        (
            "stop_name",
            json_opt(stop.as_ref().and_then(|s| s.stop_name.clone())),
        ),
        (
            "platform_code",
            json_opt(stop.as_ref().and_then(|s| s.platform_code.clone())),
        ),
        ("timestamp", json_i64(vehicle.timestamp.map(|v| v as i64))),
        (
            "occupancy",
            json_opt(occupancy_name(vehicle.occupancy_status)),
        ),
        (
            "occupancy_percentage",
            json_i64(vehicle.occupancy_percentage.map(|v| v as i64)),
        ),
    ])
}

/// Renders one trip-update entity and caps its stop list.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]), used for names and assigned platforms.
/// * `entity` - [`crate::gtfs_rt::FeedEntity`] that should carry a trip update.
/// * `stop_limit` - Maximum stop-time updates to include.
///
/// # Returns
///
/// JSON for the trip update. `stop_updates_omitted` is set when the cap drops rows. An entity with no trip update returns only `id`.
fn trip_update_row(db: &StaticDb, entity: &FeedEntity, stop_limit: usize) -> Value {
    let Some(update) = entity.trip_update.as_ref() else {
        return obj([("id", json!(entity.id))]);
    };
    let trip_id = update.trip.trip_id.clone();
    let static_trip = trip_id
        .as_deref()
        .and_then(|id| db.trip_by_id(id).ok().flatten());
    let total = update.stop_time_update.len();
    let shown = update.stop_time_update.iter().take(stop_limit);
    let stops: Vec<Value> = shown
        .map(|row| {
            let assigned = row
                .stop_time_properties
                .as_ref()
                .and_then(|props| props.assigned_stop_id.clone());
            let platform = assigned
                .as_deref()
                .and_then(|id| db.stop_by_id(id).ok().flatten())
                .and_then(|stop| stop.platform_code);
            let stop_name = row
                .stop_id
                .as_deref()
                .and_then(|id| db.stop_by_id(id).ok().flatten())
                .and_then(|stop| stop.stop_name);
            obj([
                (
                    "stop_sequence",
                    json_i64(row.stop_sequence.map(|v| v as i64)),
                ),
                ("stop_id", json_opt(row.stop_id.clone())),
                ("stop_name", json_opt(stop_name)),
                (
                    "arrival_delay_sec",
                    json_i32(row.arrival.as_ref().and_then(|e| e.delay)),
                ),
                (
                    "arrival_time",
                    json_i64(row.arrival.as_ref().and_then(|e| e.time)),
                ),
                (
                    "departure_delay_sec",
                    json_i32(row.departure.as_ref().and_then(|e| e.delay)),
                ),
                (
                    "departure_time",
                    json_i64(row.departure.as_ref().and_then(|e| e.time)),
                ),
                (
                    "schedule_relationship",
                    json_opt(stop_relationship(row.schedule_relationship)),
                ),
                ("assigned_stop_id", json_opt(assigned)),
                ("platform_code", json_opt(platform.clone())),
                ("track", json_opt(platform)),
            ])
        })
        .collect();
    obj([
        ("entity_id", json!(entity.id)),
        ("trip_id", json_opt(trip_id)),
        (
            "trip_short_name",
            json_opt(static_trip.as_ref().and_then(|t| t.short_name.clone())),
        ),
        (
            "headsign",
            json_opt(static_trip.as_ref().and_then(|t| t.headsign.clone())),
        ),
        (
            "route_id",
            json_opt(
                update
                    .trip
                    .route_id
                    .clone()
                    .or_else(|| static_trip.as_ref().map(|t| t.route_id.clone())),
            ),
        ),
        (
            "route_short_name",
            json_opt(
                static_trip
                    .as_ref()
                    .and_then(|t| t.route_short_name.clone()),
            ),
        ),
        (
            "schedule_relationship",
            json_opt(trip_relationship(update.trip.schedule_relationship)),
        ),
        ("start_date", json_opt(update.trip.start_date.clone())),
        ("delay_sec", json_i32(update.delay)),
        ("vehicle", vehicle_json(update.vehicle.as_ref())),
        ("stop_update_count", json!(total)),
        (
            "stop_updates_omitted",
            if total > stops.len() {
                json!(total - stops.len())
            } else {
                Value::Null
            },
        ),
        ("stop_updates", Value::Array(stops)),
    ])
}

/// Renders one alert entity.
///
/// The header and the description are clipped to `max_chars`. The URL is clipped to 500 characters.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]), used to name routes on informed entities.
/// * `entity` - [`crate::gtfs_rt::FeedEntity`] that should carry an alert.
/// * `max_chars` - Maximum characters kept from the header and the description.
///
/// # Returns
///
/// JSON for the alert. An entity with no alert returns only `id`.
#[allow(deprecated)]
fn alert_row(db: &StaticDb, entity: &FeedEntity, max_chars: usize) -> Value {
    let Some(alert) = entity.alert.as_ref() else {
        return obj([("id", json!(entity.id))]);
    };
    let (header, header_truncated) =
        translated(alert.header_text.as_ref(), max_chars).unwrap_or_default();
    let (description, description_truncated) =
        translated(alert.description_text.as_ref(), max_chars).unwrap_or_default();
    let (url, _) = translated(alert.url.as_ref(), 500).unwrap_or_default();
    let informed: Vec<Value> = alert
        .informed_entity
        .iter()
        .map(|selector| {
            let route = selector
                .route_id
                .as_deref()
                .and_then(|id| db.route(id).ok().flatten());
            obj([
                ("agency_id", json_opt(selector.agency_id.clone())),
                ("route_id", json_opt(selector.route_id.clone())),
                (
                    "route_short_name",
                    json_opt(route.as_ref().and_then(|r| r.short_name.clone())),
                ),
                (
                    "route_long_name",
                    json_opt(route.as_ref().and_then(|r| r.long_name.clone())),
                ),
                (
                    "route_type",
                    json_i64(selector.route_type.map(|v| v as i64)),
                ),
                ("stop_id", json_opt(selector.stop_id.clone())),
                (
                    "trip_id",
                    json_opt(selector.trip.as_ref().and_then(|t| t.trip_id.clone())),
                ),
                (
                    "direction_id",
                    json_i64(selector.direction_id.map(|v| v as i64)),
                ),
            ])
        })
        .collect();
    let periods: Vec<Value> = alert_periods(alert)
        .into_iter()
        .map(|range| {
            obj([
                ("start", json_i64(range.start.map(|v| v as i64))),
                ("end", json_i64(range.end.map(|v| v as i64))),
            ])
        })
        .collect();
    obj([
        ("id", json!(entity.id)),
        (
            "header",
            if header.is_empty() {
                Value::Null
            } else {
                json!(header)
            },
        ),
        (
            "header_truncated",
            if header_truncated {
                json!(true)
            } else {
                Value::Null
            },
        ),
        (
            "description",
            if description.is_empty() {
                Value::Null
            } else {
                json!(description)
            },
        ),
        (
            "description_truncated",
            if description_truncated {
                json!(true)
            } else {
                Value::Null
            },
        ),
        (
            "url",
            if url.is_empty() {
                Value::Null
            } else {
                json!(url)
            },
        ),
        ("cause", json_opt(cause_name(alert.cause))),
        ("effect", json_opt(effect_name(alert.effect))),
        ("severity", json_opt(severity_name(alert.severity_level))),
        ("informed_entity", Value::Array(informed)),
        (
            "active_period",
            if periods.is_empty() {
                Value::Null
            } else {
                Value::Array(periods)
            },
        ),
    ])
}

/// Renders one feed entity for [`decode_message`].
///
/// `kind` is `trip_update`, `vehicle`, `alert`, `shape`, `stop`, `trip_modifications`, or `unknown`. `is_deleted` is set when the entity is a deletion. Trip updates include at most 40 stops.
///
/// # Arguments
///
/// * `entity` - [`crate::gtfs_rt::FeedEntity`] to render.
/// * `max_chars` - Maximum characters kept from alert text.
///
/// # Returns
///
/// JSON for that entity. Null fields are omitted.
fn decode_entity(entity: &FeedEntity, max_chars: usize) -> Value {
    let kind = if entity.trip_update.is_some() {
        "trip_update"
    } else if entity.vehicle.is_some() {
        "vehicle"
    } else if entity.alert.is_some() {
        "alert"
    } else if entity.shape.is_some() {
        "shape"
    } else if entity.stop.is_some() {
        "stop"
    } else if entity.trip_modifications.is_some() {
        "trip_modifications"
    } else {
        "unknown"
    };
    let mut value = obj([
        ("id", json!(entity.id)),
        ("kind", json!(kind)),
        (
            "is_deleted",
            if entity.is_deleted == Some(true) {
                json!(true)
            } else {
                Value::Null
            },
        ),
    ]);
    if let Some(object) = value.as_object_mut() {
        if let Some(update) = entity.trip_update.as_ref() {
            let stops: Vec<Value> = update
                .stop_time_update
                .iter()
                .take(40)
                .map(|row| {
                    obj([
                        (
                            "stop_sequence",
                            json_i64(row.stop_sequence.map(|v| v as i64)),
                        ),
                        ("stop_id", json_opt(row.stop_id.clone())),
                        (
                            "arrival_delay_sec",
                            json_i32(row.arrival.as_ref().and_then(|e| e.delay)),
                        ),
                        (
                            "departure_delay_sec",
                            json_i32(row.departure.as_ref().and_then(|e| e.delay)),
                        ),
                        (
                            "schedule_relationship",
                            json_opt(stop_relationship(row.schedule_relationship)),
                        ),
                        (
                            "assigned_stop_id",
                            json_opt(
                                row.stop_time_properties
                                    .as_ref()
                                    .and_then(|props| props.assigned_stop_id.clone()),
                            ),
                        ),
                    ])
                })
                .collect();
            object.insert(
                "trip_update".into(),
                obj([
                    ("trip_id", json_opt(update.trip.trip_id.clone())),
                    ("route_id", json_opt(update.trip.route_id.clone())),
                    (
                        "schedule_relationship",
                        json_opt(trip_relationship(update.trip.schedule_relationship)),
                    ),
                    ("delay_sec", json_i32(update.delay)),
                    ("stop_updates", Value::Array(stops)),
                ]),
            );
        }
        if let Some(vehicle) = entity.vehicle.as_ref() {
            object.insert(
                "vehicle".into(),
                obj([
                    (
                        "trip_id",
                        json_opt(vehicle.trip.as_ref().and_then(|t| t.trip_id.clone())),
                    ),
                    (
                        "route_id",
                        json_opt(vehicle.trip.as_ref().and_then(|t| t.route_id.clone())),
                    ),
                    ("stop_id", json_opt(vehicle.stop_id.clone())),
                    (
                        "status",
                        json_opt(vehicle_status(
                            vehicle.current_status,
                            vehicle.current_stop_sequence.is_some(),
                        )),
                    ),
                    (
                        "lat",
                        vehicle
                            .position
                            .as_ref()
                            .map(|p| json!(round_coord(p.latitude)))
                            .unwrap_or(Value::Null),
                    ),
                    (
                        "lon",
                        vehicle
                            .position
                            .as_ref()
                            .map(|p| json!(round_coord(p.longitude)))
                            .unwrap_or(Value::Null),
                    ),
                    ("timestamp", json_i64(vehicle.timestamp.map(|v| v as i64))),
                    ("vehicle", vehicle_json(vehicle.vehicle.as_ref())),
                ]),
            );
        }
        if entity.alert.is_some() {
            let (header, truncated) = entity
                .alert
                .as_ref()
                .and_then(|alert| translated(alert.header_text.as_ref(), max_chars))
                .unwrap_or_default();
            let (description, description_truncated) = entity
                .alert
                .as_ref()
                .and_then(|alert| translated(alert.description_text.as_ref(), max_chars))
                .unwrap_or_default();
            object.insert(
                "alert".into(),
                obj([
                    (
                        "header",
                        if header.is_empty() {
                            Value::Null
                        } else {
                            json!(header)
                        },
                    ),
                    (
                        "header_truncated",
                        if truncated { json!(true) } else { Value::Null },
                    ),
                    (
                        "description",
                        if description.is_empty() {
                            Value::Null
                        } else {
                            json!(description)
                        },
                    ),
                    (
                        "description_truncated",
                        if description_truncated {
                            json!(true)
                        } else {
                            Value::Null
                        },
                    ),
                    (
                        "cause",
                        json_opt(entity.alert.as_ref().and_then(|a| cause_name(a.cause))),
                    ),
                    (
                        "effect",
                        json_opt(entity.alert.as_ref().and_then(|a| effect_name(a.effect))),
                    ),
                ]),
            );
        }
    }
    value
}

/// Wraps a page of rows with the feed timestamp and paging fields.
///
/// # Arguments
///
/// * `feed` - Source [`crate::gtfs_rt::FeedMessage`], used for the header timestamp and feed version.
/// * `key` - JSON field that holds `rows`.
/// * `rows` - Page of already rendered entities.
/// * `total` - Match count before paging.
/// * `offset` - Number of matches skipped.
///
/// # Returns
///
/// JSON with `total`, `offset`, `returned`, and the page array stored under `key`. Header fields and `next_offset` are included when those values exist.
fn page_object(
    feed: &FeedMessage,
    key: &'static str,
    rows: Vec<Value>,
    total: usize,
    offset: usize,
) -> Value {
    let next = offset + rows.len();
    obj([
        (
            "header_timestamp",
            json_i64(feed.header.timestamp.map(|v| v as i64)),
        ),
        ("feed_version", json_opt(feed.header.feed_version.clone())),
        ("total", json!(total)),
        ("offset", json!(offset)),
        ("returned", json!(rows.len())),
        (
            "next_offset",
            if next < total {
                json!(next)
            } else {
                Value::Null
            },
        ),
        (key, Value::Array(rows)),
    ])
}

/// Resolves a trip id or a trip short name.
///
/// An exact trip id returns that one trip. Any other text returns every trip with that short name, which may be an empty list.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `query` - Trip id or short name.
///
/// # Returns
///
/// Matching [`TripRow`] values.
///
/// # Errors
///
/// * `query` is empty or only whitespace.
/// * SQLite fails while reading trips.
fn resolve_trips(db: &StaticDb, query: &str) -> Result<Vec<TripRow>, AppError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(err("trip is empty").hint("pass a trip_id or train number"));
    }
    if let Some(trip) = db.trip_by_id(query)? {
        return Ok(vec![trip]);
    }
    db.trips_by_short_name(query)
}

/// Turns an optional trip query into the trip ids to keep.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `query` - Trip id or short name. `None` and whitespace mean no filter.
///
/// # Returns
///
/// `None` when the filter is unset. The ids of matching trips when the static feed knows the query. A one-element set of the raw text when nothing matches, so a realtime-only id can still hit.
///
/// # Errors
///
/// SQLite fails while resolving the trip.
fn trip_filter(db: &StaticDb, query: Option<&str>) -> Result<Option<HashSet<String>>, AppError> {
    let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        return Ok(None);
    };
    let trips = resolve_trips(db, query)?;
    if trips.is_empty() {
        return Ok(Some(HashSet::from([query.to_string()])));
    }
    Ok(Some(trips.into_iter().map(|t| t.trip_id).collect()))
}

/// Turns an optional route query into the route ids to keep.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `query` - Route id, short name, or long name. `None` and whitespace mean no filter.
///
/// # Returns
///
/// `None` when the filter is unset. Resolved route ids when the static feed knows the query. A one-element vector of the raw text when nothing matches.
///
/// # Errors
///
/// SQLite fails while resolving the route.
fn route_filter(db: &StaticDb, query: Option<&str>) -> Result<Option<Vec<String>>, AppError> {
    let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        return Ok(None);
    };
    let ids = db.resolve_route_ids(query)?;
    if ids.is_empty() {
        Ok(Some(vec![query.to_string()]))
    } else {
        Ok(Some(ids))
    }
}

/// Reports whether an alert's informed entities hit a route filter.
///
/// A selector matches on its route id, its trip's route id, or the static route of its trip id.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]), used when a selector only names a trip id.
/// * `alert` - [`crate::gtfs_rt::Alert`] to test.
/// * `routes` - Route ids that pass the filter.
///
/// # Returns
///
/// `true` when any informed entity names one of `routes`.
fn alert_matches_route(db: &StaticDb, alert: &crate::gtfs_rt::Alert, routes: &[String]) -> bool {
    alert.informed_entity.iter().any(|selector| {
        if selector
            .route_id
            .as_ref()
            .is_some_and(|id| routes.iter().any(|route| route == id))
        {
            return true;
        }
        selector
            .trip
            .as_ref()
            .and_then(|trip| trip.route_id.as_ref())
            .is_some_and(|id| routes.iter().any(|route| route == id))
            || selector
                .trip
                .as_ref()
                .and_then(|trip| trip.trip_id.as_deref())
                .and_then(|id| db.trip_route_id(id).ok().flatten())
                .is_some_and(|id| routes.iter().any(|route| route == &id))
    })
}

/// Reports whether a trip update mentions a stop in the caller's set.
///
/// A stop-time update hits when its stop id or assigned stop id is in `stop_ids`, or when its stop sequence is one the static trip uses at that stop.
///
/// # Arguments
///
/// * `update` - [`crate::gtfs_rt::TripUpdate`] to test.
/// * `stop_ids` - Expanded stop ids for the query.
/// * `by_trip` - Static stop sequences keyed by trip id.
///
/// # Returns
///
/// `true` when any stop-time update hits.
fn update_hits_stop(
    update: &TripUpdate,
    stop_ids: &HashSet<String>,
    by_trip: &HashMap<String, HashSet<i64>>,
) -> bool {
    if update.stop_time_update.iter().any(|row| {
        row.stop_id.as_ref().is_some_and(|id| stop_ids.contains(id))
            || row
                .stop_time_properties
                .as_ref()
                .and_then(|props| props.assigned_stop_id.as_ref())
                .is_some_and(|id| stop_ids.contains(id))
    }) {
        return true;
    }
    let Some(trip_id) = update.trip.trip_id.as_deref() else {
        return false;
    };
    let Some(sequences) = by_trip.get(trip_id) else {
        return false;
    };
    update.stop_time_update.iter().any(|row| {
        row.stop_sequence
            .is_some_and(|seq| sequences.contains(&(seq as i64)))
    })
}

/// Indexes vehicle positions by trip id.
///
/// A later entity replaces an earlier one that repeats the same trip id. Positions without a trip id are skipped.
///
/// # Arguments
///
/// * `feed` - Vehicle [`crate::gtfs_rt::FeedMessage`]. `None` yields an empty map.
///
/// # Returns
///
/// Trip id to the [`crate::gtfs_rt::VehiclePosition`] that names it.
fn index_vehicles(feed: Option<&FeedMessage>) -> HashMap<&str, &VehiclePosition> {
    let mut map = HashMap::new();
    if let Some(feed) = feed {
        for entity in &feed.entity {
            if let Some(vehicle) = entity.vehicle.as_ref() {
                if let Some(trip_id) = vehicle.trip.as_ref().and_then(|t| t.trip_id.as_deref()) {
                    map.insert(trip_id, vehicle);
                }
            }
        }
    }
    map
}

/// Indexes trip updates by trip id.
///
/// A later entity replaces an earlier one that repeats the same trip id. Updates without a trip id are skipped.
///
/// # Arguments
///
/// * `feed` - Trip-update [`crate::gtfs_rt::FeedMessage`]. `None` yields an empty map.
///
/// # Returns
///
/// Trip id to the [`crate::gtfs_rt::TripUpdate`] that names it.
fn index_updates(feed: Option<&FeedMessage>) -> HashMap<&str, &TripUpdate> {
    let mut map = HashMap::new();
    if let Some(feed) = feed {
        for entity in &feed.entity {
            if let Some(update) = entity.trip_update.as_ref() {
                if let Some(trip_id) = update.trip.trip_id.as_deref() {
                    map.insert(trip_id, update);
                }
            }
        }
    }
    map
}

/// Names the trip-level schedule relationship on an update.
///
/// # Arguments
///
/// * `update` - [`crate::gtfs_rt::TripUpdate`], when the feed has one for this trip.
///
/// # Returns
///
/// The spec name, or `None` when there is no update or the discriminant is unknown.
fn trip_rel_of(update: Option<&TripUpdate>) -> Option<String> {
    update.and_then(|update| trip_relationship(update.trip.schedule_relationship))
}

/// Ranks one trip on one service date against the query clock.
///
/// A live vehicle adds 1000 and a trip update adds 200. A canceled trip subtracts 50. A clock inside the trip's scheduled span, padded by ten minutes, adds 300. Each minute outside that span subtracts one point.
///
/// # Arguments
///
/// * `at` - Query clock, as Unix seconds.
/// * `tz` - Agency timezone.
/// * `date` - Service date being scored.
/// * `stops` - Static stop times for the trip ([`StopTimeRow`]).
/// * `vehicle` - Live [`crate::gtfs_rt::VehiclePosition`], when present.
/// * `update` - [`crate::gtfs_rt::TripUpdate`], when present.
///
/// # Returns
///
/// A score. Higher is a closer match.
fn score_candidate(
    at: i64,
    tz: Tz,
    date: NaiveDate,
    stops: &[StopTimeRow],
    vehicle: Option<&VehiclePosition>,
    update: Option<&TripUpdate>,
) -> i64 {
    let mut score = 0i64;
    if vehicle.is_some() {
        score += 1000;
    }
    if update.is_some() {
        score += 200;
    }
    if update.is_some_and(|update| is_canceled(update.trip.schedule_relationship)) {
        score -= 50;
    }
    if let (Some(first), Some(last)) = (stops.first(), stops.last()) {
        let start = first
            .arrival_sec
            .or(first.departure_sec)
            .map(|sec| gtfs_unix(date, sec, tz));
        let end = last
            .departure_sec
            .or(last.arrival_sec)
            .map(|sec| gtfs_unix(date, sec, tz));
        if let (Some(start), Some(end)) = (start, end) {
            if at >= start - 600 && at <= end + 600 {
                score += 300;
            }
            let distance = if at < start {
                start - at
            } else if at > end {
                at - end
            } else {
                0
            };
            score -= distance / 60;
        }
    }
    score
}

/// Collects the active service dates around the query clock.
///
/// # Arguments
///
/// * `db` - Static GTFS database ([`StaticDb`]).
/// * `service_id` - Calendar id from the trip.
/// * `at` - Query clock, as Unix seconds.
///
/// # Returns
///
/// Yesterday when that service runs, then today when that service runs. Empty when neither date is active.
fn service_dates(db: &StaticDb, service_id: &str, at: i64) -> Vec<NaiveDate> {
    let today = local_date(at, db.tz);
    let mut dates = Vec::new();
    if let Some(yesterday) = today.pred_opt() {
        if db.service_active(service_id, yesterday) {
            dates.push(yesterday);
        }
    }
    if db.service_active(service_id, today) {
        dates.push(today);
    }
    dates
}

/// Calendar date of a Unix instant in an agency timezone.
///
/// # Arguments
///
/// * `at` - Unix seconds.
/// * `tz` - Agency timezone.
///
/// # Returns
///
/// The local [`NaiveDate`].
fn local_date(at: i64, tz: Tz) -> NaiveDate {
    at_datetime(at, tz).date_naive()
}

/// Converts Unix seconds to a zoned datetime.
///
/// An instant chrono cannot represent becomes the Unix epoch in `tz`.
///
/// # Arguments
///
/// * `at` - Unix seconds.
/// * `tz` - Timezone to apply.
///
/// # Returns
///
/// The zoned [`DateTime`].
fn at_datetime(at: i64, tz: Tz) -> DateTime<Tz> {
    DateTime::from_timestamp(at, 0)
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap())
        .with_timezone(&tz)
}

/// Unix seconds of local midnight on a service date.
///
/// During a fold, the earlier offset is used. When that local midnight cannot be placed, the result is `0`.
///
/// # Arguments
///
/// * `date` - Civil date in `tz`.
/// * `tz` - Agency timezone.
///
/// # Returns
///
/// Unix seconds of midnight.
fn midnight_unix(date: NaiveDate, tz: Tz) -> i64 {
    let naive = date.and_hms_opt(0, 0, 0).unwrap();
    tz.from_local_datetime(&naive)
        .single()
        .or_else(|| tz.from_local_datetime(&naive).earliest())
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

/// Places a GTFS time-of-day on a service date.
///
/// `sec` is seconds since local midnight and may pass 24 hours, which is how GTFS encodes trips after midnight.
///
/// # Arguments
///
/// * `date` - Service date.
/// * `sec` - Seconds since local midnight.
/// * `tz` - Agency timezone.
///
/// # Returns
///
/// Unix seconds for that scheduled instant.
fn gtfs_unix(date: NaiveDate, sec: i64, tz: Tz) -> i64 {
    midnight_unix(date, tz) + sec
}

/// Formats a Unix instant as `HH:MM` in an agency timezone.
///
/// # Arguments
///
/// * `unix` - Unix seconds.
/// * `tz` - Agency timezone.
///
/// # Returns
///
/// A 24-hour `HH:MM` string. Seconds are dropped.
fn hhmm(unix: i64, tz: Tz) -> String {
    let dt = at_datetime(unix, tz);
    format!("{:02}:{:02}", dt.hour(), dt.minute())
}
