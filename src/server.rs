//! # MCP Server Handler
//!
//! MCP protocol handler that exposes Google Analytics read/report tools.

use std::future::Future;
use std::sync::Arc;

use axum::http::request::Parts;
use mcp_toolkit_core::rmcp_models;
use mcp_toolkit_core::tool_schema::tool_schema_snapshot_value;
use mcp_toolkit_observability::{EventContext, Level, emit_event, safe_error, safe_text};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};
use serde_json::Value;

use crate::config::{CapabilityProfile, UpstreamTokenSource};
use crate::contract;
use crate::error::AnalyticsError;
use crate::ga_client::{AnalyticsApiClient, with_request_access_token_override};
use crate::scratchpad::SharedScratchpadSessionManager;

#[derive(Clone)]
pub struct AnalyticsMcp {
    pub client: Arc<AnalyticsApiClient>,
    pub scratchpad_sessions: SharedScratchpadSessionManager,
    capability_profile: CapabilityProfile,
    upstream_token_source: UpstreamTokenSource,
    upstream_token_header: String,
    tool_router: ToolRouter<AnalyticsMcp>,
}

impl AnalyticsMcp {
    pub fn new(
        client: Arc<AnalyticsApiClient>,
        scratchpad_sessions: SharedScratchpadSessionManager,
        capability_profile: CapabilityProfile,
        upstream_token_source: UpstreamTokenSource,
        upstream_token_header: String,
    ) -> Self {
        Self {
            client,
            scratchpad_sessions,
            capability_profile,
            upstream_token_source,
            upstream_token_header,
            tool_router: Self::tool_router_analytics(),
        }
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.visible_tools()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    pub fn tool_schema_snapshot(&self) -> Value {
        tool_schema_snapshot_value(&self.visible_tools())
            .expect("registered tool definitions should serialize")
    }

    fn visible_tools(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router
            .list_all()
            .into_iter()
            .filter(|tool| self.is_tool_allowed(tool.name.as_ref()))
            .collect()
    }

    fn is_tool_allowed(&self, tool_name: &str) -> bool {
        tool_allowed_for_profile(self.capability_profile, tool_name)
    }

    pub fn accepts_upstream_request_tokens(&self) -> bool {
        self.upstream_token_source.uses_request_header()
    }

    pub fn capability_profile(&self) -> CapabilityProfile {
        self.capability_profile
    }
}

impl ServerHandler for AnalyticsMcp {
    fn get_info(&self) -> ServerInfo {
        rmcp_models::server_info(
            ProtocolVersion::V_2024_11_05,
            ServerCapabilities::builder().enable_tools().build(),
            Implementation::from_build_env(),
            Some(
                "Google Analytics MCP Rust server. Use read/report tools against GA4 properties."
                    .to_string(),
            ),
        )
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, rmcp::ErrorData>> + Send + '_ {
        let tools = self.visible_tools();
        std::future::ready(Ok(ListToolsResult {
            meta: None,
            tools,
            next_cursor: None,
        }))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_ {
        let tool_name = request.name.to_string();
        let token_source = self.upstream_token_source;
        let token_header = self.upstream_token_header.clone();
        let request_token = upstream_token_override_from_context(
            &context,
            self.upstream_token_source,
            &self.upstream_token_header,
        );
        let tool_context = ToolCallContext::new(self, request, context);
        async move {
            if !self.is_tool_allowed(&tool_name) {
                emit_event(
                    Level::WARN,
                    "ga4_mcp.tool.blocked",
                    &EventContext::new().with_tool_name(&tool_name),
                    &[
                        safe_text("tool", &tool_name),
                        safe_text("capability_profile", self.capability_profile.as_str()),
                    ],
                );
                let err =
                    AnalyticsError::policy_denied(self.capability_profile.as_str(), tool_name);
                let result = contract::error(err, 0);
                return Ok(result);
            }

            emit_event(
                Level::INFO,
                "ga4_mcp.tool.start",
                &EventContext::new().with_tool_name(&tool_name),
                &[safe_text("tool", &tool_name)],
            );
            emit_event(
                Level::INFO,
                "ga4_mcp.upstream_auth.context",
                &EventContext::new().with_tool_name(&tool_name),
                &[
                    safe_text("tool", &tool_name),
                    safe_text("token_source", token_source.as_str()),
                    safe_text("token_header", &token_header),
                    safe_text(
                        "request_token_present",
                        if request_token.is_some() {
                            "true"
                        } else {
                            "false"
                        },
                    ),
                ],
            );

            let result = with_request_access_token_override(
                request_token,
                self.tool_router.call(tool_context),
            )
            .await;

            match &result {
                Ok(tool_result) => {
                    emit_event(
                        Level::INFO,
                        "ga4_mcp.tool.finish",
                        &EventContext::new().with_tool_name(&tool_name),
                        &[safe_text("tool", &tool_name)],
                    );
                    emit_contract_observability(&tool_name, tool_result);
                }
                Err(err) => emit_event(
                    Level::WARN,
                    "ga4_mcp.tool.error",
                    &EventContext::new().with_tool_name(&tool_name),
                    &[safe_error("error", err), safe_text("tool", &tool_name)],
                ),
            }

            result
        }
    }
}

fn upstream_token_override_from_context(
    context: &RequestContext<RoleServer>,
    source: UpstreamTokenSource,
    header_name: &str,
) -> Option<String> {
    if !source.uses_request_header() {
        return None;
    }
    context
        .extensions
        .get::<Parts>()
        .and_then(|parts| upstream_token_override_from_parts(parts, header_name))
}

fn upstream_token_override_from_parts(parts: &Parts, header_name: &str) -> Option<String> {
    parts
        .headers
        .get(header_name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn tool_allowed_for_profile(profile: CapabilityProfile, tool_name: &str) -> bool {
    profile.allows_tool(tool_name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContractSummary {
    ok: Option<bool>,
    has_data: bool,
    has_meta: bool,
    has_error: bool,
    error_code: Option<String>,
    error_reason: Option<String>,
    output_mode: Option<String>,
    row_count_total: Option<u64>,
    row_count_returned: Option<u64>,
    truncated: Option<bool>,
    next_cursor_present: Option<bool>,
}

fn emit_contract_observability(tool_name: &str, result: &CallToolResult) {
    let context = EventContext::new().with_tool_name(tool_name);
    let Some(summary) = summarize_contract_payload(result) else {
        emit_event(
            Level::WARN,
            "ga4_mcp.contract.missing_payload",
            &context,
            &[safe_text("tool", tool_name)],
        );
        return;
    };

    emit_event(
        if summary.ok == Some(false) {
            Level::WARN
        } else {
            Level::INFO
        },
        "ga4_mcp.contract.response",
        &context,
        &[
            safe_text("tool", tool_name),
            safe_text("ok", bool_label(summary.ok)),
            safe_text("has_data", bool_label(Some(summary.has_data))),
            safe_text("has_meta", bool_label(Some(summary.has_meta))),
            safe_text("has_error", bool_label(Some(summary.has_error))),
            safe_text(
                "error_code",
                summary.error_code.as_deref().unwrap_or("none"),
            ),
            safe_text(
                "error_reason",
                summary.error_reason.as_deref().unwrap_or("none"),
            ),
        ],
    );

    if summary.row_count_total.is_some() || summary.row_count_returned.is_some() {
        emit_event(
            Level::INFO,
            "ga4_mcp.pagination.meta",
            &context,
            &[
                safe_text("tool", tool_name),
                safe_text(
                    "output_mode",
                    summary.output_mode.as_deref().unwrap_or("unknown"),
                ),
                safe_text(
                    "row_count_total",
                    summary
                        .row_count_total
                        .map(|value| value.to_string())
                        .as_deref()
                        .unwrap_or("unknown"),
                ),
                safe_text(
                    "row_count_returned",
                    summary
                        .row_count_returned
                        .map(|value| value.to_string())
                        .as_deref()
                        .unwrap_or("unknown"),
                ),
                safe_text("truncated", bool_label(summary.truncated)),
                safe_text(
                    "next_cursor_present",
                    bool_label(summary.next_cursor_present),
                ),
            ],
        );
    }

    if summary.error_reason.as_deref() == Some("invalid_cursor") {
        emit_event(
            Level::WARN,
            "ga4_mcp.pagination.cursor_error",
            &context,
            &[
                safe_text("tool", tool_name),
                safe_text(
                    "error_code",
                    summary.error_code.as_deref().unwrap_or("unknown"),
                ),
                safe_text(
                    "error_reason",
                    summary.error_reason.as_deref().unwrap_or("invalid_cursor"),
                ),
            ],
        );
    }
}

fn summarize_contract_payload(result: &CallToolResult) -> Option<ContractSummary> {
    let payload = result.structured_content.as_ref()?;
    let object = payload.as_object()?;
    let error_object = object.get("error").and_then(Value::as_object);
    let meta = object.get("meta").and_then(Value::as_object);

    Some(ContractSummary {
        ok: object.get("ok").and_then(Value::as_bool),
        has_data: object.contains_key("data"),
        has_meta: object.contains_key("meta"),
        has_error: object.contains_key("error"),
        error_code: error_object
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string),
        error_reason: error_object
            .and_then(|error| error.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string),
        output_mode: meta
            .and_then(|value| value.get("output_mode"))
            .and_then(Value::as_str)
            .map(str::to_string),
        row_count_total: meta
            .and_then(|value| value.get("row_count_total"))
            .and_then(Value::as_u64),
        row_count_returned: meta
            .and_then(|value| value.get("row_count_returned"))
            .and_then(Value::as_u64),
        truncated: meta
            .and_then(|value| value.get("truncated"))
            .and_then(Value::as_bool),
        next_cursor_present: meta
            .and_then(|value| value.get("next_cursor"))
            .map(|value| !value.is_null()),
    })
}

fn bool_label(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use serde_json::json;

    fn request_parts_with_headers(headers: &[(&str, &str)]) -> Parts {
        let mut request = Request::builder().uri("http://127.0.0.1:9420/mcp");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.body(()).expect("request should build");
        let (parts, _) = request.into_parts();
        parts
    }

    #[test]
    fn tool_allowed_for_profile_blocks_scratchpad_when_read_only() {
        assert!(tool_allowed_for_profile(
            CapabilityProfile::ReadOnly,
            "run_report"
        ));
        assert!(!tool_allowed_for_profile(
            CapabilityProfile::ReadOnly,
            "scratchpad_query"
        ));
        assert!(tool_allowed_for_profile(
            CapabilityProfile::Scratchpad,
            "scratchpad_query"
        ));
    }

    #[test]
    fn summarize_contract_payload_extracts_tabular_metadata() {
        let result = CallToolResult::structured(json!({
            "ok": true,
            "data": [{"country": "US"}],
            "meta": {
                "elapsed_ms": 12,
                "output_mode": "rows",
                "row_count_total": 20,
                "row_count_returned": 5,
                "truncated": true,
                "next_cursor": "v1:hash:5"
            }
        }));

        let summary = summarize_contract_payload(&result).expect("summary should parse");
        assert_eq!(summary.ok, Some(true));
        assert!(summary.has_data);
        assert!(summary.has_meta);
        assert_eq!(summary.output_mode.as_deref(), Some("rows"));
        assert_eq!(summary.row_count_total, Some(20));
        assert_eq!(summary.row_count_returned, Some(5));
        assert_eq!(summary.truncated, Some(true));
        assert_eq!(summary.next_cursor_present, Some(true));
    }

    #[test]
    fn summarize_contract_payload_extracts_error_taxonomy() {
        let result = CallToolResult::structured(json!({
            "ok": false,
            "error": {
                "code": "INVALID_CURSOR",
                "reason": "invalid_cursor",
                "message": "cursor mismatch",
                "category": "cursor"
            },
            "meta": {
                "elapsed_ms": 1
            }
        }));

        let summary = summarize_contract_payload(&result).expect("summary should parse");
        assert_eq!(summary.ok, Some(false));
        assert_eq!(summary.error_code.as_deref(), Some("INVALID_CURSOR"));
        assert_eq!(summary.error_reason.as_deref(), Some("invalid_cursor"));
        assert!(summary.has_error);
    }

    #[test]
    fn summarize_contract_payload_returns_none_for_non_structured_results() {
        let result = CallToolResult::success(Vec::new());
        assert!(summarize_contract_payload(&result).is_none());
    }

    #[test]
    fn upstream_token_override_reads_authorization_header() {
        let parts = request_parts_with_headers(&[("authorization", "Bearer token-123")]);
        let token = upstream_token_override_from_parts(&parts, "authorization");
        assert_eq!(token.as_deref(), Some("Bearer token-123"));
    }

    #[test]
    fn upstream_token_override_ignores_headers_in_config_mode() {
        let parts = request_parts_with_headers(&[("authorization", "Bearer token-123")]);
        let token = upstream_token_override_from_parts(&parts, "x-google-access-token");
        assert!(token.is_none(), "wrong header should not produce token");

        assert!(matches!(UpstreamTokenSource::Config, source if !source.uses_request_header()));
    }
}
