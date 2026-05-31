use clap::Parser;
use ga4_mcp::config::{Cli, Settings};
use ga4_mcp::ga_client::{AnalyticsApiClient, PaginationOptions, PropertyId, RunReportRequest};
use serde_json::{Value, json};
use std::sync::Once;

const ENABLE_ENV: &str = "GA4_LIVE_SMOKE";
const PROPERTY_ENV: &str = "GA4_LIVE_SMOKE_PROPERTY_ID";
const DEFAULT_PROPERTY: &str = "properties/301409766";
static RUSTLS_PROVIDER_INIT: Once = Once::new();

fn live_smoke_enabled() -> bool {
    std::env::var(ENABLE_ENV)
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn smoke_property_id() -> PropertyId {
    let raw = std::env::var(PROPERTY_ENV).unwrap_or_else(|_| DEFAULT_PROPERTY.to_string());
    match raw.trim().parse::<u64>() {
        Ok(value) => PropertyId::Number(value),
        Err(_) => PropertyId::Text(raw),
    }
}

fn load_settings_from_env() -> Settings {
    let cli = Cli::parse_from(["ga4-live-google-auth-smoke"]);
    Settings::from_cli(cli).expect("live smoke should load GA4 settings from env")
}

fn ensure_rustls_provider() {
    RUSTLS_PROVIDER_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[tokio::test]
async fn live_google_auth_and_ga_tool_calls_succeed() {
    if !live_smoke_enabled() {
        eprintln!(
            "skipping live smoke; set {ENABLE_ENV}=1 (and optionally {PROPERTY_ENV}) to enable"
        );
        return;
    }

    ensure_rustls_provider();

    let settings = load_settings_from_env();
    let client = AnalyticsApiClient::from_settings(&settings)
        .await
        .expect("google auth bootstrap should succeed");

    let summaries = client
        .get_account_summaries(PaginationOptions {
            page_size: Some(5),
            max_pages: Some(1),
        })
        .await
        .expect("account summary call should succeed");
    let accounts = summaries
        .get("account_summaries")
        .and_then(Value::as_array)
        .expect("account_summaries should be an array");
    assert!(
        !accounts.is_empty(),
        "expected at least one GA account summary row"
    );

    let property_id = smoke_property_id();
    let details = client
        .get_property_details(&property_id)
        .await
        .expect("property details call should succeed");
    assert!(
        details.get("name").and_then(Value::as_str).is_some(),
        "property details should include a resource name"
    );

    let report = client
        .run_report(RunReportRequest {
            property_id,
            date_ranges: vec![json!({"startDate":"7daysAgo","endDate":"yesterday"})],
            dimensions: vec!["date".to_string()],
            metrics: vec!["activeUsers".to_string()],
            dimension_filter: None,
            metric_filter: None,
            order_bys: None,
            limit: Some(5),
            offset: Some(0),
            currency_code: None,
            return_property_quota: false,
        })
        .await
        .expect("run_report call should succeed");
    assert!(
        report.get("rows").and_then(Value::as_array).is_some(),
        "run_report should include rows array"
    );
}
