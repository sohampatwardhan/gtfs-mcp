//! In-memory Harbor Line GTFS for tests and local demos.
//!
//! [`materialize`] writes a static zip and GTFS-Realtime protobufs into a
//! directory the caller chooses. [`static_zip_bytes`] builds the static zip
//! without touching the filesystem. [`reference_unix`] is the sample clock
//! those protobufs are stamped with. Nothing in this module opens a network
//! connection.

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use prost::Message;
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;
use zip::ZipWriter;

use crate::gtfs_rt::alert::{Cause, Effect, SeverityLevel};
use crate::gtfs_rt::feed_header::Incrementality;
use crate::gtfs_rt::translated_string::Translation;
use crate::gtfs_rt::trip_update::stop_time_update::ScheduleRelationship;
use crate::gtfs_rt::trip_update::{StopTimeEvent, StopTimeUpdate};
use crate::gtfs_rt::vehicle_position::VehicleStopStatus;
use crate::gtfs_rt::{
    EntitySelector, FeedEntity, FeedHeader, FeedMessage, Position, TimeRange, TranslatedString,
    TripDescriptor, TripUpdate, VehicleDescriptor, VehiclePosition,
};

/// Unix timestamp for 2026-01-15 16:30:00 UTC (11:30 in America/New_York).
///
/// Trip updates, vehicle positions, and alerts in this module are stamped with
/// this instant. Tests pass it as `at_unix` so results do not depend on the
/// wall clock.
///
/// # Examples
///
/// ```
/// let at = gtfs_mcp::fixtures::reference_unix();
/// assert_eq!(at, 1_768_494_600);
/// ```
///
/// # Returns
///
/// Seconds since the Unix epoch.
///
/// # Panics
///
/// Panics if that civil time is not a single UTC instant.
pub fn reference_unix() -> i64 {
    chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 1, 15, 16, 30, 0)
        .unwrap()
        .timestamp()
}

/// Paths and clock for one Harbor Line sample written by [`materialize`].
#[derive(Debug)]
pub struct HarborPaths {
    /// Directory that holds the sample files.
    pub dir: PathBuf,
    /// Static GTFS zip, `static.zip` under [`Self::dir`].
    pub static_zip: PathBuf,
    /// TripUpdates protobuf, `trip-updates.pb` under [`Self::dir`].
    pub trip_updates: PathBuf,
    /// VehiclePositions protobuf, `vehicle-positions.pb` under [`Self::dir`].
    pub vehicle_positions: PathBuf,
    /// Alerts protobuf, `alerts.pb` under [`Self::dir`].
    pub alerts: PathBuf,
    /// Sample clock. Equal to [`reference_unix`].
    pub at_unix: i64,
}

/// Writes a Harbor Line static zip and realtime protobufs into `dir`.
///
/// Creates `dir` when it is missing. The zip is `static.zip`. Realtime files
/// are `trip-updates.pb`, `vehicle-positions.pb`, and `alerts.pb`, each stamped
/// with [`reference_unix`].
///
/// # Arguments
///
/// * `dir` - Directory to create and fill. Files with these names are replaced.
///
/// # Returns
///
/// Paths of the written files and the sample clock.
///
/// # Errors
///
/// Returns [`std::io::Error`] when the directory cannot be created or a sample
/// file cannot be written.
pub fn materialize(dir: &Path) -> std::io::Result<HarborPaths> {
    fs::create_dir_all(dir)?;
    let static_zip = dir.join("static.zip");
    write_static_zip(&static_zip)?;
    let at = reference_unix();
    let trip_updates = dir.join("trip-updates.pb");
    let vehicle_positions = dir.join("vehicle-positions.pb");
    let alerts = dir.join("alerts.pb");
    fs::write(&trip_updates, trip_updates_bytes(at))?;
    fs::write(&vehicle_positions, vehicle_bytes(at))?;
    fs::write(&alerts, alerts_bytes(at))?;
    Ok(HarborPaths {
        dir: dir.to_path_buf(),
        static_zip,
        trip_updates,
        vehicle_positions,
        alerts,
        at_unix: at,
    })
}

/// Writes the Harbor Line GTFS text tables into a zip at `path`.
///
/// Entries use the stored method. The archive contains agency, stops, routes,
/// trips, stop times, calendar, calendar dates, and feed info.
///
/// # Arguments
///
/// * `path` - Destination zip. The parent directory must already exist.
///
/// # Returns
///
/// `Ok(())` after the archive is flushed.
///
/// # Errors
///
/// Returns [`std::io::Error`] when the file cannot be created or a zip entry
/// cannot be written.
fn write_static_zip(path: &Path) -> std::io::Result<()> {
    let file = fs::File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, body) in [
        (
            "agency.txt",
            include_str!("../tests/fixtures/harbor/agency.txt"),
        ),
        (
            "stops.txt",
            include_str!("../tests/fixtures/harbor/stops.txt"),
        ),
        (
            "routes.txt",
            include_str!("../tests/fixtures/harbor/routes.txt"),
        ),
        (
            "trips.txt",
            include_str!("../tests/fixtures/harbor/trips.txt"),
        ),
        (
            "stop_times.txt",
            include_str!("../tests/fixtures/harbor/stop_times.txt"),
        ),
        (
            "calendar.txt",
            include_str!("../tests/fixtures/harbor/calendar.txt"),
        ),
        (
            "calendar_dates.txt",
            include_str!("../tests/fixtures/harbor/calendar_dates.txt"),
        ),
        (
            "feed_info.txt",
            include_str!("../tests/fixtures/harbor/feed_info.txt"),
        ),
    ] {
        zip.start_file(name, opts)?;
        zip.write_all(body.as_bytes())?;
    }
    zip.finish()?.flush()?;
    Ok(())
}

/// Builds a full-dataset GTFS-Realtime header stamped at `at`.
///
/// # Arguments
///
/// * `at` - Header timestamp, in Unix seconds.
///
/// # Returns
///
/// A version `2.0` header with feed version `harbor-1`.
fn header(at: i64) -> FeedHeader {
    FeedHeader {
        gtfs_realtime_version: "2.0".into(),
        incrementality: Some(Incrementality::FullDataset as i32),
        timestamp: Some(at as u64),
        feed_version: Some("harbor-1".into()),
    }
}

/// Encodes a TripUpdates message for trip `177-BOS` at `at`.
///
/// Stop sequence 2 is five minutes late. Stop sequence 3 arrives seven minutes
/// late, departs eight minutes late, and is assigned to `NHV:track:12`.
///
/// # Arguments
///
/// * `at` - Feed timestamp, in Unix seconds.
///
/// # Returns
///
/// Protobuf bytes of one [`crate::gtfs_rt::FeedMessage`].
fn trip_updates_bytes(at: i64) -> Vec<u8> {
    let update = TripUpdate {
        trip: TripDescriptor {
            trip_id: Some("177-BOS".into()),
            route_id: Some("NEC".into()),
            ..Default::default()
        },
        vehicle: Some(VehicleDescriptor {
            id: Some("AMT177".into()),
            label: Some("177".into()),
            ..Default::default()
        }),
        stop_time_update: vec![
            StopTimeUpdate {
                stop_sequence: Some(2),
                arrival: Some(StopTimeEvent {
                    delay: Some(300),
                    ..Default::default()
                }),
                departure: Some(StopTimeEvent {
                    delay: Some(300),
                    ..Default::default()
                }),
                schedule_relationship: Some(ScheduleRelationship::Scheduled as i32),
                ..Default::default()
            },
            StopTimeUpdate {
                stop_sequence: Some(3),
                arrival: Some(StopTimeEvent {
                    delay: Some(420),
                    ..Default::default()
                }),
                departure: Some(StopTimeEvent {
                    delay: Some(480),
                    ..Default::default()
                }),
                schedule_relationship: Some(ScheduleRelationship::Scheduled as i32),
                stop_time_properties: Some(
                    crate::gtfs_rt::trip_update::stop_time_update::StopTimeProperties {
                        assigned_stop_id: Some("NHV:track:12".into()),
                        ..Default::default()
                    },
                ),
                ..Default::default()
            },
        ],
        timestamp: Some(at as u64),
        ..Default::default()
    };
    FeedMessage {
        header: header(at),
        entity: vec![FeedEntity {
            id: "177-BOS".into(),
            trip_update: Some(update),
            ..Default::default()
        }],
    }
    .encode_to_vec()
}

/// Encodes a VehiclePositions message for vehicle `AMT177` at `at`.
///
/// The vehicle is in transit to stop `NHV` near latitude 41.25 and longitude
/// -72.95, on trip `177-BOS`.
///
/// # Arguments
///
/// * `at` - Feed timestamp, in Unix seconds.
///
/// # Returns
///
/// Protobuf bytes of one [`crate::gtfs_rt::FeedMessage`].
fn vehicle_bytes(at: i64) -> Vec<u8> {
    let vehicle = VehiclePosition {
        trip: Some(TripDescriptor {
            trip_id: Some("177-BOS".into()),
            route_id: Some("NEC".into()),
            ..Default::default()
        }),
        vehicle: Some(VehicleDescriptor {
            id: Some("AMT177".into()),
            label: Some("177".into()),
            ..Default::default()
        }),
        position: Some(Position {
            latitude: 41.25,
            longitude: -72.95,
            bearing: Some(40.0),
            speed: Some(22.5),
            ..Default::default()
        }),
        current_stop_sequence: Some(3),
        stop_id: Some("NHV".into()),
        current_status: Some(VehicleStopStatus::InTransitTo as i32),
        timestamp: Some(at as u64),
        ..Default::default()
    };
    FeedMessage {
        header: header(at),
        entity: vec![FeedEntity {
            id: "AMT177".into(),
            vehicle: Some(vehicle),
            ..Default::default()
        }],
    }
    .encode_to_vec()
}

/// Encodes two service alerts whose active window contains `at`.
///
/// One alert names route `NEC` and stop `NYP`. The other names route `LOCAL`.
/// The window starts one hour before `at` and ends one day after.
///
/// # Arguments
///
/// * `at` - Feed timestamp, in Unix seconds.
///
/// # Returns
///
/// Protobuf bytes of one [`crate::gtfs_rt::FeedMessage`].
#[allow(deprecated)]
fn alerts_bytes(at: i64) -> Vec<u8> {
    let window = TimeRange {
        start: Some((at - 3600) as u64),
        end: Some((at + 86_400) as u64),
    };
    let nec = FeedEntity {
        id: "alert-nec".into(),
        alert: Some(crate::gtfs_rt::Alert {
            active_period: vec![window],
            informed_entity: vec![
                EntitySelector {
                    route_id: Some("NEC".into()),
                    ..Default::default()
                },
                EntitySelector {
                    stop_id: Some("NYP".into()),
                    ..Default::default()
                },
            ],
            cause: Some(Cause::Construction as i32),
            effect: Some(Effect::SignificantDelays as i32),
            header_text: Some(text("Track work between NYP and NHV")),
            description_text: Some(text(
                "Northeast Corridor trains may be delayed or reassigned to a different track at New Haven.",
            )),
            url: Some(text("https://example.test/nec")),
            severity_level: Some(SeverityLevel::Warning as i32),
            ..Default::default()
        }),
        ..Default::default()
    };
    let local = FeedEntity {
        id: "alert-local".into(),
        alert: Some(crate::gtfs_rt::Alert {
            active_period: vec![window],
            informed_entity: vec![EntitySelector {
                route_id: Some("LOCAL".into()),
                ..Default::default()
            }],
            cause: Some(Cause::OtherCause as i32),
            effect: Some(Effect::Detour as i32),
            header_text: Some(text("Local bus detour")),
            description_text: Some(text("Harbor Local is detouring off Chapel Street.")),
            severity_level: Some(SeverityLevel::Info as i32),
            ..Default::default()
        }),
        ..Default::default()
    };
    FeedMessage {
        header: header(at),
        entity: vec![nec, local],
    }
    .encode_to_vec()
}

/// Wraps `value` as one English GTFS-Realtime translation.
///
/// # Arguments
///
/// * `value` - Plain text stored on the translation.
///
/// # Returns
///
/// A [`crate::gtfs_rt::TranslatedString`] whose only translation is `value`
/// with language `en`.
fn text(value: &str) -> TranslatedString {
    TranslatedString {
        translation: vec![Translation {
            text: value.into(),
            language: Some("en".into()),
        }],
    }
}

/// Builds the Harbor Line static GTFS zip in memory.
///
/// The archive uses the stored method and includes agency, stops, routes,
/// trips, stop times, calendar, and feed info. It does not include calendar
/// dates and it does not write a file. [`materialize`] writes the full sample,
/// including calendar dates and the realtime protobufs.
///
/// # Examples
///
/// ```
/// let bytes = gtfs_mcp::fixtures::static_zip_bytes();
/// assert!(bytes.starts_with(b"PK"));
/// ```
///
/// # Returns
///
/// Zip bytes.
///
/// # Panics
///
/// Panics if an in-memory zip entry cannot be started or written.
#[allow(dead_code)]
pub fn static_zip_bytes() -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut cursor);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, body) in [
            (
                "agency.txt",
                include_str!("../tests/fixtures/harbor/agency.txt"),
            ),
            (
                "stops.txt",
                include_str!("../tests/fixtures/harbor/stops.txt"),
            ),
            (
                "routes.txt",
                include_str!("../tests/fixtures/harbor/routes.txt"),
            ),
            (
                "trips.txt",
                include_str!("../tests/fixtures/harbor/trips.txt"),
            ),
            (
                "stop_times.txt",
                include_str!("../tests/fixtures/harbor/stop_times.txt"),
            ),
            (
                "calendar.txt",
                include_str!("../tests/fixtures/harbor/calendar.txt"),
            ),
            (
                "feed_info.txt",
                include_str!("../tests/fixtures/harbor/feed_info.txt"),
            ),
        ] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    cursor.into_inner()
}
