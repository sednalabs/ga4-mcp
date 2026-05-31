# Payload Contract V1

This document defines the canonical response contract for `ga4-mcp`.

## Scope

Contract V1 applies to all tool responses produced by this server.

- Success responses use `ok/data/meta`.
- Error responses use `ok/error/meta`.
- There is no legacy response mode for this server rollout.

## Versioning Policy

- Initial public rollout is Contract V1 with full metadata semantics.
- Backward-compat envelope modes are out of scope.
- Contract changes that break V1 semantics require a new contract version document.

## Canonical Envelope

### Success envelope

```json
{
  "ok": true,
  "data": {},
  "meta": {
    "elapsed_ms": 12
  }
}
```

### Error envelope

```json
{
  "ok": false,
  "error": {
    "code": "INVALID_PARAMS",
    "reason": "invalid_params",
    "message": "property_id must be properties/<id> or integer id",
    "status_code": 400
  },
  "meta": {
    "elapsed_ms": 2
  }
}
```

## Shared Fields

### Top-level fields

- `ok` (`boolean`, required): success/failure discriminator.
- `data` (`any`, required when `ok=true`): tool payload.
- `error` (`object`, required when `ok=false`): structured error payload.
- `meta` (`object`, required): execution metadata.

### Required metadata

- `meta.elapsed_ms` (`number`, required): execution latency in milliseconds.

## Tabular Metadata Contract

For tools returning tabular payloads (including scratchpad query tools and report outputs modeled as rows), `meta` additionally requires:

- `output_mode` (`rows|tuples|scalar|compact`)
- `summary_only` (`boolean`)
- `row_count_total` (`number`)
- `row_count_returned` (`number`)
- `truncated` (`boolean`)
- `next_cursor` (`string|null`)
- `query_hash` (`string|null`)
- `columns` (`array`)

`columns` entries are engine-neutral metadata objects:

```json
{
  "name": "eventName",
  "logical_type": "string",
  "nullable": true
}
```

Optional metadata for bounded payload telemetry:

- `cell_clipping` object with clipping settings and counters.
- `query_hints` array for non-fatal guidance.

## Tabular Request Controls

Applicable tabular tools support these request fields:

- `max_rows` (`number`, optional): page-size request within server limits.
- `cursor` (`string`, optional): opaque pagination token from `meta.next_cursor`.
- `output_mode` (`rows|tuples|scalar|compact`, optional).
- `summary_only` (`boolean`, optional, default `false`): omit row payload while preserving metadata.
- `max_cell_chars` (`number`, optional): apply per-cell clipping in row payloads.

## Output Modes

- `rows`: array of row objects keyed by column name.
- `tuples`: array of positional arrays matching column order.
- `scalar`: first column of first row, or `null` when no rows exist.
- `compact`: object with `columns`, `tuples`, `row_count`.

When `summary_only=true`, `data` is `null` regardless of output mode.

## Cursor and Pagination Rules

- Clients must treat `next_cursor` as opaque.
- First page omits `cursor`.
- Follow-up page passes previous `meta.next_cursor`.
- Cursor/query mismatches return `ok=false` with `reason=invalid_cursor`.
- Invalid cursor format returns `ok=false` with `reason=invalid_cursor`.

## Error Contract

Minimum error fields:

- `code` (`string`)
- `reason` (`string`)
- `message` (`string`)

Optional error fields:

- `status_code` (`number`)
- `engine_code` (`string`)
- `category` (`string`)
- `detail` (`string|null`)
- `hint` (`string|null`)
- `position` (`string|null`)

Postgres-specific fields are not required by this contract.

## Tool Applicability

### Base-envelope tools (non-tabular metadata only)

- `get_account_summaries`
- `get_account_data_sharing_settings`
- `get_property_details`
- `get_property_data_retention_settings`
- `list_google_ads_links`
- `list_property_annotations`
- `get_custom_dimensions_and_metrics`
- `check_report_compatibility`
- `preview_report_request`
- `scratchpad_open_session`
- `scratchpad_close_session`
- `scratchpad_list_sessions`
- `scratchpad_list_tables`
- `scratchpad_drop_table`
- `scratchpad_ingest_report`
- `scratchpad_ingest_realtime_report`
- `scratchpad_get_runtime_limits`
- `scratchpad_set_runtime_limits`
- `scratchpad_export_evidence_bundle`

### Tabular-metadata tools

- `run_report`
- `run_realtime_report`
- `run_pivot_report`
- `batch_run_reports`
- `run_property_access_report`
- `run_account_access_report`
- `scratchpad_query`
- `scratchpad_describe_table`
- `scratchpad_summarize_table`
- `scratchpad_release_regression_report`
- `scratchpad_landing_param_shift_report`

## Machine-readable Schema

Contract schema: `spec/payload_contract_v1.schema.json`

This schema is the automation target for contract conformance tests and fixture validation.
