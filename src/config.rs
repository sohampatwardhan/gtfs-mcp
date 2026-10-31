//! Startup feed configuration from the process environment and CLI flags.
//!
//! Header values are parsed into [`RegisterRequest`] and are never copied into error text.
//! A missing static feed, or a malformed optional value, is a hard error: the process exits
//! before it serves MCP.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::catalog::{normalize_id, validate_startup_request, RegisterRequest};
use crate::source::validate_headers;

/// Placeholder used in the missing-static error. Reserved domain, not a live feed.
pub const STATIC_EXAMPLE: &str = "https://example.com/gtfs.zip";

#[derive(Debug, Clone, Default)]
pub struct StartupOverrides {
    pub feed_id: Option<String>,
    pub static_gtfs: Option<String>,
    pub trip_updates: Option<String>,
    pub vehicle_positions: Option<String>,
    pub alerts: Option<String>,
    pub rt_ttl_secs: Option<u64>,
    pub static_ttl_secs: Option<u64>,
    pub http_headers: Option<String>,
}

#[derive(Debug)]
pub struct ConfigError {
    pub message: String,
}

pub fn startup_plan(
    vars: impl IntoIterator<Item = (String, String)>,
    overrides: &StartupOverrides,
) -> Result<RegisterRequest, ConfigError> {
    let vars: HashMap<String, String> = vars
        .into_iter()
        .map(|(key, value)| (key, value.trim().to_string()))
        .filter(|(_, value)| !value.is_empty())
        .collect();

    let mut problems = Vec::new();

    let static_gtfs = pick(&overrides.static_gtfs, vars.get("GTFS_RT_MCP_STATIC"));
    if static_gtfs.is_none() {
        problems.push(format!(
            "GTFS_RT_MCP_STATIC is required (or pass --static). Example: GTFS_RT_MCP_STATIC={STATIC_EXAMPLE}"
        ));
    }

    let feed_id = pick(&overrides.feed_id, vars.get("GTFS_RT_MCP_FEED_ID"));
    if let Some(id) = feed_id.as_deref() {
        if let Err(error) = normalize_id(Some(id)) {
            problems.push(format!("GTFS_RT_MCP_FEED_ID: {}", error.error));
        }
    }

    let rt_ttl_secs = match ttl_from(
        overrides.rt_ttl_secs,
        vars.get("GTFS_RT_MCP_RT_TTL"),
        "GTFS_RT_MCP_RT_TTL",
        1,
        3600,
    ) {
        Ok(value) => value,
        Err(error) => {
            problems.push(error);
            None
        }
    };
    let static_ttl_secs = match ttl_from(
        overrides.static_ttl_secs,
        vars.get("GTFS_RT_MCP_STATIC_TTL"),
        "GTFS_RT_MCP_STATIC_TTL",
        0,
        7 * 24 * 3600,
    ) {
        Ok(value) => value,
        Err(error) => {
            problems.push(error);
            None
        }
    };

    let headers_raw = match &overrides.http_headers {
        Some(value) => Some(value.clone()),
        None => vars.get("GTFS_RT_MCP_HTTP_HEADERS").cloned(),
    };
    let headers = match headers_raw.as_deref() {
        Some(raw) => match parse_headers(raw) {
            Ok(headers) => headers,
            Err(error) => {
                problems.push(error);
                BTreeMap::new()
            }
        },
        None => BTreeMap::new(),
    };
    if let Err(error) = validate_headers(&headers) {
        problems.push(format!("GTFS_RT_MCP_HTTP_HEADERS: {}", error.error));
    }

    if !problems.is_empty() {
        return Err(ConfigError {
            message: problems.join("; "),
        });
    }

    let (static_url, static_path) = split_loc(static_gtfs);
    let (trip_updates_url, trip_updates_path) = split_loc(pick(
        &overrides.trip_updates,
        vars.get("GTFS_RT_MCP_TRIP_UPDATES"),
    ));
    let (vehicle_positions_url, vehicle_positions_path) = split_loc(pick(
        &overrides.vehicle_positions,
        vars.get("GTFS_RT_MCP_VEHICLE_POSITIONS"),
    ));
    let (alerts_url, alerts_path) =
        split_loc(pick(&overrides.alerts, vars.get("GTFS_RT_MCP_ALERTS")));
    let request = RegisterRequest {
        feed_id: feed_id.clone(),
        static_url,
        static_path,
        trip_updates_url,
        trip_updates_path,
        vehicle_positions_url,
        vehicle_positions_path,
        alerts_url,
        alerts_path,
        rt_ttl_secs,
        static_ttl_secs,
        headers,
    };
    match validate_startup_request(&request) {
        Ok(id) => Ok(RegisterRequest {
            feed_id: Some(id),
            ..request
        }),
        Err(error) => Err(ConfigError {
            message: match error.hint {
                Some(hint) => format!("{} ({hint})", error.error),
                None => error.error,
            },
        }),
    }
}

/// A set flag replaces the environment variable, including when the flag is empty.
fn pick(flag: &Option<String>, env: Option<&String>) -> Option<String> {
    match flag {
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        None => env.cloned().filter(|value| !value.is_empty()),
    }
}

fn ttl_from(
    flag: Option<u64>,
    env: Option<&String>,
    var: &str,
    min: u64,
    max: u64,
) -> Result<Option<u64>, String> {
    let value = if let Some(value) = flag {
        value
    } else if let Some(raw) = env {
        match raw.parse::<u64>() {
            Ok(value) => value,
            Err(_) => {
                return Err(format!(
                    "{var}: {raw:?} is not an integer number of seconds"
                ));
            }
        }
    } else {
        return Ok(None);
    };
    if value < min || value > max {
        return Err(format!(
            "{var} must be an integer from {min} to {max} seconds, got {value}"
        ));
    }
    Ok(Some(value))
}

fn parse_headers(raw: &str) -> Result<BTreeMap<String, String>, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| "GTFS_RT_MCP_HTTP_HEADERS is not a JSON object of strings".to_string())?;
    let Some(object) = value.as_object() else {
        return Err("GTFS_RT_MCP_HTTP_HEADERS is not a JSON object of strings".into());
    };
    let mut headers = BTreeMap::new();
    for (name, item) in object {
        let Some(text) = item.as_str() else {
            return Err(format!(
                "GTFS_RT_MCP_HTTP_HEADERS value for '{name}' must be a string"
            ));
        };
        headers.insert(name.clone(), text.to_string());
    }
    Ok(headers)
}

fn split_loc(value: Option<String>) -> (Option<String>, Option<String>) {
    match value {
        Some(value) if value.starts_with("http://") || value.starts_with("https://") => {
            (Some(value), None)
        }
        Some(value) => (None, Some(value)),
        None => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(pairs: &[(&str, &str)]) -> Result<RegisterRequest, ConfigError> {
        startup_plan(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
            &StartupOverrides::default(),
        )
    }

    #[test]
    fn configured_feed_stores_headers_and_ttls() {
        let secret = "super-secret-feed-token";
        let request = plan(&[
            ("GTFS_RT_MCP_FEED_ID", "harbor"),
            ("GTFS_RT_MCP_STATIC", "https://example.com/gtfs.zip"),
            (
                "GTFS_RT_MCP_TRIP_UPDATES",
                "https://example.com/realtime/trip-updates.pb",
            ),
            (
                "GTFS_RT_MCP_VEHICLE_POSITIONS",
                "https://example.com/realtime/vehicle-positions.pb",
            ),
            (
                "GTFS_RT_MCP_ALERTS",
                "https://example.com/realtime/alerts.pb",
            ),
            ("GTFS_RT_MCP_RT_TTL", "20"),
            ("GTFS_RT_MCP_STATIC_TTL", "0"),
            (
                "GTFS_RT_MCP_HTTP_HEADERS",
                &format!(r#"{{"Authorization":"Bearer {secret}"}}"#),
            ),
        ])
        .unwrap();
        assert_eq!(request.feed_id.as_deref(), Some("harbor"));
        assert_eq!(
            request.static_url.as_deref(),
            Some("https://example.com/gtfs.zip")
        );
        assert_eq!(
            request.trip_updates_url.as_deref(),
            Some("https://example.com/realtime/trip-updates.pb")
        );
        assert_eq!(request.rt_ttl_secs, Some(20));
        assert_eq!(request.static_ttl_secs, Some(0));
        assert_eq!(
            request.headers.get("Authorization").map(String::as_str),
            Some(format!("Bearer {secret}").as_str())
        );
        let rendered = format!("{request:?}");
        assert!(rendered.contains(secret));
    }

    #[test]
    fn missing_static_names_the_variable_and_an_example() {
        let error = plan(&[]).unwrap_err();
        assert!(
            error.message.contains("GTFS_RT_MCP_STATIC"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("https://example.com/gtfs.zip"),
            "{}",
            error.message
        );
        let empty = plan(&[("GTFS_RT_MCP_STATIC", "   ")]).unwrap_err();
        assert!(
            empty.message.contains("GTFS_RT_MCP_STATIC"),
            "{}",
            empty.message
        );
    }

    #[test]
    fn malformed_optional_values_fail_without_the_secret() {
        let secret = "header-secret-value";
        let error = plan(&[
            ("GTFS_RT_MCP_STATIC", "/tmp/gtfs.zip"),
            ("GTFS_RT_MCP_RT_TTL", "soon"),
            ("GTFS_RT_MCP_STATIC_TTL", "99999999"),
            (
                "GTFS_RT_MCP_HTTP_HEADERS",
                &format!(r#"{{"x-api-key":"{secret}""#),
            ),
        ])
        .unwrap_err();
        assert!(
            error.message.contains("GTFS_RT_MCP_RT_TTL"),
            "{}",
            error.message
        );
        assert!(error.message.contains("soon"), "{}", error.message);
        assert!(
            error.message.contains("GTFS_RT_MCP_STATIC_TTL"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("GTFS_RT_MCP_HTTP_HEADERS"),
            "{}",
            error.message
        );
        assert!(
            !error.message.contains(secret),
            "header value leaked: {}",
            error.message
        );
    }

    #[test]
    fn header_object_must_be_strings() {
        let error = plan(&[
            ("GTFS_RT_MCP_STATIC", "https://example.com/gtfs.zip"),
            ("GTFS_RT_MCP_HTTP_HEADERS", r#"{"x-api-key":1}"#),
        ])
        .unwrap_err();
        assert!(
            error.message.contains("must be a string"),
            "{}",
            error.message
        );
        assert!(
            !error.message.contains("\"x-api-key\":1"),
            "{}",
            error.message
        );
    }

    #[test]
    fn flags_override_the_matching_variable() {
        let request = startup_plan(
            [
                (
                    "GTFS_RT_MCP_STATIC".into(),
                    "https://example.test/old.zip".into(),
                ),
                ("GTFS_RT_MCP_FEED_ID".into(), "old".into()),
                ("GTFS_RT_MCP_RT_TTL".into(), "15".into()),
            ],
            &StartupOverrides {
                static_gtfs: Some("https://example.com/gtfs.zip".into()),
                feed_id: Some("harbor".into()),
                rt_ttl_secs: Some(30),
                ..StartupOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(
            request.static_url.as_deref(),
            Some("https://example.com/gtfs.zip")
        );
        assert_eq!(request.feed_id.as_deref(), Some("harbor"));
        assert_eq!(request.rt_ttl_secs, Some(30));
    }

    #[test]
    fn empty_flag_clears_the_environment_value() {
        let error = startup_plan(
            [(
                "GTFS_RT_MCP_STATIC".into(),
                "https://example.com/gtfs.zip".into(),
            )],
            &StartupOverrides {
                static_gtfs: Some("  ".into()),
                ..StartupOverrides::default()
            },
        )
        .unwrap_err();
        assert!(
            error.message.contains("GTFS_RT_MCP_STATIC"),
            "{}",
            error.message
        );
    }

    #[test]
    fn omitted_feed_id_defaults_to_default() {
        let request = plan(&[("GTFS_RT_MCP_STATIC", "/tmp/harbor/static.zip")]).unwrap();
        assert_eq!(request.feed_id.as_deref(), Some("default"));
        assert_eq!(
            request.static_path.as_deref(),
            Some("/tmp/harbor/static.zip")
        );
        assert!(request.trip_updates_url.is_none());
        assert!(request.headers.is_empty());
    }

    #[test]
    fn filesystem_realtime_paths_stay_paths() {
        let request = plan(&[
            ("GTFS_RT_MCP_STATIC", "/tmp/harbor/static.zip"),
            ("GTFS_RT_MCP_TRIP_UPDATES", "/tmp/harbor/trip-updates.pb"),
            (
                "GTFS_RT_MCP_VEHICLE_POSITIONS",
                "https://example.com/realtime/vehicle-positions.pb",
            ),
        ])
        .unwrap();
        assert_eq!(
            request.trip_updates_path.as_deref(),
            Some("/tmp/harbor/trip-updates.pb")
        );
        assert_eq!(
            request.vehicle_positions_url.as_deref(),
            Some("https://example.com/realtime/vehicle-positions.pb")
        );
    }
}
