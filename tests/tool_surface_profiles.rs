use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ga4_mcp::config::{CapabilityProfile, Settings};
use ga4_mcp::ga_client::AnalyticsApiClient;
use ga4_mcp::scratchpad::{
    DuckDbEngine, ScratchpadSessionConfig, ScratchpadSessionManager, SharedScratchpadEngine,
};
use ga4_mcp::server::AnalyticsMcp;
use serde_json::Value;

fn unique_test_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic")
        .as_nanos();
    std::env::temp_dir().join(format!("ga4-mcp-tool-surface-{label}-{nanos}"))
}

async fn server_for_profile(profile: CapabilityProfile) -> (AnalyticsMcp, PathBuf) {
    let settings = Settings {
        analytics_scope: "https://www.googleapis.com/auth/analytics.readonly".to_string(),
        admin_base_url: "https://analyticsadmin.googleapis.com".to_string(),
        data_base_url: "https://analyticsdata.googleapis.com".to_string(),
        http_timeout: Duration::from_millis(15_000),
        max_page_size: 200,
        max_pages: 20,
        user_agent: "ga4-mcp/test".to_string(),
        oauth_client_secret_json: None,
        oauth_refresh_token: None,
        upstream_token_source: ga4_mcp::config::UpstreamTokenSource::Config,
        upstream_token_header: "x-google-access-token".to_string(),
        quota_project: None,
        shared_adc: false,
        scratchpad_session_ttl: Duration::from_secs(60),
        scratchpad_max_sessions: 4,
        scratchpad_max_tables_per_session: 4,
        scratchpad_max_rows_per_session: 1_000,
        scratchpad_max_memory_mb: 128,
        scratchpad_query_timeout: Duration::from_millis(15_000),
        scratchpad_max_sql_bytes: 65_536,
        capability_profile: profile,
        print_tools: false,
        print_tool_schema: false,
        command: None,
    };
    let client = Arc::new(
        AnalyticsApiClient::from_settings(&settings)
            .await
            .expect("client should initialize"),
    );
    let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
    let root_dir = unique_test_root(profile.as_str());
    let config = ScratchpadSessionConfig::new(Duration::from_secs(60), 4, 4, 1_000, 128)
        .with_root_dir(root_dir.clone());
    let sessions =
        Arc::new(ScratchpadSessionManager::new(engine, config).expect("session manager"));
    let server = AnalyticsMcp::new(
        client,
        sessions,
        profile,
        settings.upstream_token_source,
        settings.upstream_token_header.clone(),
    );
    (server, root_dir)
}

fn snapshot_tool_names(snapshot: &Value) -> Vec<String> {
    snapshot["tools"]
        .as_array()
        .expect("tool snapshot should contain a tools array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn read_snapshot(path: &str) -> Value {
    let raw = std::fs::read_to_string(path).expect("snapshot should be readable");
    serde_json::from_str(&raw).expect("snapshot should be valid JSON")
}

#[tokio::test]
async fn read_only_profile_hides_scratchpad_tools_from_visible_surfaces() {
    let (server, root_dir) = server_for_profile(CapabilityProfile::ReadOnly).await;

    let tool_names = server.tool_names();
    assert!(
        tool_names.iter().all(|name| !name.starts_with("scratchpad_")),
        "read_only profile should not export scratchpad tools: {tool_names:?}"
    );

    let snapshot = server.tool_schema_snapshot();
    let snapshot_names = snapshot_tool_names(&snapshot);
    assert!(
        snapshot_names
            .iter()
            .all(|name| !name.starts_with("scratchpad_")),
        "read_only schema snapshot should not advertise scratchpad tools: {snapshot_names:?}"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(root_dir);
}

#[tokio::test]
async fn scratchpad_profile_exposes_scratchpad_tools_to_visible_surfaces() {
    let (server, root_dir) = server_for_profile(CapabilityProfile::Scratchpad).await;

    let tool_names = server.tool_names();
    assert!(
        tool_names.iter().any(|name| name == "scratchpad_query"),
        "scratchpad profile should export scratchpad tools: {tool_names:?}"
    );

    let snapshot = server.tool_schema_snapshot();
    let snapshot_names = snapshot_tool_names(&snapshot);
    assert!(
        snapshot_names.iter().any(|name| name == "scratchpad_query"),
        "scratchpad schema snapshot should advertise scratchpad tools: {snapshot_names:?}"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(root_dir);
}

#[tokio::test]
async fn committed_read_only_snapshot_matches_server_output() {
    let (server, root_dir) = server_for_profile(CapabilityProfile::ReadOnly).await;
    let actual = server.tool_schema_snapshot();
    let expected = read_snapshot("spec/tool_schema_snapshot.v1.json");
    assert_eq!(actual, expected);

    drop(server);
    let _ = std::fs::remove_dir_all(root_dir);
}

#[tokio::test]
async fn committed_scratchpad_snapshot_matches_server_output() {
    let (server, root_dir) = server_for_profile(CapabilityProfile::Scratchpad).await;
    let actual = server.tool_schema_snapshot();
    let expected = read_snapshot("spec/tool_schema_snapshot.scratchpad.v1.json");
    assert_eq!(actual, expected);

    drop(server);
    let _ = std::fs::remove_dir_all(root_dir);
}
