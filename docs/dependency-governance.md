# Dependency Governance — ga4-mcp

## Selection principles

- Prefer mature, widely used crates for transport/auth/serialization.
- Reuse `mcp-toolkit-rs` crates where they reduce duplicate logic and improve consistency.
- Keep dependency surface minimal and purpose-specific.

## Chosen crates

- `rmcp`: MCP protocol server runtime and tool macros.
- `tokio`: async runtime for stdio + HTTP operations.
- `reqwest`: HTTP client for Google Analytics REST APIs.
- `gcp_auth`: ADC token provider and token caching.
- `duckdb`: embedded analytical engine for session-scoped scratchpad workflows.
- `serde`, `serde_json`, `schemars`: typed argument schemas + JSON contracts.
- `thiserror`, `anyhow`: ergonomic error handling across layers.
- `tracing`, `tracing-subscriber`: structured runtime logs.
- `mcp-toolkit-observability`: consistent safe telemetry emission.
- `mcp-toolkit-policy-core`: shared restricted SQL classifier reused for scratchpad guardrails.
- `mcp-toolkit-testing` (dev): shared test helpers compatibility.

## Not selected (for now)

- Generated Google API Rust clients:
  - Rejected for v1 to keep binary/dependency complexity lower and maintain explicit JSON mapping control.
- SQLite scratchpad engine:
  - Rejected for current roadmap because DuckDB offers stronger analytical query ergonomics for large ad hoc datasets.
- Additional retry middleware crates:
  - Deferred until we observe real retry requirements under load.
