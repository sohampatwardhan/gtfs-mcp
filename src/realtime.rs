//! Decodes GTFS-Realtime protobufs into the values the JSON layer prints.
//!
//! [`decode_feed`] accepts a raw [`crate::gtfs_rt::FeedMessage`] or the same bytes
//! wrapped in gzip. Helpers name spec enums, clip translated text, and turn
//! stop-time events into estimated Unix timestamps. [`delay_label`], [`predict`],
//! [`clip`], and [`round_coord`] do not touch the network.

use std::io::Read;

use prost::Message;
use serde_json::{json, Map, Value};

use crate::error::{err, AppError};
use crate::gtfs_rt::feed_header::Incrementality;
use crate::gtfs_rt::trip_descriptor::ScheduleRelationship as TripRel;
use crate::gtfs_rt::trip_update::stop_time_update::ScheduleRelationship as StopRel;
use crate::gtfs_rt::vehicle_position::VehicleStopStatus;
use crate::gtfs_rt::{
    alert, vehicle_position, FeedMessage, TranslatedString, TripUpdate, VehicleDescriptor,
};

/// Inflates a gzip member, or copies bytes that are not gzip.
///
/// # Arguments
///
/// * `bytes` - Feed body. A leading `1f 8b` selects gzip.
///
/// # Returns
///
/// The uncompressed payload, or a copy of `bytes` when the gzip magic is absent.
///
/// # Errors
///
/// Returns [`AppError`] when the magic matches and the gzip decoder cannot finish the member.
pub fn maybe_gunzip(bytes: &[u8]) -> Result<Vec<u8>, AppError> {
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

/// Decodes one GTFS-Realtime [`crate::gtfs_rt::FeedMessage`], inflating gzip first.
///
/// # Arguments
///
/// * `bytes` - Protobuf body, optionally gzip-compressed.
///
/// # Returns
///
/// The decoded feed message.
///
/// # Errors
///
/// * The buffer is gzip and [`maybe_gunzip`] fails.
/// * The payload is empty after decompression.
/// * The bytes are not a `FeedMessage`.
pub fn decode_feed(bytes: &[u8]) -> Result<FeedMessage, AppError> {
    let raw = maybe_gunzip(bytes)?;
    if raw.is_empty() {
        return Err(err("protobuf payload is empty"));
    }
    Ok(FeedMessage::decode(raw.as_slice())?)
}

/// Maps a delay in seconds to a short label.
///
/// Magnitudes under 30 seconds are `on time`. Larger values round to the nearest minute. Positive seconds are late and negative seconds are early, so `0` is `on time`, `90` is `2 min late`, and `-90` is `2 min early`.
///
/// # Arguments
///
/// * `sec` - Delay in seconds.
///
/// # Returns
///
/// A label such as `on time`, `2 min late`, or `1 min early`.
///
/// # Panics
///
/// Debug builds panic when `sec` is [`i32::MIN`], because [`i32::abs`] overflows.
pub fn delay_label(sec: i32) -> String {
    if sec.abs() < 30 {
        return "on time".into();
    }
    let mins = (sec.abs() as f64 / 60.0).round() as i32;
    if mins == 0 {
        return if sec > 0 {
            format!("{sec} sec late")
        } else {
            format!("{} sec early", -sec)
        };
    }
    if sec > 0 {
        format!("{mins} min late")
    } else {
        format!("{mins} min early")
    }
}

/// Estimated instant and the delay used to produce it.
#[derive(Debug, Clone)]
pub struct Prediction {
    /// Unix seconds of the estimate, when a schedule or an absolute event time exists.
    pub estimated: Option<i64>,
    /// Seconds late when positive, early when negative, or `None` when unknown.
    pub delay_sec: Option<i32>,
}

/// Merges a scheduled instant with one realtime stop-time event.
///
/// When `event` is absent, the schedule is kept and the delay is absent. An absolute event time becomes the estimate. Its delay is the saturating difference from the schedule, or the event delay when the schedule is missing. A delay without a time is added to the schedule.
///
/// # Arguments
///
/// * `scheduled` - Scheduled Unix seconds from the static stop time.
/// * `event` - Arrival or departure [`crate::gtfs_rt::trip_update::StopTimeEvent`].
///
/// # Returns
///
/// A [`Prediction`] for that arrival or departure.
pub fn predict(
    scheduled: Option<i64>,
    event: Option<&crate::gtfs_rt::trip_update::StopTimeEvent>,
) -> Prediction {
    let Some(event) = event else {
        return Prediction {
            estimated: scheduled,
            delay_sec: None,
        };
    };
    if let Some(time) = event.time {
        let delay = scheduled
            .map(|sched| saturating_i32(time - sched))
            .or(event.delay);
        return Prediction {
            estimated: Some(time),
            delay_sec: delay,
        };
    }
    if let Some(delay) = event.delay {
        return Prediction {
            estimated: scheduled.map(|sched| sched + i64::from(delay)),
            delay_sec: Some(delay),
        };
    }
    Prediction {
        estimated: scheduled,
        delay_sec: None,
    }
}

/// Narrows a delay to `i32`, saturating at the extremes.
///
/// # Arguments
///
/// * `value` - Signed delay in seconds.
///
/// # Returns
///
/// `value` when it fits in `i32`, otherwise [`i32::MAX`] or [`i32::MIN`].
fn saturating_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value > 0 { i32::MAX } else { i32::MIN })
}

/// Names a trip schedule relationship.
///
/// # Arguments
///
/// * `value` - Raw [`crate::gtfs_rt::trip_descriptor::ScheduleRelationship`] discriminant.
///
/// # Returns
///
/// The spec name, such as `SCHEDULED` or `CANCELED`, or `None` when `value` is missing or unknown.
pub fn trip_relationship(value: Option<i32>) -> Option<String> {
    enum_label::<TripRel>(value)
}

/// Names a stop-time schedule relationship.
///
/// # Arguments
///
/// * `value` - Raw [`crate::gtfs_rt::trip_update::stop_time_update::ScheduleRelationship`] discriminant.
///
/// # Returns
///
/// The spec name, such as `SCHEDULED` or `SKIPPED`, or `None` when `value` is missing or unknown.
pub fn stop_relationship(value: Option<i32>) -> Option<String> {
    enum_label::<StopRel>(value)
}

/// Names a vehicle stop status.
///
/// An unset `value` with `has_sequence` becomes `IN_TRANSIT_TO`, the GTFS-Realtime default when a stop sequence is present.
///
/// # Arguments
///
/// * `value` - Raw [`crate::gtfs_rt::vehicle_position::VehicleStopStatus`] discriminant.
/// * `has_sequence` - Whether the position includes a current stop sequence.
///
/// # Returns
///
/// The spec name, or `None` when no status can be chosen.
pub fn vehicle_status(value: Option<i32>, has_sequence: bool) -> Option<String> {
    let value = value.or(if has_sequence {
        Some(VehicleStopStatus::InTransitTo as i32)
    } else {
        None
    });
    enum_label::<VehicleStopStatus>(value)
}

/// Names a feed header's incrementality.
///
/// An unset value becomes `FULL_DATASET`, the GTFS-Realtime default.
///
/// # Arguments
///
/// * `value` - Raw [`crate::gtfs_rt::feed_header::Incrementality`] discriminant.
///
/// # Returns
///
/// The spec name. Unknown discriminants yield `None`.
pub fn incrementality(value: Option<i32>) -> Option<String> {
    enum_label::<Incrementality>(value.or(Some(Incrementality::FullDataset as i32)))
}

/// Names an alert cause.
///
/// # Arguments
///
/// * `value` - Raw [`crate::gtfs_rt::alert::Cause`] discriminant.
///
/// # Returns
///
/// The spec name, or `None` when `value` is missing or unknown.
pub fn cause_name(value: Option<i32>) -> Option<String> {
    enum_label::<alert::Cause>(value)
}

/// Names an alert effect.
///
/// # Arguments
///
/// * `value` - Raw [`crate::gtfs_rt::alert::Effect`] discriminant.
///
/// # Returns
///
/// The spec name, or `None` when `value` is missing or unknown.
pub fn effect_name(value: Option<i32>) -> Option<String> {
    enum_label::<alert::Effect>(value)
}

/// Names an alert severity.
///
/// # Arguments
///
/// * `value` - Raw [`crate::gtfs_rt::alert::SeverityLevel`] discriminant.
///
/// # Returns
///
/// The spec name, or `None` when `value` is missing or unknown.
pub fn severity_name(value: Option<i32>) -> Option<String> {
    enum_label::<alert::SeverityLevel>(value)
}

/// Names a vehicle occupancy status.
///
/// # Arguments
///
/// * `value` - Raw [`crate::gtfs_rt::vehicle_position::OccupancyStatus`] discriminant.
///
/// # Returns
///
/// The spec name, or `None` when `value` is missing or unknown.
pub fn occupancy_name(value: Option<i32>) -> Option<String> {
    enum_label::<vehicle_position::OccupancyStatus>(value)
}

/// Maps a raw protobuf integer through [`Label`].
///
/// # Arguments
///
/// * `value` - Discriminant. `None` stays absent.
///
/// # Returns
///
/// The spec name, or `None` when `value` is missing or not a known enumerator.
fn enum_label<E>(value: Option<i32>) -> Option<String>
where
    E: TryFrom<i32> + Copy,
    E: Label,
{
    value
        .and_then(|v| E::try_from(v).ok())
        .map(|v| v.label().to_string())
}

/// Protobuf enum that can report its GTFS-Realtime name.
trait Label {
    /// Returns the spec name of this enumerator.
    ///
    /// # Arguments
    ///
    /// * `self` - Enumerator to name.
    ///
    /// # Returns
    ///
    /// A static name such as `SCHEDULED` or `FULL_DATASET`.
    fn label(self) -> &'static str;
}

/// Implements [`Label`] for one prost enum by calling `as_str_name`.
///
/// # Arguments
///
/// * `ty` - Path of a generated GTFS-Realtime enum that exposes `as_str_name`.
macro_rules! label_impl {
    ($ty:path) => {
        impl Label for $ty {
            /// Returns the spec name of this enumerator.
            ///
            /// # Arguments
            ///
            /// * `self` - Enumerator to name.
            ///
            /// # Returns
            ///
            /// The `as_str_name` string for `self`.
            fn label(self) -> &'static str {
                self.as_str_name()
            }
        }
    };
}

label_impl!(TripRel);
label_impl!(StopRel);
label_impl!(VehicleStopStatus);
label_impl!(Incrementality);
label_impl!(alert::Cause);
label_impl!(alert::Effect);
label_impl!(alert::SeverityLevel);
label_impl!(vehicle_position::OccupancyStatus);

/// Picks one translation and clips it to a character budget.
///
/// English (`en` or a tag starting with `en-`) wins, then a translation with no language, then the first translation.
///
/// # Arguments
///
/// * `text` - [`crate::gtfs_rt::TranslatedString`] from an alert, when present.
/// * `max_chars` - Maximum Unicode scalar values to keep.
///
/// # Returns
///
/// The clipped text and whether it was shortened, or `None` when `text` is missing or has no translation.
pub fn translated(text: Option<&TranslatedString>, max_chars: usize) -> Option<(String, bool)> {
    let text = text?;
    let chosen = text
        .translation
        .iter()
        .find(|t| {
            t.language.as_deref().is_some_and(|lang| {
                lang.eq_ignore_ascii_case("en") || lang.to_ascii_lowercase().starts_with("en-")
            })
        })
        .or_else(|| text.translation.iter().find(|t| t.language.is_none()))
        .or(text.translation.first())?;
    Some(clip(&chosen.text, max_chars))
}

/// Keeps at most `max_chars` Unicode scalar values.
///
/// # Arguments
///
/// * `text` - Source text.
/// * `max_chars` - Maximum number of characters to keep.
///
/// # Returns
///
/// The kept prefix and whether `text` was shortened. `clip("track", 3)` yields `("tra", true)`. Characters are counted, so `clip("café", 3)` yields `("caf", true)`.
pub fn clip(text: &str, max_chars: usize) -> (String, bool) {
    let count = text.chars().count();
    if count <= max_chars {
        (text.to_string(), false)
    } else {
        let clipped: String = text.chars().take(max_chars).collect();
        (clipped, true)
    }
}

/// Builds a JSON object for a vehicle descriptor.
///
/// # Arguments
///
/// * `vehicle` - [`crate::gtfs_rt::VehicleDescriptor`], when the entity has one.
///
/// # Returns
///
/// Null when `vehicle` is `None`. Otherwise an object whose `id`, `label`, and `license_plate` keys are omitted when unset.
pub fn vehicle_json(vehicle: Option<&VehicleDescriptor>) -> Value {
    let Some(vehicle) = vehicle else {
        return Value::Null;
    };
    obj([
        ("id", json_opt(vehicle.id.clone())),
        ("label", json_opt(vehicle.label.clone())),
        ("license_plate", json_opt(vehicle.license_plate.clone())),
    ])
}

/// Builds a JSON object and drops keys whose value is null.
///
/// # Arguments
///
/// * `pairs` - Key and value pairs. Keys are static strings.
///
/// # Returns
///
/// A JSON object containing only the non-null values.
pub fn obj(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        if !value.is_null() {
            map.insert(key.to_string(), value);
        }
    }
    Value::Object(map)
}

/// Converts an optional string into JSON.
///
/// # Arguments
///
/// * `value` - Text to encode.
///
/// # Returns
///
/// A JSON string, or null when `value` is `None`.
pub fn json_opt(value: Option<impl Into<String>>) -> Value {
    value
        .map(|v| Value::String(v.into()))
        .unwrap_or(Value::Null)
}

/// Converts an optional `i64` into JSON.
///
/// # Arguments
///
/// * `value` - Integer to encode.
///
/// # Returns
///
/// A JSON number, or null when `value` is `None`.
pub fn json_i64(value: Option<i64>) -> Value {
    value.map(|v| json!(v)).unwrap_or(Value::Null)
}

/// Converts an optional `i32` into JSON.
///
/// # Arguments
///
/// * `value` - Integer to encode.
///
/// # Returns
///
/// A JSON number, or null when `value` is `None`.
pub fn json_i32(value: Option<i32>) -> Value {
    value.map(|v| json!(v)).unwrap_or(Value::Null)
}

/// Converts an optional `f64` into JSON.
///
/// # Arguments
///
/// * `value` - Number to encode.
///
/// # Returns
///
/// A JSON number, or null when `value` is `None`.
pub fn json_f64(value: Option<f64>) -> Value {
    value.map(|v| json!(v)).unwrap_or(Value::Null)
}

/// Rounds a coordinate to five decimal places.
///
/// # Arguments
///
/// * `value` - Latitude or longitude in degrees.
///
/// # Returns
///
/// `value` as `f64`, rounded to 1e-5 degrees. `round_coord(12.345678)` is `12.34568`.
pub fn round_coord(value: f32) -> f64 {
    (f64::from(value) * 100_000.0).round() / 100_000.0
}

/// Reports whether a trip descriptor is canceled or deleted.
///
/// # Arguments
///
/// * `relationship` - Raw [`crate::gtfs_rt::trip_descriptor::ScheduleRelationship`] discriminant.
///
/// # Returns
///
/// `true` when `relationship` is `CANCELED` or `DELETED`.
pub fn is_canceled(relationship: Option<i32>) -> bool {
    matches!(
        relationship,
        Some(x) if x == TripRel::Canceled as i32 || x == TripRel::Deleted as i32
    )
}

/// Finds the stop-time update for one scheduled stop.
///
/// A matching `stop_sequence` wins. The `stop_id` is used when no sequence matches.
///
/// # Arguments
///
/// * `update` - Trip update to search.
/// * `stop_sequence` - Static `stop_sequence` for the stop.
/// * `stop_id` - Static stop id used as the fallback key.
///
/// # Returns
///
/// The matching [`crate::gtfs_rt::trip_update::StopTimeUpdate`], if any.
pub fn find_stop_update<'a>(
    update: &'a TripUpdate,
    stop_sequence: u32,
    stop_id: &str,
) -> Option<&'a crate::gtfs_rt::trip_update::StopTimeUpdate> {
    update
        .stop_time_update
        .iter()
        .find(|row| row.stop_sequence == Some(stop_sequence))
        .or_else(|| {
            update
                .stop_time_update
                .iter()
                .find(|row| row.stop_id.as_deref() == Some(stop_id))
        })
}

/// Finds the latest stop-time update before a sequence number.
///
/// Updates that omit `stop_sequence` are ignored.
///
/// # Arguments
///
/// * `update` - Trip update to search.
/// * `stop_sequence` - Exclusive upper bound.
///
/// # Returns
///
/// The update with the greatest sequence still below `stop_sequence`, if any.
pub fn previous_stop_update(
    update: &TripUpdate,
    stop_sequence: u32,
) -> Option<&crate::gtfs_rt::trip_update::StopTimeUpdate> {
    update
        .stop_time_update
        .iter()
        .filter(|row| row.stop_sequence.is_some_and(|seq| seq < stop_sequence))
        .max_by_key(|row| row.stop_sequence.unwrap_or(0))
}

/// Delay to carry forward when a stop has no exact update.
///
/// The previous stop's departure delay wins, then its arrival delay. A previous stop marked `NO_DATA` suppresses the trip-level delay. With no previous delay, the trip-level delay is used.
///
/// # Arguments
///
/// * `update` - Trip update that may carry a delay.
/// * `stop_sequence` - Sequence of the stop that needs a delay.
///
/// # Returns
///
/// Seconds late or early, or `None` when the previous stop is `NO_DATA` or no delay exists.
pub fn propagated_delay(update: &TripUpdate, stop_sequence: u32) -> Option<i32> {
    if let Some(previous) = previous_stop_update(update, stop_sequence) {
        if previous.schedule_relationship == Some(StopRel::NoData as i32) {
            return None;
        }
        if let Some(delay) = previous
            .departure
            .as_ref()
            .and_then(|event| event.delay)
            .or_else(|| previous.arrival.as_ref().and_then(|event| event.delay))
        {
            return Some(delay);
        }
    }
    update.delay
}

/// Borrows the time ranges that bound an alert.
///
/// `impact_period` is used when it is non-empty. Otherwise `active_period` is used.
///
/// # Arguments
///
/// * `alert` - [`crate::gtfs_rt::Alert`] to read.
///
/// # Returns
///
/// Borrowed [`crate::gtfs_rt::TimeRange`] values, empty when the alert lists neither.
#[allow(deprecated)]
pub fn alert_periods(alert: &crate::gtfs_rt::Alert) -> Vec<&crate::gtfs_rt::TimeRange> {
    if !alert.impact_period.is_empty() {
        alert.impact_period.iter().collect()
    } else {
        alert.active_period.iter().collect()
    }
}

/// Reports whether a Unix instant falls inside a time range.
///
/// `start` is inclusive and `end` is exclusive. A missing bound is open. A negative `at` is treated as zero so it can be compared with the protobuf's `u64` bounds.
///
/// # Arguments
///
/// * `range` - [`crate::gtfs_rt::TimeRange`] from an alert.
/// * `at` - Unix seconds to test.
///
/// # Returns
///
/// `true` when `at` is inside the range.
pub fn period_contains(range: &crate::gtfs_rt::TimeRange, at: i64) -> bool {
    let at = u64::try_from(at).unwrap_or(0);
    let starts = range.start.is_none_or(|start| at >= start);
    let ends = range.end.is_none_or(|end| at < end);
    starts && ends
}
