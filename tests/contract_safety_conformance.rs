use ga4_mcp::contract::{self, ColumnMeta, OutputMode, TabularMeta};
use ga4_mcp::error::AnalyticsError;
use ga4_mcp::sql_safety::{ScratchpadSqlPolicyCode, validate_scratchpad_sql};
use serde_json::{Value, json};

fn payload(result: rmcp::model::CallToolResult) -> Value {
    result
        .structured_content
        .expect("structured payload must be present")
}

fn tool_input_property_names(snapshot: &Value, tool_name: &str) -> Vec<String> {
    snapshot["tools"]
        .as_array()
        .expect("tools array should exist")
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(tool_name))
        .unwrap_or_else(|| panic!("tool {tool_name} missing from snapshot"))
        .get("inputSchema")
        .and_then(|schema| schema.get("properties"))
        .and_then(Value::as_object)
        .expect("input schema properties should exist")
        .keys()
        .cloned()
        .collect()
}

fn tool_input_schema(snapshot: &Value, tool_name: &str) -> Value {
    snapshot["tools"]
        .as_array()
        .expect("tools array should exist")
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(tool_name))
        .unwrap_or_else(|| panic!("tool {tool_name} missing from snapshot"))
        .get("inputSchema")
        .cloned()
        .expect("input schema should exist")
}

fn tool_names(snapshot: &Value) -> Vec<String> {
    snapshot["tools"]
        .as_array()
        .expect("tools array should exist")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

#[test]
fn contract_success_tabular_emits_required_v1_fields() {
    let mut tabular = TabularMeta::rows(100, 25, vec![ColumnMeta::new("event_name")]);
    tabular.output_mode = OutputMode::Compact;
    tabular.summary_only = false;
    tabular.next_cursor = Some("v1:abcd:25".to_string());
    tabular.query_hash = Some("abcd".to_string());

    let payload = payload(contract::success_tabular(
        Some(json!([{ "event_name": "page_view" }])),
        tabular,
        8,
    ));

    assert_eq!(payload["ok"], Value::Bool(true));
    assert!(payload["meta"]["elapsed_ms"].is_number());
    assert_eq!(payload["meta"]["output_mode"], json!("compact"));
    assert_eq!(payload["meta"]["summary_only"], Value::Bool(false));
    assert_eq!(payload["meta"]["row_count_total"], json!(100));
    assert_eq!(payload["meta"]["row_count_returned"], json!(25));
    assert_eq!(payload["meta"]["truncated"], Value::Bool(true));
    assert_eq!(payload["meta"]["next_cursor"], json!("v1:abcd:25"));
    assert_eq!(payload["meta"]["query_hash"], json!("abcd"));
}

#[test]
fn contract_error_policy_denied_is_stable() {
    let payload = payload(contract::error(
        AnalyticsError::policy_denied("read_only", "scratchpad_query"),
        3,
    ));

    assert_eq!(payload["ok"], Value::Bool(false));
    assert_eq!(payload["error"]["code"], json!("POLICY_DENIED"));
    assert_eq!(payload["error"]["reason"], json!("policy_denied"));
    assert_eq!(payload["error"]["category"], json!("policy"));
    assert_eq!(
        payload["error"]["detail"],
        json!("profile=read_only;tool=scratchpad_query")
    );
    assert!(payload["error"]["hint"].is_string());
    assert_eq!(payload["meta"]["elapsed_ms"], json!(3));
}

#[test]
fn cursor_query_mismatch_maps_to_invalid_cursor_reason() {
    let payload = payload(contract::error(AnalyticsError::CursorQueryMismatch, 5));
    assert_eq!(payload["error"]["code"], json!("CURSOR_QUERY_MISMATCH"));
    assert_eq!(payload["error"]["reason"], json!("invalid_cursor"));
    assert_eq!(payload["error"]["category"], json!("cursor"));
}

#[test]
fn sql_safety_rejects_duckdb_extension_and_external_scan() {
    let install_err =
        validate_scratchpad_sql("INSTALL httpfs", 65_536).expect_err("INSTALL should be rejected");
    assert!(
        matches!(
            install_err.code,
            ScratchpadSqlPolicyCode::NotReadOnlyPrefix
                | ScratchpadSqlPolicyCode::DuckDbForbiddenKeyword
        ),
        "unexpected policy code for INSTALL: {:?}",
        install_err.code
    );

    let scan_err = validate_scratchpad_sql("SELECT * FROM read_csv_auto('x.csv')", 65_536)
        .expect_err("external scan should be rejected");
    assert!(
        matches!(
            scan_err.code,
            ScratchpadSqlPolicyCode::ForbiddenFunction
                | ScratchpadSqlPolicyCode::DuckDbForbiddenFunction
        ),
        "unexpected policy code for read_csv_auto: {:?}",
        scan_err.code
    );
}

#[test]
fn tool_snapshot_exposes_tabular_request_controls_for_contract_v1() {
    let snapshot_raw = std::fs::read_to_string("spec/tool_schema_snapshot.v1.json")
        .expect("tool schema snapshot should be readable");
    let snapshot: Value =
        serde_json::from_str(&snapshot_raw).expect("tool schema snapshot should be valid JSON");
    let scratchpad_snapshot_raw =
        std::fs::read_to_string("spec/tool_schema_snapshot.scratchpad.v1.json")
            .expect("scratchpad tool schema snapshot should be readable");
    let scratchpad_snapshot: Value = serde_json::from_str(&scratchpad_snapshot_raw)
        .expect("scratchpad tool schema snapshot should be valid JSON");

    let default_tool_names = tool_names(&snapshot);
    assert!(
        !default_tool_names.iter().any(|name| name == "scratchpad_query"),
        "default read_only snapshot must not advertise scratchpad_query"
    );

    let run_report_keys = tool_input_property_names(&snapshot, "run_report");
    for required in [
        "max_rows",
        "cursor",
        "output_mode",
        "summary_only",
        "max_cell_chars",
    ] {
        assert!(
            run_report_keys.iter().any(|name| name == required),
            "run_report is missing required control: {required}"
        );
    }

    let scratchpad_query_keys =
        tool_input_property_names(&scratchpad_snapshot, "scratchpad_query");
    for required in [
        "max_rows",
        "cursor",
        "output_mode",
        "summary_only",
        "max_cell_chars",
    ] {
        assert!(
            scratchpad_query_keys.iter().any(|name| name == required),
            "scratchpad_query is missing required control: {required}"
        );
    }

    let run_pivot_report_keys = tool_input_property_names(&snapshot, "run_pivot_report");
    for required in [
        "max_rows",
        "cursor",
        "output_mode",
        "summary_only",
        "max_cell_chars",
    ] {
        assert!(
            run_pivot_report_keys.iter().any(|name| name == required),
            "run_pivot_report is missing required control: {required}"
        );
    }

    let run_property_access_report_keys =
        tool_input_property_names(&snapshot, "run_property_access_report");
    for required in [
        "max_rows",
        "cursor",
        "output_mode",
        "summary_only",
        "max_cell_chars",
    ] {
        assert!(
            run_property_access_report_keys
                .iter()
                .any(|name| name == required),
            "run_property_access_report is missing required control: {required}"
        );
    }

    let run_account_access_report_keys =
        tool_input_property_names(&snapshot, "run_account_access_report");
    for required in [
        "max_rows",
        "cursor",
        "output_mode",
        "summary_only",
        "max_cell_chars",
    ] {
        assert!(
            run_account_access_report_keys
                .iter()
                .any(|name| name == required),
            "run_account_access_report is missing required control: {required}"
        );
    }

    let batch_schema = tool_input_schema(&snapshot, "batch_run_reports");
    assert!(
        batch_schema["properties"]["requests"].is_object(),
        "batch_run_reports must expose requests array input"
    );
}
