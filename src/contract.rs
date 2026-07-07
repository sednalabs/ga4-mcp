//! # Contract Builders
//!
//! Shared helpers for Contract V1 tool response envelopes.

use std::time::Instant;

use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::error::AnalyticsError;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    Rows,
    Tuples,
    Scalar,
    Compact,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nullable: Option<bool>,
}

impl ColumnMeta {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            logical_type: None,
            nullable: None,
        }
    }

    pub fn with_logical_type(mut self, logical_type: impl Into<String>) -> Self {
        self.logical_type = Some(logical_type.into());
        self
    }

    pub fn with_nullable(mut self, nullable: bool) -> Self {
        self.nullable = Some(nullable);
        self
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TabularMeta {
    pub output_mode: OutputMode,
    pub summary_only: bool,
    pub row_count_total: usize,
    pub row_count_returned: usize,
    pub truncated: bool,
    pub next_cursor: Option<String>,
    pub query_hash: Option<String>,
    pub columns: Vec<ColumnMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_hints: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell_clipping: Option<Value>,
}

impl TabularMeta {
    pub fn rows(
        row_count_total: usize,
        row_count_returned: usize,
        columns: Vec<ColumnMeta>,
    ) -> Self {
        Self {
            output_mode: OutputMode::Rows,
            summary_only: false,
            row_count_total,
            row_count_returned,
            truncated: row_count_returned < row_count_total,
            next_cursor: None,
            query_hash: None,
            columns,
            query_hints: None,
            cell_clipping: None,
        }
    }
}

pub fn elapsed_ms(started: Instant) -> u64 {
    let elapsed = started.elapsed().as_millis();
    if elapsed > u128::from(u64::MAX) {
        u64::MAX
    } else {
        elapsed as u64
    }
}

pub fn success(data: Value, elapsed_ms: u64) -> CallToolResult {
    CallToolResult::structured(json!({
        "ok": true,
        "data": data,
        "meta": {
            "elapsed_ms": elapsed_ms,
        }
    }))
}

pub fn success_with_meta(data: Value, meta: Value, elapsed_ms: u64) -> CallToolResult {
    let meta = attach_elapsed(meta, elapsed_ms);
    CallToolResult::structured(json!({
        "ok": true,
        "data": data,
        "meta": meta,
    }))
}

pub fn success_tabular(
    data: Option<Value>,
    tabular_meta: TabularMeta,
    elapsed_ms: u64,
) -> CallToolResult {
    let data = if tabular_meta.summary_only {
        Value::Null
    } else {
        data.unwrap_or(Value::Null)
    };
    let meta = attach_elapsed(json!(tabular_meta), elapsed_ms);
    CallToolResult::structured(json!({
        "ok": true,
        "data": data,
        "meta": meta,
    }))
}

pub fn error(err: AnalyticsError, elapsed_ms: u64) -> CallToolResult {
    let mut error_obj = json!({
        "code": err.code(),
        "reason": err.reason(),
        "message": redact_secret_text(&err.to_string()),
        "category": err.category(),
    });
    if let Some(status_code) = err.status_code() {
        error_obj["status_code"] = json!(status_code);
    }
    if let Some(engine_code) = err.engine_code() {
        error_obj["engine_code"] = json!(engine_code);
    }
    if let Some(detail) = err.detail() {
        error_obj["detail"] = json!(detail);
    }
    if let Some(hint) = err.hint() {
        error_obj["hint"] = json!(hint);
    }
    if let Some(position) = err.position() {
        error_obj["position"] = json!(position);
    }

    CallToolResult::structured(json!({
        "ok": false,
        "error": error_obj,
        "meta": {
            "elapsed_ms": elapsed_ms,
        }
    }))
}

pub fn redact_secret_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for (line_index, line) in input.lines().enumerate() {
        if line_index > 0 {
            out.push('\n');
        }
        let mut first = true;
        for token in line.split_whitespace() {
            if !first {
                out.push(' ');
            }
            first = false;
            if looks_secret_bearing(token) {
                out.push_str("[redacted]");
            } else {
                out.push_str(token);
            }
        }
    }
    out
}

fn looks_secret_bearing(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    lower.contains("refresh_token")
        || lower.contains("access_token")
        || lower.contains("client_secret")
        || lower.contains("private_key")
        || lower.starts_with("ya29.")
        || lower.starts_with("1//")
        || lower.contains("1%2f%2f")
}

fn attach_elapsed(meta: Value, elapsed_ms: u64) -> Value {
    let mut map = match meta {
        Value::Object(existing) => existing,
        _ => Map::new(),
    };
    map.insert("elapsed_ms".to_string(), json!(elapsed_ms));
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AnalyticsError;

    fn payload(result: CallToolResult) -> Value {
        result
            .structured_content
            .expect("structured payload is required")
    }

    #[test]
    fn success_adds_required_meta() {
        let data = json!({ "k": "v" });
        let payload = payload(success(data.clone(), 7));

        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["data"], data);
        assert_eq!(payload["meta"]["elapsed_ms"], json!(7));
    }

    #[test]
    fn success_with_meta_keeps_fields_and_adds_elapsed() {
        let payload = payload(success_with_meta(
            json!([1, 2]),
            json!({ "row_count_total": 2 }),
            11,
        ));

        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["meta"]["row_count_total"], json!(2));
        assert_eq!(payload["meta"]["elapsed_ms"], json!(11));
    }

    #[test]
    fn error_contains_contract_fields() {
        let err = AnalyticsError::invalid("property_id", "missing");
        let payload = payload(error(err, 3));

        assert_eq!(payload["ok"], Value::Bool(false));
        assert_eq!(payload["error"]["code"], json!("INVALID_PARAMS"));
        assert_eq!(payload["error"]["reason"], json!("invalid_params"));
        assert_eq!(payload["error"]["category"], json!("validation"));
        assert_eq!(payload["error"]["detail"], json!("field=property_id"));
        assert!(payload["error"]["hint"].is_string());
        assert!(payload["error"]["message"].is_string());
        assert_eq!(payload["meta"]["elapsed_ms"], json!(3));
    }

    #[test]
    fn redacts_google_tokens_without_flattening_lines() {
        let text = "first ya29.secret\nsecond refresh_token=1//refresh\nthird token=1%2F%2Fencoded";
        let redacted = redact_secret_text(text);

        assert_eq!(redacted.lines().count(), 3);
        assert!(!redacted.contains("ya29.secret"));
        assert!(!redacted.contains("1//refresh"));
        assert!(!redacted.contains("1%2F%2Fencoded"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn success_tabular_respects_summary_only() {
        let mut meta = TabularMeta::rows(10, 5, vec![ColumnMeta::new("event_name")]);
        meta.summary_only = true;
        meta.next_cursor = Some("v1:abc:5".to_string());

        let payload = payload(success_tabular(
            Some(json!([{"event_name": "page_view"}])),
            meta,
            9,
        ));
        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["data"], Value::Null);
        assert_eq!(payload["meta"]["summary_only"], Value::Bool(true));
        assert_eq!(payload["meta"]["next_cursor"], json!("v1:abc:5"));
        assert_eq!(payload["meta"]["elapsed_ms"], json!(9));
    }
}
