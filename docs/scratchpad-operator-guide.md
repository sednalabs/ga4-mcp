# Scratchpad Operator Guide

This runbook covers the GA4 DuckDB scratchpad workflow and the V1 response contract expected by agents.

## Rollout Mode

- This server is V1-only (`ok/data/meta` and `ok/error/meta`).
- There is no legacy response mode and no compatibility shim.

## Capability Profiles

Set `GOOGLE_ANALYTICS_MCP_CAPABILITY_PROFILE` (or `--capability-profile`) to control dispatch-time tool authorization.

- `read_only` (default): GA read/report tools only. All `scratchpad_*` tools are blocked.
- `scratchpad`: GA + scratchpad tools enabled.

When blocked, tools return:

```json
{
  "ok": false,
  "error": {
    "code": "POLICY_DENIED",
    "reason": "policy_denied",
    "category": "policy"
  },
  "meta": {
    "elapsed_ms": 0
  }
}
```

## Scratchpad Workflow (Recommended)

1. Open session

Tool: `scratchpad_open_session`

Request args:

```json
{
  "session_id": "analysis_2026_02_23"
}
```

Runtime limit controls (no restart required):

- `scratchpad_get_runtime_limits`
- `scratchpad_set_runtime_limits`
- `scratchpad_get_runtime_limits` also reports live DuckDB `memory_pressure` metrics, including per-session probe status and aggregate usage percentage.

Example runtime update:

```json
{
  "max_sessions": 64,
  "max_tables_per_session": 64
}
```

2. Preview report payload before execution (recommended)

Tool: `preview_report_request`

Use this as the first validation step for complex report payloads. It validates locally and returns a normalized request + projected columns without executing an upstream GA API call.

Request args (shorthand `dimension_filter` example):

```json
{
  "property_id": "properties/<YOUR_PROPERTY_ID>",
  "date_ranges": [{ "startDate": "7daysAgo", "endDate": "yesterday" }],
  "dimensions": ["sessionDefaultChannelGroup", "landingPagePlusQueryString"],
  "metrics": ["sessions"],
  "dimension_filter": "sessionDefaultChannelGroup==\"Paid Other\"",
  "max_rows": 500
}
```

Request args (JSON `FilterExpression` example):

```json
{
  "property_id": "properties/<YOUR_PROPERTY_ID>",
  "date_ranges": [{ "startDate": "2026-02-01", "endDate": "2026-02-07" }],
  "dimensions": ["sessionSource", "country"],
  "metrics": ["sessions"],
  "dimension_filter": {
    "and_group": {
      "expressions": [
        {
          "filter": {
            "field_name": "sessionSource",
            "string_filter": { "match_type": "EXACT", "value": "google" }
          }
        }
      ]
    }
  }
}
```

Representative response shape:

```json
{
  "ok": true,
  "data": {
    "preview": {
      "tool": "run_report",
      "query_hash": "<hash>",
      "request": { "...": "normalized request payload" },
      "pagination": { "...": "effective local paging window" }
    },
    "projection": {
      "tabular_columns": [{ "name": "sessionSource", "logical_type": "string", "nullable": true }],
      "ingest_columns": [{ "source_name": "date", "target_name": "date_parsed", "transform": "parse_ga_date" }]
    },
    "hints": [
      "preview_report_request does not execute an upstream GA API call"
    ]
  },
  "meta": {
    "elapsed_ms": 4
  }
}
```

3. Optional compatibility preflight for large pulls

Tool: `check_report_compatibility`

Use this before large ingests when dimension/metric compatibility is uncertain.

Request args (example):

```json
{
  "property_id": "properties/<YOUR_PROPERTY_ID>",
  "dimensions": ["sessionDefaultChannelGroup", "landingPagePlusQueryString"],
  "metrics": ["sessions"]
}
```

4. Ingest GA report output

Tool: `scratchpad_ingest_report`

Request args (example):

```json
{
  "session_id": "analysis_2026_02_23",
  "table_name": "daily_engagement",
  "property_id": "properties/<YOUR_PROPERTY_ID>",
  "date_ranges": [{ "startDate": "7daysAgo", "endDate": "yesterday" }],
  "dimensions": ["date", "country"],
  "metrics": ["activeUsers", "sessions"],
  "dimension_filter": "eventName==signup_complete",
  "append": false,
  "max_rows": 25000
}
```

Notes:

- `date_ranges` entries must be objects containing non-empty `start_date`/`startDate` and `end_date`/`endDate` strings.
- `dimension_filter` supports object, JSON-string object, and shorthand `field==value`.
- Shorthand values with spaces must be quoted, for example `sessionDefaultChannelGroup=="Paid Other"`.
- Ingest auto-pages upstream report responses; `max_rows` controls page size and `limit` caps total ingested rows.
- Ingest responses now include `source.pagination` metadata so truncation/completion is explicit.
- Set `append=true` to ingest additional rows into an existing scratchpad table. Append mode requires the incoming schema to match the existing table exactly (column names/order) and remain type-compatible with existing DuckDB column types.
- For temporal dimensions, ingest preserves raw values and adds parsed helpers:
  - `date` -> `date_parsed` (`DATE`)
  - `dateHour` -> `datehour_parsed` (`TIMESTAMP`)

5. Query scratchpad table with pagination controls

Tool: `scratchpad_query`

Request args:

```json
{
  "session_id": "analysis_2026_02_23",
  "sql": "SELECT date_parsed, country, active_users FROM daily_engagement ORDER BY active_users DESC",
  "max_rows": 100,
  "output_mode": "rows",
  "summary_only": false,
  "max_cell_chars": 512
}
```

Representative success response (shape):

```json
{
  "ok": true,
  "data": [
    { "date_parsed": "2026-02-22", "country": "US", "active_users": 1234 }
  ],
  "meta": {
    "elapsed_ms": 14,
    "output_mode": "rows",
    "summary_only": false,
    "row_count_total": 1000,
    "row_count_returned": 100,
    "truncated": true,
    "next_cursor": "v1:8ac9...:100",
    "query_hash": "8ac9...",
    "columns": [
      { "name": "date_parsed", "logical_type": "date", "nullable": true },
      { "name": "country", "logical_type": "string", "nullable": true },
      { "name": "active_users", "logical_type": "integer", "nullable": true }
    ],
    "scratchpad": {
      "session_id": "analysis_2026_02_23",
      "pagination_mode": "wrapped_sql",
      "page_size": 100,
      "offset": 0
    }
  }
}
```

6. Table profile helpers and cleanup

- `scratchpad_list_tables` includes a bounded `schema_summary` for each table (`column_count`, column metadata, and `columns_truncated`) so many workflows can skip per-table describe calls.
- `scratchpad_drop_table` drops a table and reclaims table-slot/row quota in the session (use this to free slot capacity for more ingests).
- `scratchpad_describe_table` for schema introspection
- `scratchpad_summarize_table` for column statistics
- `scratchpad_release_regression_report` for pre/transition/post release diagnostics with confidence flags
- `scratchpad_landing_param_shift_report` for pre-vs-post landing URL parameter shift diagnostics
- `scratchpad_export_evidence_bundle` for JSON+Markdown shareable evidence packs

Example args:

```json
{
  "session_id": "analysis_2026_02_23",
  "table_name": "daily_engagement",
  "output_mode": "compact"
}
```

Release-regression helper example:

```json
{
  "session_id": "analysis_2026_02_23",
  "table_name": "daily_engagement",
  "release_date": "2026-02-24",
  "anchor_event": "signup_complete",
  "comparison_event": "session_start",
  "date_column": "date",
  "event_column": "event_name",
  "metric_column": "event_count"
}
```

Release-regression outputs include statistical context fields for pre/post ratio stability:

- `ratio_n_pre`, `ratio_n_post`
- `ratio_mean_pre`, `ratio_mean_post`
- `ratio_sd_pre`, `ratio_sd_post`
- `ratio_mean_delta`, `ratio_mean_delta_se`, `ratio_mean_delta_z`

Landing parameter shift helper example:

```json
{
  "session_id": "analysis_2026_02_23",
  "table_name": "daily_engagement",
  "release_date": "2026-02-24",
  "date_column": "date_parsed",
  "landing_url_column": "landingpageplusquerystring",
  "channel_column": "sessiondefaultchannelgroup",
  "source_medium_column": "sessionsourcemedium",
  "top_n": 100
}
```

Evidence bundle helper example:

```json
{
  "session_id": "analysis_2026_02_23",
  "table_names": ["daily_engagement"],
  "sample_rows_per_table": 20
}
```

Drop-table helper example:

```json
{
  "session_id": "analysis_2026_02_23",
  "table_name": "daily_engagement",
  "if_exists": true
}
```

7. Close session

Tool: `scratchpad_close_session`

```json
{
  "session_id": "analysis_2026_02_23"
}
```

## Cursor Rules

- Treat `meta.next_cursor` as opaque.
- Reuse the same SQL and tool arguments when passing `cursor`.
- Cursor/query mismatches fail with `reason=invalid_cursor`.

## Safety and Limits

- Scratchpad SQL is read-only and policy-restricted.
- Session, table, row, SQL-size, memory, and timeout limits are enforced fail-closed.
- Session data is isolated by `session_id` and expires by configured TTL.

## Troubleshooting

- `SCRATCHPAD_SQL_REJECTED`: query violates read-only policy or includes restricted DuckDB constructs.
- `SCRATCHPAD_QUERY_TIMEOUT`: reduce query complexity or increase timeout config.
- `SCRATCHPAD_LIMIT_EXCEEDED`: reduce ingest/query size or close old sessions/tables.
- `SCRATCHPAD_SESSION_NOT_FOUND`: open session before list/query/ingest.

### Scratchpad Runtime-Limit Regression Triage

Use this flow when validation or CI reports intermittent failures around scratchpad ingest/session limits.

1. Re-run the exact failing integration test:

```bash
cargo test --locked --test scratchpad_integration_load scratchpad_ingest_rejects_rows_beyond_session_bound -- --nocapture
```

2. Re-run the full suite to check for broader side effects:

```bash
cargo test --locked
```

3. Probe for flakiness with repeated exact runs:

```bash
for i in $(seq 1 30); do
  cargo test --locked --test scratchpad_integration_load scratchpad_ingest_rejects_rows_beyond_session_bound -- --exact
done
```

Interpretation guidance:

- Fails once but passes repeated runs: treat as transient/stale artifact first; record residual risk and keep diagnostics hardening open.
- Fails repeatedly: treat as deterministic regression and prioritize a minimal fix with a regression test.
- If failures mention quota drift (`tables_used` or `rows_used`), inspect `reserve_ingest_capacity` and `rollback_ingest_capacity` behavior in `src/scratchpad.rs`.
