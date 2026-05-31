//! # ga4-mcp
//!
//! Rust stdio MCP server for Google Analytics (GA4) read/report workflows.
//!
//! ## Rationale
//! Provide a modular Rust implementation of the Google Analytics MCP surface so
//! agents can run high-value analytics queries with low startup overhead.
//!
//! ## Security Boundaries
//! * Read-only OAuth scope (`analytics.readonly`) is used for all API calls.
//! * Tool validation rejects malformed property identifiers and unsafe argument shapes.
//! * Upstream error payloads are sanitized into deterministic MCP-facing contracts.
//!
//! ## References
//! * `docs/GETTING_STARTED.md`
//! * `docs/SECURITY_MODEL.md`
//! * `https://github.com/googleanalytics/google-analytics-mcp`

pub mod config;
pub mod contract;
pub mod error;
pub mod ga_client;
pub mod http_config;
pub mod http_runtime;
pub mod scratchpad;
pub mod server;
pub mod sql_safety;
pub mod tools;

pub type McpError = rmcp::ErrorData;
