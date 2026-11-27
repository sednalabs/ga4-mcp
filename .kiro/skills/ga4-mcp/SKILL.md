---
name: ga4-mcp
description: Use the ga4-mcp server for read-only Google Analytics 4 reporting. Use for authentication troubleshooting, realtime traffic, historical page and acquisition reports, and bounded DuckDB scratchpad analysis.
---

# Google Analytics 4 reporting

Discover the configured MCP server and use its exposed tool schemas. The
client may namespace the tool names. Confirm the target GA4 property and its
reporting timezone with `get_property_details`. If the property is unclear,
look up the properties visible to the configured Google identity with
`get_account_summaries` and resolve any ambiguity before running reports.
Use a property ID or `properties/<id>`, as accepted by the tool schema.

## Authenticate and diagnose errors

- Use `ga4_get_started` for setup and `ga4_auth_status` for the configured
  credential source. Use `verify_token: true` when checking the configured
  identity's Google API access. The requested property still needs its own
  access check.
- Distinguish MCP connection or authorization failures from upstream Google
  authentication and property permissions. Use `ga4_auth_login_command` for
  supported read-only sign-in when the configured local credential needs
  reauthorization. For browser OAuth using a loopback callback, use the browser
  on the host running the listener or follow the supported headless flow.
  For hosted per-user auth, follow the client's configured Google token flow.
- Treat `invalid_grant` as a credential issue. If Google denies access, check
  the active identity, property access and the reported error reason before
  retrying. For a report validation error, correct the request. For a quota
  error, reduce request volume and follow the provider's retry guidance.
- Read `ok` first. For a tool error, use `error.reason`, `error.code` and
  `error.status_code` when present. Keep credentials, sign-in and callback URLs,
  and private report rows out of chat, issue reports and version control.

## Choose the report

| Question | Starting point |
| --- | --- |
| How much traffic is arriving now? | Use `run_realtime_report` with `screenPageViews` or `activeUsers`. Add `unifiedScreenName` to group or filter by page title and screen name. |
| Which URLs received views over a date range? | Use `run_report` with `pagePath`, `screenPageViews` and explicit inclusive dates in the property timezone. Add `pageTitle` when it helps distinguish content and error-page traffic. |
| Where did sessions come from? | Use `run_report` with `sessionSourceMedium` and an appropriate metric, such as `sessions` for sessions or `screenPageViews` for views attributed to the session source. Recent standard data can lag Realtime. |

For Core reports, use `preview_report_request` to check the request shape
without calling Google and `check_report_compatibility` to check a proposed
dimension/metric combination with Google. For Realtime, check the separate
[Google Realtime schema](https://developers.google.com/analytics/devguides/reporting/data/v1/realtime-api-schema).
Core dimensions such as `pagePath` and `sessionSourceMedium` are unavailable
in Realtime. Leave current attribution unresolved when standard reporting has
yet to process a surge.

Start with the smallest useful report. For `run_report`, check
`meta.truncated` and continue with `meta.next_cursor` when full coverage is
needed and the cursor is present. Pass the cursor with the same report
definition. Realtime returns one bounded page and has no cursor pagination.
When available in the server schema, use `minute_ranges` to select the rolling
window (default minutes 29 through 0). Report the UTC capture time and window.
Avoid summing overlapping Realtime windows or adding active users across rows.

### Example: measure views on an error-page title

Replace the placeholder property ID and confirm the site's actual title.
Pass this request to `run_realtime_report`.

```json
{
  "property_id": "properties/PROPERTY_ID",
  "dimensions": ["unifiedScreenName"],
  "metrics": ["screenPageViews"],
  "dimension_filter": {
    "filter": {
      "fieldName": "unifiedScreenName",
      "stringFilter": {
        "matchType": "CONTAINS",
        "value": "Page not found",
        "caseSensitive": false
      }
    }
  },
  "limit": 20
}
```

Report this as views of a matching page title. Confirm affected URLs and
actual response statuses using a processed page-path report and, when
available, authorized server logs or public HTTP checks. A title can cover
multiple URLs and can be served with HTTP 200. Keep GA4 views and HTTP request
counts separate.

## Use the scratchpad for bounded analysis

When the server exposes the scratchpad profile, read
`scratchpad_get_runtime_limits` and open a task-owned session with
`scratchpad_open_session`. Use `scratchpad_ingest_report` or
`scratchpad_ingest_realtime_report` to load each needed report once, then
`scratchpad_describe_table` and `scratchpad_query` to check the stored schema
and compute aggregates. Verify ingest coverage before treating totals as
complete. Export only the evidence needed and close the task-owned session
with `scratchpad_close_session`. For a small report, query the report tool
directly.

Record the property, time window and timezone, dimensions, filters, metric
units, row coverage and material limitations with the result.

## Reference

- [Tool guide](https://github.com/sednalabs/ga4-mcp/blob/main/docs/TOOL_GUIDE.md)
- [Auth modes](https://github.com/sednalabs/ga4-mcp/blob/main/docs/auth-modes.md)
- [Scratchpad operator guide](https://github.com/sednalabs/ga4-mcp/blob/main/docs/scratchpad-operator-guide.md)
