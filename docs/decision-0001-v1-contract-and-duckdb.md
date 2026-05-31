# Decision 0001: Contract V1 Full Metadata and DuckDB Scratchpad

- Status: accepted
- Date: 2026-02-23

## Context

`ga4-mcp` is moving from a minimal response envelope to a richer contract that supports:

- predictable agent parsing,
- bounded payload behavior for large result sets,
- deeper, multi-step analysis without prompt-context blowout.

We also need an in-server analytical scratchpad to let agents stage and query intermediate datasets without repeatedly shipping raw data through MCP messages.

## Decision

We will implement **Contract V1 as the full metadata contract from initial rollout**.

- No legacy mode.
- No backward-compatibility response path.
- All new work targets the V1 contract directly.

For analytical scratchpad workloads, we choose **DuckDB** as the engine.

## Contract V1 Requirements

### Envelope

- Success: `ok/data/meta`
- Error: `ok/error/meta`

### Shared metadata

- `meta.elapsed_ms` is required on all responses.

### Tabular metadata (required for applicable tools)

- `output_mode`
- `summary_only`
- `row_count_total`
- `row_count_returned`
- `truncated`
- `next_cursor`
- `query_hash`
- ordered `columns` metadata
- optional clipping telemetry (`max_cell_chars` + clipping counters)

### Tabular request controls (applicable tools)

- `max_rows`
- `cursor`
- `output_mode`
- `summary_only`
- `max_cell_chars`

### Field portability constraints

We adopt proven semantics from `postgres-mcp-rs` but keep the schema engine-neutral.

- Keep: envelope shape and pagination/summary semantics.
- Exclude Postgres-specific fields from the GA4/DuckDB contract:
  - `sqlstate`
  - `pg_type`
  - `response_formatting_mode`
  - `currency_columns`

## DuckDB Scratchpad Requirements

- Session scoped by explicit `session_id`.
- Read/query oriented by default; fail closed on disallowed operations.
- Guardrails:
  - query timeout,
  - row/result size caps,
  - bounded in-memory usage per session,
  - explicit cleanup/TTL behavior.
- No cross-session data visibility.

## Alternatives Considered

### SQLite in-memory

Pros:

- simple dependency/runtime profile,
- mature and well-understood.

Cons:

- weaker analytical ergonomics for large joins/window-heavy workflows,
- lower headroom for investigation-style agent workflows.

Decision: not selected.

### Dual-mode rollout (legacy + v1)

Pros:

- easier transition for hypothetical legacy consumers.

Cons:

- unnecessary complexity for initial release,
- doubles contract maintenance/test matrix,
- increases drift risk during rapid evolution.

Decision: not selected.

## Consequences

- Positive:
  - better agent reliability and context efficiency,
  - clearer operational signals from structured metadata,
  - single contract target reduces maintenance overhead.
- Tradeoffs:
  - larger initial implementation scope,
  - stronger need for contract conformance tests and documentation discipline.
