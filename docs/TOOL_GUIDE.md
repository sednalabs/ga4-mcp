# Tool Guide

`ga4-mcp` exposes Google Analytics read/report tools plus optional local
scratchpad analysis tools. All responses use Contract V1 envelopes:
`ok/data/meta` for success and `ok/error/meta` for failure.

## Core GA Tools

| Tool | Purpose |
|---|---|
| `get_account_summaries` | List GA accounts and properties visible to the current Google identity. |
| `get_property_details` | Read details for one GA4 property. |
| `get_account_data_sharing_settings` | Read account-level data-sharing settings. |
| `get_property_data_retention_settings` | Read property-level retention settings. |
| `list_google_ads_links` | List Google Ads links for one property. |
| `list_property_annotations` | List annotations for one property. |
| `get_custom_dimensions_and_metrics` | List custom definitions for one property. |

## Report Tools

| Tool | Purpose |
|---|---|
| `run_report` | Run a GA Data API report. |
| `run_realtime_report` | Run a realtime report. |
| `run_pivot_report` | Run a pivot report. |
| `batch_run_reports` | Run up to five report requests in one batch. |
| `run_property_access_report` | Read property access-report rows. |
| `run_account_access_report` | Read account access-report rows. |

Report-like tools support tabular response controls where applicable:

- `max_rows` for response page size within server limits.
- `cursor` for follow-up pages using `meta.next_cursor`.
- `output_mode` as `rows`, `tuples`, `scalar`, or `compact`.
- `summary_only=true` to return metadata without row payload.
- `max_cell_chars` to clip large cell values.

## Preflight Tools

Use these before expensive or repeated report calls:

| Tool | Purpose |
|---|---|
| `preview_report_request` | Validate and normalize a report request without calling GA. |
| `check_report_compatibility` | Check dimension/metric compatibility for a property. |

`dimension_filter` accepts GA `FilterExpression` objects, JSON-object strings,
or shorthand expressions such as `field==value`. `metric_filter` accepts GA
`FilterExpression` objects or JSON-object strings.

## Scratchpad Tools

Scratchpad tools require:

```bash
GOOGLE_ANALYTICS_MCP_CAPABILITY_PROFILE=scratchpad
```

| Tool | Purpose |
|---|---|
| `scratchpad_open_session` | Open a bounded local DuckDB-backed analysis session. |
| `scratchpad_close_session` | Close a scratchpad session. |
| `scratchpad_list_sessions` | List scratchpad sessions. |
| `scratchpad_get_runtime_limits` | Inspect current scratchpad runtime limits. |
| `scratchpad_set_runtime_limits` | Adjust scratchpad runtime limits within policy. |
| `scratchpad_list_tables` | List tables in a scratchpad session. |
| `scratchpad_drop_table` | Drop a scratchpad table and reclaim quota. |
| `scratchpad_ingest_report` | Ingest a GA report into a scratchpad table. |
| `scratchpad_ingest_realtime_report` | Ingest a realtime report into a scratchpad table. |
| `scratchpad_query` | Run a restricted read-only SQL query over scratchpad tables. |
| `scratchpad_describe_table` | Describe table columns and sample metadata. |
| `scratchpad_summarize_table` | Produce a bounded table summary. |
| `scratchpad_release_regression_report` | Compare pre/post release windows. |
| `scratchpad_landing_param_shift_report` | Compare landing-parameter distributions. |
| `scratchpad_export_evidence_bundle` | Export bounded evidence for review. |

## Identifier Formats

Property ids may be passed as either an integer id or `properties/<id>`.
Account ids may be passed as either an integer id or `accounts/<id>`.

## Error Handling

Clients should branch on `ok` first. When `ok=false`, inspect
`error.reason`, `error.code`, and `error.status_code` if present. Authentication
and upstream Google failures are reported through the same Contract V1 shape as
validation and policy failures.
