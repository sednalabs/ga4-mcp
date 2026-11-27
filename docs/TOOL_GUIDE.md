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
| `list_key_events` | List configured key-event definitions (including counting method and default value). |
| `get_custom_dimensions_and_metrics` | List custom definitions for one property. |

## Report Tools

The table below describes the local `ga4-mcp` surface. It includes both
`run_funnel_report` and `run_conversions_report`, which use Google Analytics
Data API v1alpha. The upstream reference inventory may vary by revision.
Contract V1 envelopes, validation, projections, and related response metadata
described below are local `ga4-mcp` semantics and extensions.

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

`run_realtime_report` is the exception to cursor paging: Google's Realtime
request schema supports `limit` but has no `offset` field. It returns one
bounded result page, rejects non-zero offsets and cursors, and reports
truncation without advertising a cursor that would repeat the same rows.
Standard `run_report` pagination is unchanged.

Both `run_realtime_report` and `scratchpad_ingest_realtime_report` accept
optional `minute_ranges` (up to two objects), e.g.
`[{"name":"recent","start_minutes_ago":4,"end_minutes_ago":0}]`.
CamelCase field aliases are accepted. Omitted bounds default to 29 and 0;
minutes 30–59 require a GA360 property and eligibility is checked by Google.
Ranges are validated before an upstream request and are not pagination.

`run_conversions_report` uses the same tabular controls and cursor behavior as
`run_report`. Its `conversion_spec.conversion_actions` accepts zero or more
`conversionActions/<id>` resource names; an empty list means all conversions.
The MCP accepts at most 50 conversion actions and caps the combined outbound
JSON size of the date ranges, dimensions, metrics, conversion spec, filters,
ordering expressions, and currency code at 64 KiB before calling Google.
The optional attribution model is `DATA_DRIVEN` or `LAST_CLICK`. Google limits
this alpha report to its documented conversion dimensions and metrics, which
the MCP validates before making an upstream request.

`run_funnel_report` accepts simple event steps such as
`{"name":"Read","event":"page_view"}` or complete GA
`filter_expression` objects. When `name` is omitted, local normalization names
steps `Step 1`, `Step 2`, and so on. It returns `funnel_table` and
`funnel_visualization` as
separately projected subreports. `max_rows`,
`output_mode`, `summary_only`, and `max_cell_chars` apply to both. Google does
not return an exact total row count for these subreports, so metadata reports
`row_count_total_known=false` and conservatively marks a subreport truncated
when it fills the effective request limit; no cursor is advertised.
Requests are limited to 10 funnel steps, and the combined outbound JSON size
of funnel steps, date ranges, segments, filters, breakdown, and next-action
structures is capped at 64 KiB before calling Google.

The generated JSON schema describes request structure; runtime validation also
enforces semantic constraints. Each funnel step must be non-empty and provide
exactly one of `event` or `filter_expression`.

Funnel `segments` are provider-boundary JSON objects. The MCP enforces at most
four non-empty objects, but does not fully validate Google's segment union
shape; malformed or unsupported segment objects may therefore be rejected by
Google.

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

## BigQuery export discovery

Use `list_bigquery_links` with a property ID to inspect the configured export
project (Google returns a project number), dataset location, daily and
streaming export settings, selected streams, and excluded events. The GA Admin
v1alpha endpoint accepts the existing `analytics.readonly` scope. Pagination
is bounded by the same server limits as other Admin list tools. Discovery
does not verify that an export dataset or table exists, establish BigQuery IAM
permissions, read event records, or start a billable BigQuery job. Resolve the
project number and verify the expected `analytics_<property_id>` dataset in the
linked project and location before any event-export query. Export data and
GA Data API aggregate reports have different semantics.

## Elevated configuration audit

`search_change_history_events` requires capability profile `audit` and explicit
`analytics.edit` scope, even though the operation is read-only. Google also
checks the caller's account permissions. Supports account/property, resource,
action, actor and RFC3339 time filters. Request one to ten pages (default one),
each at most 200 events (default 50); use the returned continuation token only
with the same filters. Actor emails and allowlisted configuration snapshots are
omitted by default. Secret-bearing resources and unrecognised fields are never
included in snapshots. The API exposes only Google's supported change history,
not necessarily the complete UI audit log.

The audit runtime deliberately accepts the exact explicit `analytics.edit`
scope, not a broader mixed-scope configuration. Provider rejection messages
are replaced with status-specific guidance to avoid reflecting actor filters
or sensitive upstream values. An unexpectedly oversized provider page fails
closed rather than silently dropping audit events or breaching the row bound.

## Optional configuration creation

In a dedicated local stdio process with `mutation` capability profile, exact
`analytics.edit` scope and `config` upstream token source, the following tools
are exposed: `create_reporting_data_annotation`, `create_custom_dimension`, and
`create_key_event`. Each validates its arguments and requires an independent
client-mediated approval of the exact property and payload. See SECURITY_MODEL.md
for limitations. A successful POST is returned even when readback is unconfirmed;
never blindly retry an ambiguous create. No live Google mutations are used in tests.
