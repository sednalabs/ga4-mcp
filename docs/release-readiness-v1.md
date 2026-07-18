# Release Readiness Checklist — Contract V1

Date: 2026-07-18

## Scope

Initial GA4 MCP rollout with Contract V1 full metadata and DuckDB scratchpad surface.

## Checklist

- [x] Contract policy is V1-only (no legacy mode).
- [x] Tool schema snapshot regenerated from current build.
- [x] Core Rust validation and tool-contract checks are green in hosted CI.
- [x] Scratchpad tools included in schema snapshot.
- [x] Policy gating (`read_only` vs `scratchpad`) documented.
- [x] Operator runbook with request/response examples published.
- [x] Public documentation front door reviewed for release-facing links.

## Verification Evidence

The [PR checks page](https://github.com/sednalabs/ga4-mcp/pull/30/checks) is the
authoritative source for final verification. The exact final head SHA and
hosted run IDs are recorded in the release handoff; individual run links below
are historical code-slice evidence and must not be read as the current PR head
after later commits.

- [Rust Validation](https://github.com/sednalabs/ga4-mcp/actions/runs/29652538484): pass for an earlier code slice.
- [Rust Cobertura coverage](https://github.com/sednalabs/ga4-mcp/actions/runs/29645947140): pass for an earlier code slice.

The tool schema snapshot was regenerated with the explicit binary target:

```bash
cargo run --bin ga4-mcp -- --print-tool-schema > spec/tool_schema_snapshot.v1.json
```

The `sql_policy_toolkit_conformance` command was not rerun for this PR because
an external companion conformance dependency was unavailable. Any earlier pass
recorded for that command is historical baseline evidence only, not current PR
verification.

## Tool Inventory (from `spec/tool_schema_snapshot.v1.json`)

- `get_account_summaries`
- `ga4_get_started`
- `ga4_auth_status`
- `ga4_auth_login_command`
- `get_account_data_sharing_settings`
- `check_report_compatibility`
- `get_custom_dimensions_and_metrics`
- `get_property_details`
- `get_property_data_retention_settings`
- `list_google_ads_links`
- `list_property_annotations`
- `run_account_access_report`
- `batch_run_reports`
- `preview_report_request`
- `run_pivot_report`
- `run_property_access_report`
- `run_realtime_report`
- `run_report`
- `run_conversions_report`
- `run_funnel_report`
- `scratchpad_close_session`
- `scratchpad_describe_table`
- `scratchpad_drop_table`
- `scratchpad_export_evidence_bundle`
- `scratchpad_get_runtime_limits`
- `scratchpad_ingest_realtime_report`
- `scratchpad_ingest_report`
- `scratchpad_landing_param_shift_report`
- `scratchpad_list_sessions`
- `scratchpad_list_tables`
- `scratchpad_open_session`
- `scratchpad_query`
- `scratchpad_release_regression_report`
- `scratchpad_set_runtime_limits`
- `scratchpad_summarize_table`

## Signoff Notes

- Contract envelope: `ok/data/meta` and `ok/error/meta` only.
- Policy denials return deterministic taxonomy (`POLICY_DENIED`, `policy_denied`).
- Cursor, output mode, summary-only, and clipping controls are active for tabular tools.
- Funnel output is bounded per subreport and explicitly avoids claiming an exact total or cursor that Google does not provide.
- Funnel and conversion reports remain read-only and use Data API v1alpha; conversion eligibility is property-dependent.
