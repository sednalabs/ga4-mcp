# Release Readiness Checklist — Contract V1

Date: 2026-02-24

## Scope

Initial GA4 MCP rollout with Contract V1 full metadata and DuckDB scratchpad surface.

## Checklist

- [x] Contract policy is V1-only (no legacy mode).
- [x] Tool schema snapshot regenerated from current build.
- [x] Core validation commands executed in clean sequence.
- [x] Read-only and scratchpad schema snapshots regenerated for their respective capability profiles.
- [x] Policy gating (`read_only` vs `scratchpad`) documented.
- [x] Operator runbook with request/response examples published.
- [x] Public documentation front door reviewed for release-facing links.

## Verification Commands

Executed:

```bash
cargo fmt --all
cargo check
cargo test
./scripts/sql_policy_toolkit_conformance.sh
cargo run --bin ga4-mcp -- --print-tool-schema > spec/tool_schema_snapshot.v1.json
cargo run --bin ga4-mcp -- --capability-profile scratchpad --print-tool-schema > spec/tool_schema_snapshot.scratchpad.v1.json
```

Observed result:

- `cargo check`: pass
- `cargo test`: pass
- `sql_policy_toolkit_conformance`: pass
- schema snapshot generation: pass
- scratchpad schema snapshot generation: pass

## Read-Only Tool Inventory (from `spec/tool_schema_snapshot.v1.json`)

- `get_account_summaries`
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

## Scratchpad Tool Inventory (from `spec/tool_schema_snapshot.scratchpad.v1.json`)

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
