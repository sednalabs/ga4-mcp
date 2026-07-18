# Observability Events

This document lists structured telemetry events emitted by `ga4-mcp`.

All events are emitted through `mcp-toolkit-observability` and only include
sanitized/redacted text fields.

## Core lifecycle

- `ga4_mcp.startup`
  - Fields: `transport`
- `ga4_mcp.http.request`
  - Fields: `method`, `path`, `status`, `auth_enabled`
  - Optional fields (when present): `host`, `cf_ray`, `cf_connecting_ip`, `true_client_ip`, `user_agent`, `remote_addr`
- `ga4_mcp.tool.start`
  - Fields: `tool`
- `ga4_mcp.tool.finish`
  - Fields: `tool`
- `ga4_mcp.tool.error`
  - Fields: `tool`, `error`
- `ga4_mcp.tool.blocked`
  - Fields: `tool`, `capability_profile`

## Contract and pagination

- `ga4_mcp.contract.response`
  - Fields: `tool`, `ok`, `has_data`, `has_meta`, `has_error`, `error_code`, `error_reason`
- `ga4_mcp.contract.missing_payload`
  - Fields: `tool`
- `ga4_mcp.pagination.window`
  - Fields: `tool`, `query_hash`, `cursor_supplied`, `offset`, `page_size`
- `ga4_mcp.pagination.meta`
  - Fields: `tool`, `output_mode`, `row_count_total`, `row_count_returned`, `truncated`, `next_cursor_present`
  - Optional fields (when present in Contract V1 metadata): `query_hash`, `requested_limit`, `effective_limit`, `row_count_total_known`, `truncation_basis`
  - Subreport events also include `subreport`; `query_hash`, `requested_limit`, and `effective_limit` are inherited from the top-level metadata, while `row_count_total_known` and `truncation_basis` are read from that subreport's metadata.
- `ga4_mcp.pagination.cursor_error`
  - Fields: `tool`, `error_code`, `error_reason`, `error`

## Scratchpad telemetry

- `ga4_mcp.scratchpad.load`
  - Fields: `session_id`, `table_name`, `mode`, `rows_inserted`, `columns_inserted`, `duration_ms`
- `ga4_mcp.scratchpad.load.error`
  - Fields: `session_id`, `table_name`, `mode`, `duration_ms`, `error`
- `ga4_mcp.scratchpad.ingest`
  - Fields: `tool`, `report_kind`, `table_name`, `ingest_mode`, `rows_inserted`, `columns_inserted`, `duration_ms`
- `ga4_mcp.scratchpad.ingest.error`
  - Fields: `tool`, `report_kind`, `table_name`, `ingest_mode`, `duration_ms`, `error`
- `ga4_mcp.scratchpad.query.duration`
  - Fields: `tool`, `session_id`, `query_hash`, `pagination_mode`, `row_count_total`, `row_count_returned`, `duration_ms`
- `ga4_mcp.scratchpad.query.error`
  - Fields: `tool`, `session_id`, `query_hash`, `duration_ms`, `error`
- `ga4_mcp.scratchpad.query.runtime`
  - Fields: `session_id`, `outcome`, `duration_ms` (+ `reason` and `error` when present)
- `ga4_mcp.scratchpad.quota_breach`
  - Fields: `session_id`, `field`, `limit`
- `ga4_mcp.scratchpad.table.drop`
  - Fields: `session_id`, `table_name`, `rows_removed`, `duration_ms`

## Notes

- `query_hash` values are emitted in shortened form.
- `run_funnel_report` emits one `ga4_mcp.pagination.window` event before its
  upstream request. Because funnel responses contain independent table and
  visualization subreports rather than cursor pages, it always reports
  `cursor_supplied=false` and `offset=0`; `page_size` is the effective funnel
  response limit.
- Cursor errors are duplicated at both tool-level contract (`ga4_mcp.contract.response`)
  and pagination-specific (`ga4_mcp.pagination.cursor_error`) events to simplify alerting.
