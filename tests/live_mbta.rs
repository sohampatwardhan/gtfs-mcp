//! Opt-in check against a live GTFS static zip and GTFS-Realtime protobufs.
//!
//! Skips unless `GTFS_RT_MCP_STATIC` is set. Optional realtime URLs use the same
//! names as the server: `GTFS_RT_MCP_TRIP_UPDATES`, `GTFS_RT_MCP_VEHICLE_POSITIONS`,
//! and `GTFS_RT_MCP_ALERTS`.
//!
//! ```text
//! GTFS_RT_MCP_STATIC=https://example.com/gtfs.zip \
//! GTFS_RT_MCP_TRIP_UPDATES=https://example.com/realtime/trip-updates.pb \
//! GTFS_RT_MCP_VEHICLE_POSITIONS=https://example.com/realtime/vehicle-positions.pb \
//! GTFS_RT_MCP_ALERTS=https://example.com/realtime/alerts.pb \
//! cargo test --test live_mbta -- --nocapture
//! ```

use serde_json::Value;

fn env_url(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[tokio::test]
async fn live_feed_from_env() {
    let Some(static_url) = env_url("GTFS_RT_MCP_STATIC") else {
        eprintln!("skipping live feed test: GTFS_RT_MCP_STATIC is unset");
        return;
    };
    let trip_updates_url = env_url("GTFS_RT_MCP_TRIP_UPDATES");
    let vehicle_positions_url = env_url("GTFS_RT_MCP_VEHICLE_POSITIONS");
    let alerts_url = env_url("GTFS_RT_MCP_ALERTS");

    let catalog = std::sync::Arc::new(gtfs_rt_mcp::Catalog::new().unwrap());
    let health = catalog
        .register(gtfs_rt_mcp::RegisterRequest {
            feed_id: Some("live".into()),
            static_url: Some(static_url),
            trip_updates_url: trip_updates_url.clone(),
            vehicle_positions_url: vehicle_positions_url.clone(),
            alerts_url: alerts_url.clone(),
            rt_ttl_secs: Some(30),
            ..Default::default()
        })
        .await
        .expect("register live feed");
    assert!(health["static"]["stops"].as_i64().unwrap_or(0) > 0);
    if vehicle_positions_url.is_some() {
        assert!(
            health["realtime"]["vehicle_positions"]["entities"]
                .as_i64()
                .unwrap_or(0)
                > 0
        );
    }

    let stops = catalog
        .search_stops(Some("live".into()), "", 5, 0)
        .await
        .unwrap();
    let stop_id = stops["stops"][0]["stop_id"]
        .as_str()
        .expect("a stop")
        .to_string();
    let board = catalog
        .departure_board(
            Some("live".into()),
            &stop_id,
            None,
            None,
            90,
            5,
            5,
            0,
            false,
        )
        .await
        .unwrap();
    let vehicles = catalog
        .vehicles(
            Some("live".into()),
            gtfs_rt_mcp::VehicleQuery {
                route: None,
                trip: None,
                vehicle: None,
                stop: None,
                limit: 3,
                offset: 0,
            },
        )
        .await
        .unwrap();
    let alerts = catalog
        .alerts(
            Some("live".into()),
            gtfs_rt_mcp::AlertQuery {
                route: None,
                stop: None,
                text: None,
                active_only: true,
                at_unix: chrono_now(),
                limit: 3,
                offset: 0,
                max_chars: 180,
            },
        )
        .await
        .unwrap();

    println!("HEALTH {}", compact(&health));
    println!("STOPS {}", compact(&stops));
    println!("BOARD {}", compact(&board));
    println!("VEHICLES {}", compact(&vehicles));
    println!("ALERTS {}", compact(&alerts));
    assert!(board["departures"].as_array().is_some());
    if alerts_url.is_some() {
        assert!(alerts["alerts"].as_array().is_some());
    }
    if vehicle_positions_url.is_some() {
        assert!(!vehicles["vehicles"].as_array().unwrap().is_empty());
    }
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
