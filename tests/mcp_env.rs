use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use serde_json::{json, Value};

fn tool_json(result: &rmcp::model::CallToolResult) -> Value {
    let text = result
        .content
        .first()
        .and_then(|block| block.as_text())
        .expect("tool text")
        .text
        .clone();
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("json: {text}"))
}

fn child(args: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_gtfs-mcp"));
    command
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("GTFS_MCP_LOG", "error");
    command
}

fn harbor_env(command: &mut tokio::process::Command, paths: &gtfs_mcp::fixtures::HarborPaths) {
    command
        .env("GTFS_MCP_FEED_ID", "harbor")
        .env("GTFS_MCP_STATIC", paths.static_zip.display().to_string())
        .env(
            "GTFS_MCP_TRIP_UPDATES",
            paths.trip_updates.display().to_string(),
        )
        .env(
            "GTFS_MCP_VEHICLE_POSITIONS",
            paths.vehicle_positions.display().to_string(),
        )
        .env("GTFS_MCP_ALERTS", paths.alerts.display().to_string());
}

#[tokio::test]
async fn env_registers_a_feed_without_register_feed() {
    let dir = std::env::temp_dir().join(format!(
        "gtfs-mcp-env-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let paths = gtfs_mcp::fixtures::materialize(&dir).unwrap();
    let secret = "env-only-header-value";
    let mut command = child(&["stdio"]);
    harbor_env(&mut command, &paths);
    command
        .env("GTFS_MCP_RT_TTL", "30")
        .env("GTFS_MCP_STATIC_TTL", "0")
        .env(
            "GTFS_MCP_HTTP_HEADERS",
            format!(r#"{{"x-demo":"{secret}"}}"#),
        );
    let client =
        ().serve(TokioChildProcess::new(command).expect("spawn"))
            .await
            .expect("initialize");

    let listed = client
        .call_tool(CallToolRequestParams::new("list_feeds"))
        .await
        .expect("list_feeds");
    assert_ne!(listed.is_error, Some(true), "{listed:?}");
    let listed = tool_json(&listed);
    let rendered = listed.to_string();
    assert_eq!(listed["feeds"][0]["feed_id"], "harbor");
    assert_eq!(listed["feeds"][0]["ready"], true);
    assert_eq!(listed["feeds"][0]["source"]["kind"], "direct");
    assert_eq!(listed["feeds"][0]["headers"][0], "x-demo");
    assert_eq!(listed["feeds"][0]["realtime"]["ttl_sec"], 30);
    assert!(!rendered.contains(secret), "{rendered}");

    let status = client
        .call_tool(
            CallToolRequestParams::new("trip_status").with_arguments(
                json!({
                    "trip": "177",
                    "stop": "NHV",
                    "at_unix": paths.at_unix
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("trip_status");
    assert_ne!(status.is_error, Some(true), "{status:?}");
    let status = tool_json(&status);
    assert_eq!(status["trips"][0]["at_stop"]["track"], "12");
    assert!(!status.to_string().contains(secret));

    let _ = client.cancel().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn missing_static_exits_nonzero() {
    let mut command = child(&["stdio"]);
    let output = command.output().await.expect("spawn");
    assert!(!output.status.success(), "status {:?}", output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("GTFS_MCP_STATIC"), "{stderr}");
    assert!(stderr.contains("https://example.com/gtfs.zip"), "{stderr}");
}

#[tokio::test]
async fn malformed_optional_values_exit_without_leaking_headers() {
    let secret = "header-secret-value";
    let mut command = child(&["stdio"]);
    command
        .env("GTFS_MCP_STATIC", "/tmp/gtfs.zip")
        .env("GTFS_MCP_RT_TTL", "soon")
        .env(
            "GTFS_MCP_HTTP_HEADERS",
            format!(r#"{{"Authorization":"Bearer {secret}""#),
        );
    let output = command.output().await.expect("spawn");
    assert!(!output.status.success(), "status {:?}", output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("GTFS_MCP_RT_TTL"), "{stderr}");
    assert!(stderr.contains("soon"), "{stderr}");
    assert!(stderr.contains("GTFS_MCP_HTTP_HEADERS"), "{stderr}");
    assert!(!stderr.contains(secret), "{stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains(secret), "{stdout}");
}

#[tokio::test]
async fn unloadable_static_stays_up_and_register_feed_replaces_it() {
    let dir = std::env::temp_dir().join(format!(
        "gtfs-mcp-bad-file-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let paths = gtfs_mcp::fixtures::materialize(&dir).unwrap();
    let mut command = child(&["stdio"]);
    command
        .env("GTFS_MCP_FEED_ID", "harbor")
        .env("GTFS_MCP_STATIC", "/no/such/gtfs-mcp-static.zip");
    let client =
        ().serve(TokioChildProcess::new(command).expect("spawn"))
            .await
            .expect("initialize");

    let listed = client
        .call_tool(CallToolRequestParams::new("list_feeds"))
        .await
        .expect("list_feeds");
    assert_ne!(listed.is_error, Some(true), "{listed:?}");
    let listed = tool_json(&listed);
    assert_eq!(listed["feeds"][0]["feed_id"], "harbor");
    assert_eq!(listed["feeds"][0]["ready"], false);

    let registered = client
        .call_tool(
            CallToolRequestParams::new("register_feed").with_arguments(
                json!({
                    "feed_id": "harbor",
                    "static_path": paths.static_zip.display().to_string(),
                    "trip_updates_path": paths.trip_updates.display().to_string(),
                    "vehicle_positions_path": paths.vehicle_positions.display().to_string(),
                    "alerts_path": paths.alerts.display().to_string(),
                    "rt_ttl_secs": 15
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("register_feed");
    assert_ne!(registered.is_error, Some(true), "{registered:?}");
    let registered = tool_json(&registered);
    assert_eq!(registered["ready"], true);
    assert_eq!(registered["source"]["kind"], "direct");

    let status = client
        .call_tool(
            CallToolRequestParams::new("trip_status").with_arguments(
                json!({"trip": "177", "at_unix": paths.at_unix})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("trip_status");
    assert_ne!(status.is_error, Some(true), "{status:?}");
    assert_eq!(tool_json(&status)["trips"][0]["trip_short_name"], "177");

    let _ = client.cancel().await;
    let _ = std::fs::remove_dir_all(dir);
}
