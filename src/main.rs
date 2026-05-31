//! # ga4-mcp Main
//!
//! Entrypoint for the Rust stdio Google Analytics MCP server.

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use rmcp::serve_server;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

use ga4_mcp::config::{Cli, Settings};
use ga4_mcp::ga_client::AnalyticsApiClient;
use ga4_mcp::scratchpad::{
    DuckDbEngine, ScratchpadSessionConfig, ScratchpadSessionManager, SharedScratchpadEngine,
};
use ga4_mcp::server::AnalyticsMcp;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("ga4-mcp failed to start: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    init_tracing();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    let settings = Settings::from_cli(cli)?;
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

    if settings.print_tools {
        println!("{}", serde_json::to_string_pretty(&server.tool_names())?);
        return Ok(());
    }

    if settings.print_tool_schema {
        println!(
            "{}",
            serde_json::to_string_pretty(&server.tool_schema_snapshot())?
        );
        return Ok(());
    }

    mcp_toolkit_observability::emit_event(
        mcp_toolkit_observability::Level::INFO,
        "ga4_mcp.startup",
        &mcp_toolkit_observability::EventContext::new(),
        &[mcp_toolkit_observability::safe_text("transport", "stdio")],
    );

    let transport = stdio();
    let service = serve_server(server, transport).await?;
    service.waiting().await?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_writer(std::io::stderr)
        .try_init();
}
