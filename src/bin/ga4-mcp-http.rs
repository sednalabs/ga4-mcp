//! # ga4-mcp-http
//!
//! Streamable HTTP entrypoint for GA4 MCP server deployment.

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use ga4_mcp::config::{Cli, Settings};
use ga4_mcp::ga_client::AnalyticsApiClient;
use ga4_mcp::http_config::{HttpSettings, validate_http_runtime_credential_posture};
use ga4_mcp::http_runtime::run_http_server;
use ga4_mcp::scratchpad::{
    DuckDbEngine, ScratchpadSessionConfig, ScratchpadSessionManager, SharedScratchpadEngine,
};
use ga4_mcp::server::AnalyticsMcp;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("ga4-mcp-http failed to start: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    init_tracing();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    let settings = Settings::from_cli(cli)?;
    let http_settings = HttpSettings::from_env()?;
    validate_http_runtime_credential_posture(
        &http_settings,
        settings.upstream_token_source,
        &settings.upstream_token_header,
    )?;
    let client = Arc::new(AnalyticsApiClient::from_settings(&settings).await?);

    let scratchpad_engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new()?);
    let scratchpad_config = ScratchpadSessionConfig::new(
        settings.scratchpad_session_ttl,
        settings.scratchpad_max_sessions,
        settings.scratchpad_max_tables_per_session,
        settings.scratchpad_max_rows_per_session,
        settings.scratchpad_max_memory_mb,
    )
    .with_query_timeout(settings.scratchpad_query_timeout)
    .with_max_sql_bytes(settings.scratchpad_max_sql_bytes);
    let scratchpad_sessions = Arc::new(ScratchpadSessionManager::new(
        scratchpad_engine,
        scratchpad_config,
    )?);

    let server = AnalyticsMcp::new(
        client,
        scratchpad_sessions,
        settings.capability_profile,
        settings.upstream_token_source,
        settings.upstream_token_header.clone(),
    );
    run_http_server(server, http_settings).await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_writer(std::io::stderr)
        .try_init();
}
