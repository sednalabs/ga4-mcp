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
| `run_conversions_report` | Run a v1alpha conversion, ad-performance, ROAS, or attribution report. |
| `run_funnel_report` | Run a v1alpha funnel report with optional breakdown, next-action, segment, and trended views. |
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

`run_conversions_report` uses the same tabular controls and cursor behavior as
`run_report`. Its `conversion_spec.conversion_actions` accepts zero or more
`conversionActions/<id>` resource names; an empty list means all conversions.
The optional attribution model is `DATA_DRIVEN` or `LAST_CLICK`. Google limits
this alpha report to its documented conversion dimensions and metrics, which
the MCP validates before making an upstream request.

`run_funnel_report` accepts simple event steps such as
`{"name":"Read","event":"page_view"}` or complete GA
`filter_expression` objects. When `name` is omitted, steps are named `Step 1`,
`Step 2`, and so on to match Google's official implementation. It returns `funnel_table` and
`funnel_visualization` as separately projected subreports. `max_rows`,
`output_mode`, `summary_only`, and `max_cell_chars` apply to both. Google does
not return an exact total row count for these subreports, so metadata reports
`row_count_total_known=false` and conservatively marks a subreport truncated
when it fills the effective request limit; no cursor is advertised.

Both tools use Google Analytics Data API v1alpha. Conversion reporting may not
be enabled for every property, and alpha contracts can change. Provider
eligibility and alpha errors are returned through the normal Contract V1 error
envelope.

Provider references: [funnel reports](https://developers.google.com/analytics/devguides/reporting/data/v1/funnels)
and [conversion reports](https://developers.google.com/analytics/devguides/reporting/data/v1/conversions-api-basics).

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
