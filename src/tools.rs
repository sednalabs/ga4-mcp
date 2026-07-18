//! # Tool Handlers
//!
//! MCP tools for GA4 Admin/Data API interactions.

use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

use duckdb::arrow::datatypes::DataType as DuckDataType;
use duckdb::types::Value as DuckValue;
use mcp_toolkit_observability::{EventContext, Level, emit_event, safe_error, safe_text};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};

use crate::auth_ux::{
    auth_login_cli_command, google_provider_auth_config, local_credential_material_detected,
    login_command_for_scope_with_cloudsdk, quota_project_command_with_cloudsdk,
};
use crate::config::{
    DEFAULT_ANALYTICS_SCOPE, UpstreamTokenSource, conventional_adc_credentials_path,
    server_adc_credentials_path, server_cloudsdk_config_dir,
};
use crate::contract;
use crate::error::AnalyticsError;
use crate::ga_client::{
    AccountId, AuthSource, BatchRunReportItemRequest, BatchRunReportsRequest, PaginationOptions,
    PropertyId, RunAccessReportRequest, RunConversionsReportRequest, RunFunnelReportRequest,
    RunPivotReportRequest, RunRealtimeReportRequest, RunReportRequest, normalize_funnel_step,
    snake_to_camel_json, sort_object,
};
use crate::scratchpad::{ScratchpadIngestColumn, ScratchpadIngestMode, ScratchpadTableInfo};
use crate::server::AnalyticsMcp;

const MAX_REPORT_LIMIT: u64 = 250_000;
const MAX_FUNNEL_SEGMENTS: usize = 4;
const MAX_FUNNEL_BREAKDOWN_LIMIT: u64 = 15;
const MAX_FUNNEL_NEXT_ACTION_LIMIT: u64 = 5;
const MAX_BATCH_REPORT_REQUESTS: usize = 5;
const MAX_DIMENSIONS: usize = 50;
const MAX_METRICS: usize = 50;
const DEFAULT_TABULAR_MAX_ROWS: usize = 200;
const MAX_TABULAR_MAX_ROWS: usize = 25_000;
const MAX_CELL_CHARS_LIMIT: usize = 16_384;
const DEFAULT_SCRATCHPAD_LIST_LIMIT: usize = 50;
const MAX_SCRATCHPAD_LIST_LIMIT: usize = 200;
const MAX_SCRATCHPAD_INGEST_COLUMNS: usize = 256;
const MAX_SCRATCHPAD_INGEST_ROWS: usize = 100_000;
const DEFAULT_SCRATCHPAD_INGEST_PAGE_SIZE: u64 = MAX_TABULAR_MAX_ROWS as u64;
const MAX_SCRATCHPAD_INGEST_PAGES: usize = 128;
const DEFAULT_RELEASE_PRE_DAYS: u32 = 7;
const DEFAULT_RELEASE_TRANSITION_DAYS: u32 = 1;
const DEFAULT_RELEASE_POST_DAYS: u32 = 7;
const MAX_RELEASE_WINDOW_DAYS: u32 = 90;
const DEFAULT_LANDING_SHIFT_TOP_N: usize = 100;
const MAX_LANDING_SHIFT_TOP_N: usize = 1_000;
const DEFAULT_EVIDENCE_SAMPLE_ROWS: usize = 20;
const MAX_EVIDENCE_SAMPLE_ROWS: usize = 200;
const MAX_EVIDENCE_TABLES: usize = 50;
const MAX_MEMORY_PRESSURE_SAMPLE_SESSIONS: usize = 64;
const MEMORY_PRESSURE_MODERATE_PCT: f64 = 60.0;
const MEMORY_PRESSURE_HIGH_PCT: f64 = 80.0;
const MEMORY_PRESSURE_CRITICAL_PCT: f64 = 95.0;
const SCRATCHPAD_QUERY_ALIAS: &str = "ga4_scratchpad_query";

const CONVERSION_DIMENSIONS: &[&str] = &[
    "campaignName",
    "continent",
    "country",
    "defaultChannelGroup",
    "deviceCategory",
    "medium",
    "platform",
    "primaryChannelGroup",
    "source",
    "sourceMedium",
    "sourcePlatform",
    "subcontinent",
];

const CONVERSION_METRICS: &[&str] = &[
    "advertiserAdClicks",
    "advertiserAdCost",
    "advertiserAdCostPerAllConversionsByConversionDate",
    "advertiserAdCostPerAllConversionsByInteractionDate",
    "advertiserAdCostPerClick",
    "advertiserAdImpressions",
    "allConversionsByConversionDate",
    "allConversionsByInteractionDate",
    "returnOnAdSpendByConversionDate",
    "returnOnAdSpendByInteractionDate",
    "totalRevenueByConversionDate",
    "totalRevenueByInteractionDate",
];

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuthStatusArgs {
    /// When true, acquire a Google access token and call GA account summaries. The token is never returned.
    #[serde(default)]
    pub verify_token: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuthLoginCommandArgs {
    /// Include the headless browser flag for SSH or remote environments.
    #[serde(default)]
    pub headless: bool,
    /// Optional Google OAuth client id file for direct browser OAuth.
    #[serde(default)]
    pub client_id_file: Option<String>,
    /// Optional Google account hint for browser login.
    #[serde(default)]
    pub account: Option<String>,
    /// Optional fixed loopback callback port for direct browser OAuth.
    #[serde(default)]
    pub callback_port: Option<u16>,
    /// Optional quota project to include as a follow-up command.
    #[serde(default)]
    pub quota_project: Option<String>,
    /// Use the conventional shared gcloud ADC file instead of the server-specific file.
    #[serde(default)]
    pub shared_adc: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TabularOutputMode {
    Rows,
    Tuples,
    Scalar,
    Compact,
}

impl TabularOutputMode {
    const ALLOWED_VALUES: &'static [&'static str] = &["rows", "tuples", "scalar", "compact"];

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "rows" => Some(Self::Rows),
            "tuples" => Some(Self::Tuples),
            "scalar" => Some(Self::Scalar),
            "compact" => Some(Self::Compact),
            _ => None,
        }
    }

    fn invalid_value_message(raw: &str) -> String {
        format!(
            "output_mode must be one of [{}]; got {:?}. Example: {{\"output_mode\":\"rows\"}}",
            Self::ALLOWED_VALUES.join(", "),
            raw
        )
    }
}

impl From<TabularOutputMode> for contract::OutputMode {
    fn from(value: TabularOutputMode) -> Self {
        match value {
            TabularOutputMode::Rows => contract::OutputMode::Rows,
            TabularOutputMode::Tuples => contract::OutputMode::Tuples,
            TabularOutputMode::Scalar => contract::OutputMode::Scalar,
            TabularOutputMode::Compact => contract::OutputMode::Compact,
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct GetAccountSummariesArgs {
    /// Optional page size used during pagination. Defaults to server max.
    #[serde(default)]
    pub page_size: Option<u32>,
    /// Optional max pages to fetch before stopping.
    #[serde(default)]
    pub max_pages: Option<u32>,
}

#[derive(Debug, Clone, JsonSchema)]
pub struct ReportDateRangeSchema {
    /// Inclusive start date (for example `2026-02-01`, `7daysAgo`).
    pub start_date: String,
    /// Inclusive end date (for example `2026-02-07`, `yesterday`).
    pub end_date: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PropertyArgs {
    /// The Google Analytics property id as integer or "properties/<id>".
    pub property_id: PropertyId,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckReportCompatibilityArgs {
    /// The Google Analytics property id as integer or "properties/<id>".
    pub property_id: PropertyId,
    /// Requested dimensions for preflight compatibility checks.
    pub dimensions: Vec<String>,
    /// Requested metrics for preflight compatibility checks.
    pub metrics: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccountArgs {
    /// The Google Analytics account id as integer or "accounts/<id>".
    pub account_id: AccountId,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PropertyListArgs {
    /// The Google Analytics property id as integer or "properties/<id>".
    pub property_id: PropertyId,
    /// Optional page size used during pagination. Defaults to server max.
    #[serde(default)]
    pub page_size: Option<u32>,
    /// Optional max pages to fetch before stopping.
    #[serde(default)]
    pub max_pages: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunReportArgs {
    pub property_id: PropertyId,
    #[schemars(with = "Vec<ReportDateRangeSchema>")]
    pub date_ranges: Vec<Value>,
    pub dimensions: Vec<String>,
    pub metrics: Vec<String>,
    /// GA FilterExpression. Accepts an object, JSON-object string, or `field==value` shorthand.
    #[serde(default, deserialize_with = "deserialize_optional_dimension_filter")]
    pub dimension_filter: Option<Value>,
    /// GA FilterExpression. Accepts an object or JSON-object string.
    #[serde(default, deserialize_with = "deserialize_optional_metric_filter")]
    pub metric_filter: Option<Value>,
    #[serde(default)]
    pub order_bys: Option<Vec<Value>>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub currency_code: Option<String>,
    #[serde(default = "default_true")]
    pub return_property_quota: bool,
    /// Optional response row cap for Contract V1 tabular payloads.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Opaque pagination cursor from `meta.next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Tabular output shape for the response payload.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit row payload and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to response cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AttributionModel {
    DataDriven,
    LastClick,
}

impl AttributionModel {
    fn as_str(self) -> &'static str {
        match self {
            Self::DataDriven => "DATA_DRIVEN",
            Self::LastClick => "LAST_CLICK",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversionSpecArgs {
    /// Conversion action resource names such as `conversionActions/1234`. An empty list means all conversions.
    #[serde(default)]
    pub conversion_actions: Vec<String>,
    /// Attribution model. Google defaults to DATA_DRIVEN when omitted.
    #[serde(default)]
    pub attribution_model: Option<AttributionModel>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunConversionsReportArgs {
    pub property_id: PropertyId,
    #[schemars(with = "Vec<ReportDateRangeSchema>")]
    pub date_ranges: Vec<Value>,
    /// Conversion-report dimensions. See the tool description for the supported names.
    pub dimensions: Vec<String>,
    /// Conversion-report metrics. See the tool description for the supported names.
    pub metrics: Vec<String>,
    pub conversion_spec: ConversionSpecArgs,
    /// GA FilterExpression. Accepts an object, JSON-object string, or `field==value` shorthand.
    #[serde(default, deserialize_with = "deserialize_optional_dimension_filter")]
    pub dimension_filter: Option<Value>,
    /// GA FilterExpression. Accepts an object or JSON-object string.
    #[serde(default, deserialize_with = "deserialize_optional_metric_filter")]
    pub metric_filter: Option<Value>,
    #[serde(default)]
    pub order_bys: Option<Vec<Value>>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub currency_code: Option<String>,
    #[serde(default)]
    pub return_property_quota: bool,
    /// Optional response row cap for Contract V1 tabular payloads.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Opaque pagination cursor from `meta.next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Tabular output shape for the response payload.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit row payload and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to response cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FunnelStepArgs {
    /// Display name for the funnel step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Simple event-name shorthand. Mutually exclusive with `filter_expression`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// Complete GA FunnelFilterExpression. Mutually exclusive with `event`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_expression: Option<Value>,
    /// Require this step to directly follow the prior step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_directly_followed_by: Option<bool>,
    /// Protobuf duration such as `3600s` relative to the prior step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_duration_from_prior_step: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FunnelBreakdownArgs {
    /// Dimension used to break down each funnel step, such as `deviceCategory`.
    pub breakdown_dimension: String,
    /// Maximum distinct breakdown values. Must be 1 through 15.
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FunnelNextActionArgs {
    /// Dimension used to analyze the next action, such as `eventName` or `pagePath`.
    pub next_action_dimension: String,
    /// Maximum distinct next-action values. Must be 1 through 5.
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FunnelVisualizationType {
    StandardFunnel,
    TrendedFunnel,
}

impl FunnelVisualizationType {
    fn as_str(self) -> &'static str {
        match self {
            Self::StandardFunnel => "STANDARD_FUNNEL",
            Self::TrendedFunnel => "TRENDED_FUNNEL",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunFunnelReportArgs {
    pub property_id: PropertyId,
    /// Ordered funnel steps. Each step must set exactly one of `event` or `filter_expression`.
    pub funnel_steps: Vec<FunnelStepArgs>,
    /// Allow users to enter at any funnel step. Defaults to a closed funnel.
    #[serde(default)]
    pub is_open_funnel: bool,
    /// Optional report date ranges.
    #[serde(default)]
    #[schemars(with = "Vec<ReportDateRangeSchema>")]
    pub date_ranges: Vec<Value>,
    #[serde(default)]
    pub funnel_breakdown: Option<FunnelBreakdownArgs>,
    #[serde(default)]
    pub funnel_next_action: Option<FunnelNextActionArgs>,
    #[serde(default)]
    pub funnel_visualization_type: Option<FunnelVisualizationType>,
    /// Optional GA segment objects. Google permits at most four.
    #[serde(default)]
    pub segments: Option<Vec<Value>>,
    /// GA FilterExpression applied to dimensions. Accepts an object, JSON-object string, or `field==value` shorthand.
    #[serde(default, deserialize_with = "deserialize_optional_dimension_filter")]
    pub dimension_filter: Option<Value>,
    /// Maximum rows requested from Google. Must be 1 through 250,000.
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub return_property_quota: bool,
    /// Per-subreport response row cap. Defaults to 200 and is also used to bound the upstream request.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Tabular output shape applied independently to the table and visualization subreports.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit both subreport row payloads and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to subreport cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PreviewReportRequestArgs {
    /// GA Data API report request fields.
    #[serde(flatten)]
    pub report: RunReportArgs,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunAccessReportPropertyArgs {
    pub property_id: PropertyId,
    #[schemars(with = "Vec<ReportDateRangeSchema>")]
    pub date_ranges: Vec<Value>,
    pub dimensions: Vec<String>,
    pub metrics: Vec<String>,
    /// GA FilterExpression. Accepts an object, JSON-object string, or `field==value` shorthand.
    #[serde(default, deserialize_with = "deserialize_optional_dimension_filter")]
    pub dimension_filter: Option<Value>,
    /// GA FilterExpression. Accepts an object or JSON-object string.
    #[serde(default, deserialize_with = "deserialize_optional_metric_filter")]
    pub metric_filter: Option<Value>,
    #[serde(default)]
    pub order_bys: Option<Vec<Value>>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub time_zone: Option<String>,
    /// Optional response row cap for Contract V1 tabular payloads.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Opaque pagination cursor from `meta.next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Tabular output shape for the response payload.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit row payload and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to response cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunAccessReportAccountArgs {
    pub account_id: AccountId,
    #[schemars(with = "Vec<ReportDateRangeSchema>")]
    pub date_ranges: Vec<Value>,
    pub dimensions: Vec<String>,
    pub metrics: Vec<String>,
    /// GA FilterExpression. Accepts an object, JSON-object string, or `field==value` shorthand.
    #[serde(default, deserialize_with = "deserialize_optional_dimension_filter")]
    pub dimension_filter: Option<Value>,
    /// GA FilterExpression. Accepts an object or JSON-object string.
    #[serde(default, deserialize_with = "deserialize_optional_metric_filter")]
    pub metric_filter: Option<Value>,
    #[serde(default)]
    pub order_bys: Option<Vec<Value>>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub time_zone: Option<String>,
    /// Optional response row cap for Contract V1 tabular payloads.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Opaque pagination cursor from `meta.next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Tabular output shape for the response payload.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit row payload and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to response cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunRealtimeReportArgs {
    pub property_id: PropertyId,
    pub dimensions: Vec<String>,
    pub metrics: Vec<String>,
    /// GA FilterExpression. Accepts an object, JSON-object string, or `field==value` shorthand.
    #[serde(default, deserialize_with = "deserialize_optional_dimension_filter")]
    pub dimension_filter: Option<Value>,
    /// GA FilterExpression. Accepts an object or JSON-object string.
    #[serde(default, deserialize_with = "deserialize_optional_metric_filter")]
    pub metric_filter: Option<Value>,
    #[serde(default)]
    pub order_bys: Option<Vec<Value>>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default = "default_true")]
    pub return_property_quota: bool,
    /// Optional response row cap for Contract V1 tabular payloads.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Opaque pagination cursor from `meta.next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Tabular output shape for the response payload.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit row payload and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to response cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunPivotReportArgs {
    pub property_id: PropertyId,
    #[schemars(with = "Vec<ReportDateRangeSchema>")]
    pub date_ranges: Vec<Value>,
    pub dimensions: Vec<String>,
    pub metrics: Vec<String>,
    pub pivots: Vec<Value>,
    /// GA FilterExpression. Accepts an object, JSON-object string, or `field==value` shorthand.
    #[serde(default, deserialize_with = "deserialize_optional_dimension_filter")]
    pub dimension_filter: Option<Value>,
    /// GA FilterExpression. Accepts an object or JSON-object string.
    #[serde(default, deserialize_with = "deserialize_optional_metric_filter")]
    pub metric_filter: Option<Value>,
    #[serde(default)]
    pub order_bys: Option<Vec<Value>>,
    #[serde(default)]
    pub currency_code: Option<String>,
    #[serde(default)]
    pub keep_empty_rows: bool,
    #[serde(default = "default_true")]
    pub return_property_quota: bool,
    /// Optional response row cap for Contract V1 tabular payloads.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Opaque pagination cursor from `meta.next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Tabular output shape for the response payload.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit row payload and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to response cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct BatchRunReportItemArgs {
    #[schemars(with = "Vec<ReportDateRangeSchema>")]
    pub date_ranges: Vec<Value>,
    pub dimensions: Vec<String>,
    pub metrics: Vec<String>,
    /// GA FilterExpression. Accepts an object, JSON-object string, or `field==value` shorthand.
    #[serde(default, deserialize_with = "deserialize_optional_dimension_filter")]
    pub dimension_filter: Option<Value>,
    /// GA FilterExpression. Accepts an object or JSON-object string.
    #[serde(default, deserialize_with = "deserialize_optional_metric_filter")]
    pub metric_filter: Option<Value>,
    #[serde(default)]
    pub order_bys: Option<Vec<Value>>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub currency_code: Option<String>,
    #[serde(default = "default_true")]
    pub return_property_quota: bool,
    /// Optional response row cap for Contract V1 tabular payloads.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Opaque pagination cursor from `meta.next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Tabular output shape for the response payload.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit row payload and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to response cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchRunReportsArgs {
    pub property_id: PropertyId,
    pub requests: Vec<BatchRunReportItemArgs>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadSessionArgs {
    /// Logical scratchpad session identifier.
    pub session_id: String,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ScratchpadRuntimeLimitsArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadSetRuntimeLimitsArgs {
    /// Maximum number of active scratchpad sessions allowed at runtime.
    #[serde(default)]
    pub max_sessions: Option<usize>,
    /// Maximum number of tables allowed per scratchpad session at runtime.
    #[serde(default)]
    pub max_tables_per_session: Option<usize>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ScratchpadInventoryArgs {
    /// Optional row cap for inventory listing.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadTableInventoryArgs {
    /// Logical scratchpad session identifier.
    pub session_id: String,
    /// Optional row cap for table inventory listing.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadDropTableArgs {
    /// Logical scratchpad session identifier.
    pub session_id: String,
    /// Scratchpad table name to drop.
    pub table_name: String,
    /// If true, return success when the table is absent.
    #[serde(default)]
    pub if_exists: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadIngestReportArgs {
    /// Logical scratchpad session identifier.
    pub session_id: String,
    /// Destination scratchpad table name.
    pub table_name: String,
    /// GA Data API report request fields.
    #[serde(flatten)]
    pub report: RunReportArgs,
    /// If true, append rows into an existing scratchpad table with identical schema.
    #[serde(default)]
    pub append: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadIngestRealtimeArgs {
    /// Logical scratchpad session identifier.
    pub session_id: String,
    /// Destination scratchpad table name.
    pub table_name: String,
    /// GA realtime report request fields.
    #[serde(flatten)]
    pub realtime: RunRealtimeReportArgs,
    /// If true, append rows into an existing scratchpad table with identical schema.
    #[serde(default)]
    pub append: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadQueryArgs {
    /// Logical scratchpad session identifier.
    pub session_id: String,
    /// Read-only SQL query against the scratchpad session.
    pub sql: String,
    /// Optional response row cap for Contract V1 tabular payloads.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Opaque pagination cursor from `meta.next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Tabular output shape for the response payload.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit row payload and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to response cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadTableHelperArgs {
    /// Logical scratchpad session identifier.
    pub session_id: String,
    /// Scratchpad table to target.
    pub table_name: String,
    /// Optional response row cap for Contract V1 tabular payloads.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Opaque pagination cursor from `meta.next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Tabular output shape for the response payload.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit row payload and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to response cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadReleaseRegressionArgs {
    /// Logical scratchpad session identifier.
    pub session_id: String,
    /// Scratchpad table to target.
    pub table_name: String,
    /// Release date in `YYYY-MM-DD`.
    pub release_date: String,
    /// Anchor event used as the instrumentation signal.
    pub anchor_event: String,
    /// Comparison event used as behavior baseline.
    pub comparison_event: String,
    /// Optional date column name (defaults to `date`).
    #[serde(default)]
    pub date_column: Option<String>,
    /// Optional event-name column (defaults to `event_name`).
    #[serde(default)]
    pub event_column: Option<String>,
    /// Optional numeric metric column (defaults to row-count weighting).
    #[serde(default)]
    pub metric_column: Option<String>,
    /// Number of baseline days before `release_date` (default: 7).
    #[serde(default)]
    pub pre_days: Option<u32>,
    /// Number of transition days from `release_date` (default: 1).
    #[serde(default)]
    pub transition_days: Option<u32>,
    /// Number of post-release days after transition window (default: 7).
    #[serde(default)]
    pub post_days: Option<u32>,
    /// Optional response row cap for Contract V1 tabular payloads.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Opaque pagination cursor from `meta.next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Tabular output shape for the response payload.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit row payload and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to response cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadLandingParamShiftArgs {
    /// Logical scratchpad session identifier.
    pub session_id: String,
    /// Scratchpad table to target.
    pub table_name: String,
    /// Release date in `YYYY-MM-DD`.
    pub release_date: String,
    /// Optional date column name (defaults to `date_parsed`).
    #[serde(default)]
    pub date_column: Option<String>,
    /// Optional landing URL column (defaults to `landingpageplusquerystring`).
    #[serde(default)]
    pub landing_url_column: Option<String>,
    /// Optional channel column; when unset, all rows are grouped under `all`.
    #[serde(default)]
    pub channel_column: Option<String>,
    /// Optional source/medium column; when unset, all rows are grouped under `all`.
    #[serde(default)]
    pub source_medium_column: Option<String>,
    /// Number of baseline days before `release_date` (default: 7).
    #[serde(default)]
    pub pre_days: Option<u32>,
    /// Number of transition days from `release_date` (default: 1).
    #[serde(default)]
    pub transition_days: Option<u32>,
    /// Number of post-release days after transition window (default: 7).
    #[serde(default)]
    pub post_days: Option<u32>,
    /// Maximum number of ranked parameter shifts to return (default: 100).
    #[serde(default)]
    pub top_n: Option<usize>,
    /// Optional response row cap for Contract V1 tabular payloads.
    #[serde(default)]
    pub max_rows: Option<usize>,
    /// Opaque pagination cursor from `meta.next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Tabular output shape for the response payload.
    #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
    pub output_mode: Option<TabularOutputMode>,
    /// If true, omit row payload and return metadata only.
    #[serde(default)]
    pub summary_only: bool,
    /// Optional string clipping limit applied to response cell values.
    #[serde(default)]
    pub max_cell_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadEvidenceBundleArgs {
    /// Logical scratchpad session identifier.
    pub session_id: String,
    /// Optional subset of tables to include; defaults to all session tables (bounded).
    #[serde(default)]
    pub table_names: Option<Vec<String>>,
    /// Optional number of sample rows to include per table (default: 20).
    #[serde(default)]
    pub sample_rows_per_table: Option<usize>,
}

#[derive(Debug)]
struct CursorToken {
    query_hash: String,
    offset: u64,
}

#[derive(Debug, Clone)]
struct TabularResponseOptions {
    query_hash: String,
    output_mode: contract::OutputMode,
    summary_only: bool,
    max_cell_chars: Option<usize>,
    cursor_offset: u64,
}

#[derive(Debug, Clone)]
struct FunnelResponseOptions {
    query_hash: String,
    output_mode: contract::OutputMode,
    summary_only: bool,
    max_cell_chars: Option<usize>,
    effective_limit: u64,
    requested_limit: Option<u64>,
}

#[derive(Debug, Clone)]
struct GaTabularProjection {
    rows: Vec<Map<String, Value>>,
    row_count_total: usize,
    columns: Vec<contract::ColumnMeta>,
    ga_meta: Value,
}

#[derive(Debug, Clone)]
struct ScratchpadQueryControls {
    max_rows: Option<usize>,
    cursor: Option<String>,
    output_mode: contract::OutputMode,
    summary_only: bool,
    max_cell_chars: Option<usize>,
}

#[derive(Debug, Clone)]
struct ScratchpadTabularProjection {
    rows: Vec<Map<String, Value>>,
    row_count_total: usize,
    columns: Vec<contract::ColumnMeta>,
    query_hints: Vec<String>,
    pagination_mode: &'static str,
}

#[tool_router(router = tool_router_analytics, vis = "pub")]
impl AnalyticsMcp {
    /// Return the recommended first-run path.
    #[tool(
        name = "ga4_get_started",
        description = "Return the recommended GA4 first-run auth flow, credential modes, and safe starter tools."
    )]
    async fn ga4_get_started(&self) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let quota_project = self.client.quota_project().unwrap_or("<PROJECT_ID>");
        Ok(contract_success(
            json!({
                "server": "ga4-mcp",
                "capability_profile": self.capability_profile().as_str(),
                "auth_source_candidate": self.client.auth_source().as_str(),
                "auth_source_note": "A candidate server credential source is not proof credentials exist; call ga4_auth_status or auth status --verify-token.",
                "scope": self.client.analytics_scope(),
                "upstream_token_source": self.client.upstream_token_source().as_str(),
                "upstream_token_header": self.client.upstream_token_header(),
                "recommended_cli": {
                    "login": format!("ga4-mcp auth login --quota-project {quota_project}"),
                    "login_headless": format!("ga4-mcp auth login --headless --quota-project {quota_project}"),
                    "login_with_client_id_file": format!("ga4-mcp auth login --quota-project {quota_project} --client-id-file /path/to/client_id.json"),
                    "login_headless_with_client_id_file": format!("ga4-mcp auth login --headless --quota-project {quota_project} --client-id-file /path/to/client_id.json"),
                    "status": "ga4-mcp auth status --verify-token",
                    "doctor": "ga4-mcp auth doctor --verify-token"
                },
                "first_steps": [
                    format!("Run ga4-mcp auth login --headless --quota-project {quota_project} --client-id-file /path/to/client_id.json for the easiest unblocked SSH/browser login."),
                    "Call ga4-mcp auth status --verify-token or ga4_auth_status with verify_token=true to prove Google Analytics access without returning a token.",
                    "If Google blocks the bundled gcloud OAuth app for Analytics scopes, use --client-id-file /path/to/client_id.json from a Desktop OAuth client; ga4-mcp will use direct browser OAuth and its own credential file.",
                    "For the lowest-friction local or loopback HTTP service, use GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header_or_config and GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=authorization.",
                    "For a hosted per-user service, keep GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header so each client supplies its own Google token.",
                    "By default ga4-mcp auth login writes a GA4-specific ADC file so sibling Google MCPs keep their own tokens and scopes.",
                    "If verification says local ADC requires a quota project, enable analyticsadmin.googleapis.com and analyticsdata.googleapis.com on that project, then rerun ga4-mcp auth login --quota-project YOUR_PROJECT.",
                    format!("If Google blocks the bundled gcloud OAuth app during login, create a Desktop OAuth client and rerun ga4-mcp auth login --headless --quota-project {quota_project} --client-id-file /path/to/client_id.json."),
                    "Call get_account_summaries to discover accessible GA4 accounts and properties."
                ],
                "credential_options": [
                    {
                        "name": "Application Default Credentials",
                        "best_for": "lowest-friction local browser login",
                        "command": format!("ga4-mcp auth login --quota-project {quota_project}"),
                        "headless_command": format!("ga4-mcp auth login --headless --quota-project {quota_project}"),
                        "client_id_file_command": format!("ga4-mcp auth login --quota-project {quota_project} --client-id-file /path/to/client_id.json"),
                        "client_id_file_headless_command": format!("ga4-mcp auth login --headless --quota-project {quota_project} --client-id-file /path/to/client_id.json"),
                        "quota_project_command": "ga4-mcp auth command --quota-project YOUR_PROJECT",
                        "quota_project_note": "Only needed when Google reports local ADC requires a quota project; the project must have the Analytics Admin and Data APIs enabled.",
                        "client_id_file_note": "Use a Desktop OAuth client JSON when Google blocks the bundled gcloud OAuth app. Without --shared-adc, ga4-mcp handles browser OAuth directly and writes its GA4-specific credential file.",
                        "env": [],
                        "shared_adc_escape_hatch": "add --shared-adc only when intentionally creating the conventional shared gcloud ADC file; set GOOGLE_ANALYTICS_MCP_SHARED_ADC=true when the runtime should use it"
                    },
                    {
                        "name": "per-request Google bearer token",
                        "best_for": "hosted or multi-user MCP services",
                        "env": [
                            "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header",
                            "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=authorization"
                        ]
                    },
                    {
                        "name": "local request-header-or-ADC fallback",
                        "best_for": "loopback user-level HTTP services",
                        "env": [
                            "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header_or_config",
                            "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=authorization"
                        ]
                    },
                    {
                        "name": "GOOGLE_APPLICATION_CREDENTIALS",
                        "best_for": "standard service-account or ADC file configuration",
                        "env": ["GOOGLE_APPLICATION_CREDENTIALS"]
                    }
                ],
                "safe_starter_tools": [
                    "get_account_summaries",
                    "get_property_details",
                    "run_report",
                    "preview_report_request",
                    "check_report_compatibility"
                ],
                "scratchpad_note": "Scratchpad tools require GOOGLE_ANALYTICS_MCP_CAPABILITY_PROFILE=scratchpad or --capability-profile scratchpad."
            }),
            started,
        ))
    }

    /// Explain configured auth without exposing secrets.
    #[tool(
        name = "ga4_auth_status",
        description = "Explain configured Google Analytics auth source and optionally verify Google API access without returning secrets."
    )]
    async fn ga4_auth_status(
        &self,
        Parameters(args): Parameters<AuthStatusArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let token_check = if args.verify_token {
            let result =
                if self.client.upstream_token_source() == UpstreamTokenSource::RequestHeader {
                    self.client.verify_config_token().await
                } else {
                    self.client.verify_token().await
                };
            match result {
                Ok(()) => json!({ "checked": true, "ok": true }),
                Err(err) => json!({
                    "checked": true,
                    "ok": false,
                    "error": redact_tool_error_message(&err)
                }),
            }
        } else {
            json!({ "checked": false })
        };
        let token_ok = token_check.get("ok").and_then(Value::as_bool);
        let auth_source_candidate = self.client.auth_source();
        let credential_material_detected = credential_material_detected_for_auth_source(
            auth_source_candidate,
            local_credential_material_detected(),
        );
        let auth_source = if matches!(
            auth_source_candidate,
            AuthSource::GoogleDefaultProviderChain
        ) && !credential_material_detected
            && token_ok != Some(true)
        {
            Value::Null
        } else {
            json!(auth_source_candidate.as_str())
        };

        Ok(contract_success(
            json!({
                "auth_source": auth_source,
                "auth_source_candidate": auth_source_candidate.as_str(),
                "scope": self.client.analytics_scope(),
                "capability_profile": self.capability_profile().as_str(),
                "upstream_token_source": self.client.upstream_token_source().as_str(),
                "upstream_token_header": self.client.upstream_token_header(),
                "quota_project_configured": self.client.quota_project_configured(),
                "quota_project": self.client.quota_project(),
                "credential_material_detected": credential_material_detected,
                "detected_env": auth_env_presence(),
                "token_check": token_check,
                "next_steps": auth_next_steps(self.client.upstream_token_source(), self.client.analytics_scope(), args.verify_token, token_ok),
                "secrets_returned": false
            }),
            started,
        ))
    }

    /// Return copyable GA4 auth login commands.
    #[tool(
        name = "ga4_auth_login_command",
        description = "Return copyable Google Analytics auth login commands, including direct browser OAuth when a client id file is supplied."
    )]
    async fn ga4_auth_login_command(
        &self,
        Parameters(args): Parameters<AuthLoginCommandArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let scope = login_scope_for_mcp_command(self.client.analytics_scope());
        let quota_project = args
            .quota_project
            .clone()
            .or_else(|| self.client.quota_project().map(str::to_string));
        let shared_adc = args.shared_adc.unwrap_or(false);
        let cloudsdk_config = if shared_adc {
            None
        } else {
            server_cloudsdk_config_dir()
        };
        if !shared_adc && cloudsdk_config.is_none() {
            return Ok(contract_error(
                AnalyticsError::invalid(
                    "shared_adc",
                    "failed to determine the server-specific gcloud config directory; set HOME/XDG_CONFIG_HOME on Unix or APPDATA on Windows, or pass shared_adc=true to intentionally use conventional shared ADC",
                ),
                started,
            ));
        }
        let credential_file = if shared_adc {
            conventional_adc_credentials_path()
        } else {
            server_adc_credentials_path()
        };
        let client_id_file = args.client_id_file.as_deref().map(Path::new);
        let gcloud_command = login_command_for_scope_with_cloudsdk(
            scope,
            args.headless,
            if shared_adc { client_id_file } else { None },
            cloudsdk_config.as_deref(),
        );
        let headless_command = login_command_for_scope_with_cloudsdk(
            scope,
            true,
            if shared_adc { client_id_file } else { None },
            cloudsdk_config.as_deref(),
        );
        let preferred_cli = auth_login_cli_command(
            scope,
            args.headless,
            client_id_file,
            quota_project.as_deref(),
            shared_adc,
            args.account.as_deref(),
            args.callback_port,
        );
        let client_id_file_command = auth_login_cli_command(
            scope,
            args.headless,
            Some(Path::new("/path/to/client_id.json")),
            quota_project.as_deref(),
            shared_adc,
            args.account.as_deref(),
            args.callback_port,
        );
        let client_id_file_headless_command = auth_login_cli_command(
            scope,
            true,
            Some(Path::new("/path/to/client_id.json")),
            quota_project.as_deref(),
            shared_adc,
            args.account.as_deref(),
            args.callback_port,
        );
        let command = if client_id_file.is_some() && !shared_adc {
            preferred_cli.clone()
        } else {
            gcloud_command.clone()
        };
        let setup_plan = google_provider_auth_config(scope).adc_setup_plan();
        let after_login = after_login_instruction(
            self.client.upstream_token_source(),
            self.client.analytics_scope(),
            scope,
        );
        let follow_up_commands = if client_id_file.is_some() && !shared_adc {
            Vec::new()
        } else {
            quota_project
                .as_deref()
                .map(|project| {
                    vec![quota_project_command_with_cloudsdk(
                        project,
                        cloudsdk_config.as_deref(),
                    )]
                })
                .unwrap_or_default()
        };
        Ok(contract_success(
            json!({
                "command": command,
                "gcloud_command": gcloud_command,
                "preferred_cli": preferred_cli,
                "headless_command": headless_command,
                "client_id_file_command": client_id_file_command,
                "client_id_file_headless_command": client_id_file_headless_command,
                "quota_project_command": quota_project_command_with_cloudsdk(
                    "YOUR_PROJECT",
                    cloudsdk_config.as_deref(),
                ),
                "api_enable_command": setup_plan.api_enable.as_ref().map(|command| command.shell.as_str()),
                "follow_up_commands": follow_up_commands,
                "adc_scopes": setup_plan.scopes.clone(),
                "cloudsdk_config": cloudsdk_config.as_ref().map(|path| path.display().to_string()),
                "credential_file": credential_file.as_ref().map(|path| path.display().to_string()),
                "shared_adc": shared_adc,
                "scope": scope,
                "headless": args.headless,
                "client_id_file": args.client_id_file,
                "account": args.account,
                "callback_port": args.callback_port,
                "quota_project": quota_project,
                "after_login": after_login,
                "next_steps": setup_plan.next_steps.clone(),
                "notes": [
                    "By default this command writes a GA4-specific ADC file for this OS user.",
                    "When client_id_file is supplied without shared_adc, ga4-mcp uses toolkit browser OAuth and stores the result as a GA4-specific ADC file instead of using gcloud's bundled OAuth app.",
                    "Set shared_adc=true only when you intentionally want the conventional shared gcloud ADC file; set GOOGLE_ANALYTICS_MCP_SHARED_ADC=true when the runtime should use it.",
                    "No token or client secret is returned by this tool."
                ],
                "client_id_file_hint": "If Google blocks the bundled gcloud OAuth app for Analytics scopes, pass client_id_file from a Desktop OAuth client; headless mode will ask for the redirected loopback callback URL.",
                "quota_project_hint": "If verification says local ADC requires a quota project, enable analyticsadmin.googleapis.com and analyticsdata.googleapis.com on that project, run the quota_project_command, then verify again.",
                "local_http_env": {
                    "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE": "request_header_or_config",
                    "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER": "authorization"
                },
                "hosted_per_user_env": {
                    "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE": "request_header",
                    "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER": "authorization"
                },
                "service_account_alternative": {
                    "standard_env": "GOOGLE_APPLICATION_CREDENTIALS=/path/to/service-account-or-adc.json"
                }
            }),
            started,
        ))
    }

    /// Retrieve account summaries and linked properties for the authenticated identity.
    #[tool(
        name = "get_account_summaries",
        description = "Retrieves information about the user's Google Analytics accounts and properties."
    )]
    async fn get_account_summaries(
        &self,
        Parameters(args): Parameters<GetAccountSummariesArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        match self
            .client
            .get_account_summaries(PaginationOptions {
                page_size: args.page_size,
                max_pages: args.max_pages,
            })
            .await
        {
            Ok(data) => Ok(contract_success(data, started)),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Get property metadata/details from the Admin API.
    #[tool(
        name = "get_property_details",
        description = "Returns details about a property."
    )]
    async fn get_property_details(
        &self,
        Parameters(args): Parameters<PropertyArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        match self.client.get_property_details(&args.property_id).await {
            Ok(data) => Ok(contract_success(sort_object(data), started)),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Get account-level data sharing settings from the Admin API.
    #[tool(
        name = "get_account_data_sharing_settings",
        description = "Returns account-level data sharing settings for a Google Analytics account."
    )]
    async fn get_account_data_sharing_settings(
        &self,
        Parameters(args): Parameters<AccountArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        match self
            .client
            .get_account_data_sharing_settings(&args.account_id)
            .await
        {
            Ok(data) => Ok(contract_success(sort_object(data), started)),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Get property-level data retention settings from the Admin API.
    #[tool(
        name = "get_property_data_retention_settings",
        description = "Returns property-level data retention settings for a Google Analytics property."
    )]
    async fn get_property_data_retention_settings(
        &self,
        Parameters(args): Parameters<PropertyArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        match self
            .client
            .get_property_data_retention_settings(&args.property_id)
            .await
        {
            Ok(data) => Ok(contract_success(sort_object(data), started)),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// List Google Ads links configured on a property.
    #[tool(
        name = "list_google_ads_links",
        description = "Returns a list of links to Google Ads accounts for a property."
    )]
    async fn list_google_ads_links(
        &self,
        Parameters(args): Parameters<PropertyListArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        match self
            .client
            .list_google_ads_links(
                &args.property_id,
                PaginationOptions {
                    page_size: args.page_size,
                    max_pages: args.max_pages,
                },
            )
            .await
        {
            Ok(data) => Ok(contract_success(data, started)),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// List reporting data annotations for a property.
    #[tool(
        name = "list_property_annotations",
        description = "Returns reporting data annotations for a property."
    )]
    async fn list_property_annotations(
        &self,
        Parameters(args): Parameters<PropertyListArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        match self
            .client
            .list_property_annotations(
                &args.property_id,
                PaginationOptions {
                    page_size: args.page_size,
                    max_pages: args.max_pages,
                },
            )
            .await
        {
            Ok(data) => Ok(contract_success(data, started)),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Run an Admin API access report for a property.
    #[tool(
        name = "run_property_access_report",
        description = "Runs a Google Analytics Admin API access report for a property."
    )]
    async fn run_property_access_report(
        &self,
        Parameters(args): Parameters<RunAccessReportPropertyArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if let Err(err) = validate_report_inputs(
            &args.dimensions,
            &args.metrics,
            &args.date_ranges,
            args.limit,
            args.offset,
        ) {
            return Ok(contract_error(err, started));
        }
        if let Err(err) =
            validate_tabular_controls(args.max_rows, args.max_cell_chars, args.cursor.as_deref())
        {
            return Ok(contract_error(err, started));
        }

        let query_hash = match run_property_access_query_hash(&args) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let (effective_offset, effective_limit) = match resolve_cursor_window(
            &query_hash,
            args.cursor.as_deref(),
            args.offset,
            args.max_rows,
            args.limit,
        ) {
            Ok(values) => {
                emit_pagination_window(
                    "run_property_access_report",
                    &query_hash,
                    args.cursor.is_some(),
                    values.0,
                    values.1,
                );
                values
            }
            Err(err) => {
                emit_cursor_error("run_property_access_report", &err);
                return Ok(contract_error(err, started));
            }
        };

        let request = RunAccessReportRequest {
            date_ranges: args.date_ranges,
            dimensions: args.dimensions,
            metrics: args.metrics,
            dimension_filter: args.dimension_filter,
            metric_filter: args.metric_filter,
            order_bys: args.order_bys,
            offset: Some(effective_offset),
            limit: Some(effective_limit),
            time_zone: args.time_zone,
        };

        let response_options = TabularResponseOptions {
            query_hash,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
            cursor_offset: effective_offset,
        };

        match self
            .client
            .run_property_access_report(&args.property_id, request)
            .await
        {
            Ok(data) => Ok(contract_success_ga_tabular(
                data,
                started,
                "run_property_access_report",
                response_options,
            )),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Run an Admin API access report for an account.
    #[tool(
        name = "run_account_access_report",
        description = "Runs a Google Analytics Admin API access report for an account."
    )]
    async fn run_account_access_report(
        &self,
        Parameters(args): Parameters<RunAccessReportAccountArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if let Err(err) = validate_report_inputs(
            &args.dimensions,
            &args.metrics,
            &args.date_ranges,
            args.limit,
            args.offset,
        ) {
            return Ok(contract_error(err, started));
        }
        if let Err(err) =
            validate_tabular_controls(args.max_rows, args.max_cell_chars, args.cursor.as_deref())
        {
            return Ok(contract_error(err, started));
        }

        let query_hash = match run_account_access_query_hash(&args) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let (effective_offset, effective_limit) = match resolve_cursor_window(
            &query_hash,
            args.cursor.as_deref(),
            args.offset,
            args.max_rows,
            args.limit,
        ) {
            Ok(values) => {
                emit_pagination_window(
                    "run_account_access_report",
                    &query_hash,
                    args.cursor.is_some(),
                    values.0,
                    values.1,
                );
                values
            }
            Err(err) => {
                emit_cursor_error("run_account_access_report", &err);
                return Ok(contract_error(err, started));
            }
        };

        let request = RunAccessReportRequest {
            date_ranges: args.date_ranges,
            dimensions: args.dimensions,
            metrics: args.metrics,
            dimension_filter: args.dimension_filter,
            metric_filter: args.metric_filter,
            order_bys: args.order_bys,
            offset: Some(effective_offset),
            limit: Some(effective_limit),
            time_zone: args.time_zone,
        };

        let response_options = TabularResponseOptions {
            query_hash,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
            cursor_offset: effective_offset,
        };

        match self
            .client
            .run_account_access_report(&args.account_id, request)
            .await
        {
            Ok(data) => Ok(contract_success_ga_tabular(
                data,
                started,
                "run_account_access_report",
                response_options,
            )),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Run a GA4 Data API report request.
    #[tool(
        name = "run_report",
        description = "Runs a Google Analytics report using the Data API."
    )]
    async fn run_report(
        &self,
        Parameters(args): Parameters<RunReportArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if let Err(err) = validate_report_inputs(
            &args.dimensions,
            &args.metrics,
            &args.date_ranges,
            args.limit,
            args.offset,
        ) {
            return Ok(contract_error(err, started));
        }
        if let Err(err) =
            validate_tabular_controls(args.max_rows, args.max_cell_chars, args.cursor.as_deref())
        {
            return Ok(contract_error(err, started));
        }

        let query_hash = match run_report_query_hash(&args) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let (effective_offset, effective_limit) = match resolve_cursor_window(
            &query_hash,
            args.cursor.as_deref(),
            args.offset,
            args.max_rows,
            args.limit,
        ) {
            Ok(values) => {
                emit_pagination_window(
                    "run_report",
                    &query_hash,
                    args.cursor.is_some(),
                    values.0,
                    values.1,
                );
                values
            }
            Err(err) => {
                emit_cursor_error("run_report", &err);
                return Ok(contract_error(err, started));
            }
        };

        let request = RunReportRequest {
            property_id: args.property_id,
            date_ranges: args.date_ranges,
            dimensions: args.dimensions,
            metrics: args.metrics,
            dimension_filter: args.dimension_filter,
            metric_filter: args.metric_filter,
            order_bys: args.order_bys,
            limit: Some(effective_limit),
            offset: Some(effective_offset),
            currency_code: args.currency_code,
            return_property_quota: args.return_property_quota,
        };

        let response_options = TabularResponseOptions {
            query_hash,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
            cursor_offset: effective_offset,
        };

        match self.client.run_report(request).await {
            Ok(data) => Ok(contract_success_ga_tabular(
                data,
                started,
                "run_report",
                response_options,
            )),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Run a GA4 conversion and attribution report using the Data API v1alpha surface.
    #[tool(
        name = "run_conversions_report",
        description = "Runs a Google Analytics conversion report using the Data API v1alpha surface. Use this for conversions, ad performance, ROAS, or attribution. Supported dimensions: campaignName, continent, country, defaultChannelGroup, deviceCategory, medium, platform, primaryChannelGroup, source, sourceMedium, sourcePlatform, subcontinent. Supported metrics: advertiserAdClicks, advertiserAdCost, advertiserAdCostPerAllConversionsByConversionDate, advertiserAdCostPerAllConversionsByInteractionDate, advertiserAdCostPerClick, advertiserAdImpressions, allConversionsByConversionDate, allConversionsByInteractionDate, returnOnAdSpendByConversionDate, returnOnAdSpendByInteractionDate, totalRevenueByConversionDate, totalRevenueByInteractionDate. This alpha feature may not be available to every property."
    )]
    async fn run_conversions_report(
        &self,
        Parameters(args): Parameters<RunConversionsReportArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if let Err(err) = validate_conversions_report_inputs(&args) {
            return Ok(contract_error(err, started));
        }
        if let Err(err) =
            validate_tabular_controls(args.max_rows, args.max_cell_chars, args.cursor.as_deref())
        {
            return Ok(contract_error(err, started));
        }

        let query_hash = match run_conversions_report_query_hash(&args) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let (effective_offset, effective_limit) = match resolve_cursor_window(
            &query_hash,
            args.cursor.as_deref(),
            args.offset,
            args.max_rows,
            args.limit,
        ) {
            Ok(values) => {
                emit_pagination_window(
                    "run_conversions_report",
                    &query_hash,
                    args.cursor.is_some(),
                    values.0,
                    values.1,
                );
                values
            }
            Err(err) => {
                emit_cursor_error("run_conversions_report", &err);
                return Ok(contract_error(err, started));
            }
        };

        let mut conversion_spec = json!({
            "conversion_actions": args.conversion_spec.conversion_actions,
        });
        if let Some(attribution_model) = args.conversion_spec.attribution_model {
            conversion_spec["attribution_model"] = json!(attribution_model.as_str());
        }
        let request = RunConversionsReportRequest {
            property_id: args.property_id,
            date_ranges: args.date_ranges,
            dimensions: args.dimensions,
            metrics: args.metrics,
            conversion_spec,
            dimension_filter: args.dimension_filter,
            metric_filter: args.metric_filter,
            order_bys: args.order_bys,
            limit: Some(effective_limit),
            offset: Some(effective_offset),
            currency_code: args.currency_code,
            return_property_quota: args.return_property_quota,
        };
        let response_options = TabularResponseOptions {
            query_hash,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
            cursor_offset: effective_offset,
        };

        match self.client.run_conversions_report(request).await {
            Ok(data) => {
                if let Err(err) =
                    validate_ga_tabular_response_shape(&data, "run_conversions_report")
                {
                    return Ok(contract_error(err, started));
                }
                Ok(contract_success_ga_tabular(
                    data,
                    started,
                    "run_conversions_report",
                    response_options,
                ))
            }
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Run a GA4 funnel report using the Data API v1alpha surface.
    #[tool(
        name = "run_funnel_report",
        description = "Runs a Google Analytics funnel report using the Data API v1alpha surface. Each funnel step must provide either an event shorthand or a complete FunnelFilterExpression. Supports open or closed funnels, standard or trended visualization, breakdowns, next-action analysis, up to four segments, dimension filters, and bounded per-subreport Contract V1 output."
    )]
    async fn run_funnel_report(
        &self,
        Parameters(args): Parameters<RunFunnelReportArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if let Err(err) = validate_funnel_report_inputs(&args) {
            return Ok(contract_error(err, started));
        }
        if let Err(err) = validate_tabular_controls(args.max_rows, args.max_cell_chars, None) {
            return Ok(contract_error(err, started));
        }

        let query_hash = match run_funnel_report_query_hash(&args) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let effective_limit = resolve_funnel_report_limit(args.max_rows, args.limit);
        emit_pagination_window("run_funnel_report", &query_hash, false, 0, effective_limit);
        let funnel_steps = match args
            .funnel_steps
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(steps) => steps,
            Err(err) => {
                return Ok(contract_error(
                    AnalyticsError::Internal(format!(
                        "failed to serialize validated funnel steps: {err}"
                    )),
                    started,
                ));
            }
        };
        let funnel_breakdown = args.funnel_breakdown.map(|breakdown| {
            let mut value = json!({
                "breakdown_dimension": { "name": breakdown.breakdown_dimension },
            });
            if let Some(limit) = breakdown.limit {
                value["limit"] = json!(limit.to_string());
            }
            value
        });
        let funnel_next_action = args.funnel_next_action.map(|next_action| {
            let mut value = json!({
                "next_action_dimension": { "name": next_action.next_action_dimension },
            });
            if let Some(limit) = next_action.limit {
                value["limit"] = json!(limit.to_string());
            }
            value
        });
        let request = RunFunnelReportRequest {
            property_id: args.property_id,
            funnel_steps,
            is_open_funnel: args.is_open_funnel,
            date_ranges: args.date_ranges,
            funnel_breakdown,
            funnel_next_action,
            funnel_visualization_type: args
                .funnel_visualization_type
                .map(FunnelVisualizationType::as_str)
                .map(str::to_string),
            segments: args.segments,
            dimension_filter: args.dimension_filter,
            limit: Some(effective_limit),
            return_property_quota: args.return_property_quota,
        };
        let response_options = FunnelResponseOptions {
            query_hash,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
            effective_limit,
            requested_limit: args.limit,
        };

        match self.client.run_funnel_report(request).await {
            Ok(data) => Ok(contract_success_ga_funnel(data, started, response_options)),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Validate and preview a run_report request without making an upstream API call.
    #[tool(
        name = "preview_report_request",
        description = "Validates and previews a normalized run_report request payload and projected columns without executing the upstream GA API call."
    )]
    async fn preview_report_request(
        &self,
        Parameters(args): Parameters<PreviewReportRequestArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let report = args.report;
        if let Err(err) = validate_report_inputs(
            &report.dimensions,
            &report.metrics,
            &report.date_ranges,
            report.limit,
            report.offset,
        ) {
            return Ok(contract_error(err, started));
        }
        if let Err(err) = validate_tabular_controls(
            report.max_rows,
            report.max_cell_chars,
            report.cursor.as_deref(),
        ) {
            return Ok(contract_error(err, started));
        }

        let query_hash = match run_report_query_hash(&report) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let (effective_offset, effective_limit) = match resolve_cursor_window(
            &query_hash,
            report.cursor.as_deref(),
            report.offset,
            report.max_rows,
            report.limit,
        ) {
            Ok(values) => values,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let payload = match build_run_report_preview_payload(
            &report,
            &query_hash,
            effective_offset,
            effective_limit,
        ) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        Ok(contract_success(payload, started))
    }

    /// Run a GA4 realtime report request.
    #[tool(
        name = "run_realtime_report",
        description = "Runs a Google Analytics realtime report using the Data API."
    )]
    async fn run_realtime_report(
        &self,
        Parameters(args): Parameters<RunRealtimeReportArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if let Err(err) =
            validate_realtime_inputs(&args.dimensions, &args.metrics, args.limit, args.offset)
        {
            return Ok(contract_error(err, started));
        }
        if let Err(err) =
            validate_tabular_controls(args.max_rows, args.max_cell_chars, args.cursor.as_deref())
        {
            return Ok(contract_error(err, started));
        }

        let query_hash = match run_realtime_query_hash(&args) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let (effective_offset, effective_limit) = match resolve_cursor_window(
            &query_hash,
            args.cursor.as_deref(),
            args.offset,
            args.max_rows,
            args.limit,
        ) {
            Ok(values) => {
                emit_pagination_window(
                    "run_realtime_report",
                    &query_hash,
                    args.cursor.is_some(),
                    values.0,
                    values.1,
                );
                values
            }
            Err(err) => {
                emit_cursor_error("run_realtime_report", &err);
                return Ok(contract_error(err, started));
            }
        };

        let request = RunRealtimeReportRequest {
            property_id: args.property_id,
            dimensions: args.dimensions,
            metrics: args.metrics,
            dimension_filter: args.dimension_filter,
            metric_filter: args.metric_filter,
            order_bys: args.order_bys,
            limit: Some(effective_limit),
            offset: Some(effective_offset),
            return_property_quota: args.return_property_quota,
        };

        let response_options = TabularResponseOptions {
            query_hash,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
            cursor_offset: effective_offset,
        };

        match self.client.run_realtime_report(request).await {
            Ok(data) => Ok(contract_success_ga_tabular(
                data,
                started,
                "run_realtime_report",
                response_options,
            )),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Run a GA4 pivot report request.
    #[tool(
        name = "run_pivot_report",
        description = "Runs a Google Analytics pivot report using the Data API."
    )]
    async fn run_pivot_report(
        &self,
        Parameters(args): Parameters<RunPivotReportArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if let Err(err) = validate_pivot_inputs(
            &args.dimensions,
            &args.metrics,
            &args.date_ranges,
            &args.pivots,
        ) {
            return Ok(contract_error(err, started));
        }
        if let Err(err) =
            validate_tabular_controls(args.max_rows, args.max_cell_chars, args.cursor.as_deref())
        {
            return Ok(contract_error(err, started));
        }

        let query_hash = match run_pivot_query_hash(&args) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let (effective_offset, effective_limit) = match resolve_cursor_window(
            &query_hash,
            args.cursor.as_deref(),
            None,
            args.max_rows,
            None,
        ) {
            Ok(values) => {
                emit_pagination_window(
                    "run_pivot_report",
                    &query_hash,
                    args.cursor.is_some(),
                    values.0,
                    values.1,
                );
                values
            }
            Err(err) => {
                emit_cursor_error("run_pivot_report", &err);
                return Ok(contract_error(err, started));
            }
        };

        let request = RunPivotReportRequest {
            property_id: args.property_id,
            date_ranges: args.date_ranges,
            dimensions: args.dimensions,
            metrics: args.metrics,
            pivots: args.pivots,
            dimension_filter: args.dimension_filter,
            metric_filter: args.metric_filter,
            order_bys: args.order_bys,
            currency_code: args.currency_code,
            keep_empty_rows: args.keep_empty_rows,
            return_property_quota: args.return_property_quota,
        };

        let response_options = TabularResponseOptions {
            query_hash,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
            cursor_offset: effective_offset,
        };

        match self.client.run_pivot_report(request).await {
            Ok(data) => {
                let projection = project_ga_tabular_response(&data, "run_pivot_report");
                let projection = apply_local_window_to_projection(
                    projection,
                    effective_offset,
                    effective_limit,
                    "run_pivot_report",
                );
                Ok(contract_success_ga_projection(
                    projection,
                    started,
                    response_options,
                ))
            }
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Run up to five GA4 Data API report requests in a single batch call.
    #[tool(
        name = "batch_run_reports",
        description = "Runs multiple Google Analytics reports in a single Data API batch request."
    )]
    async fn batch_run_reports(
        &self,
        Parameters(args): Parameters<BatchRunReportsArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if let Err(err) = validate_batch_run_reports_inputs(&args.requests) {
            return Ok(contract_error(err, started));
        }

        let property = match args.property_id.to_resource_name() {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let mut requests = Vec::with_capacity(args.requests.len());
        let mut response_options = Vec::with_capacity(args.requests.len());

        for (index, request_args) in args.requests.into_iter().enumerate() {
            if let Err(err) = validate_report_inputs(
                &request_args.dimensions,
                &request_args.metrics,
                &request_args.date_ranges,
                request_args.limit,
                request_args.offset,
            ) {
                return Ok(contract_error(err, started));
            }
            if let Err(err) = validate_tabular_controls(
                request_args.max_rows,
                request_args.max_cell_chars,
                request_args.cursor.as_deref(),
            ) {
                return Ok(contract_error(err, started));
            }

            let query_hash = match batch_run_report_query_hash(&property, index, &request_args) {
                Ok(value) => value,
                Err(err) => return Ok(contract_error(err, started)),
            };

            let (effective_offset, effective_limit) = match resolve_cursor_window(
                &query_hash,
                request_args.cursor.as_deref(),
                request_args.offset,
                request_args.max_rows,
                request_args.limit,
            ) {
                Ok(values) => {
                    emit_pagination_window(
                        "batch_run_reports",
                        &query_hash,
                        request_args.cursor.is_some(),
                        values.0,
                        values.1,
                    );
                    values
                }
                Err(err) => {
                    emit_cursor_error("batch_run_reports", &err);
                    return Ok(contract_error(err, started));
                }
            };

            requests.push(BatchRunReportItemRequest {
                date_ranges: request_args.date_ranges,
                dimensions: request_args.dimensions,
                metrics: request_args.metrics,
                dimension_filter: request_args.dimension_filter,
                metric_filter: request_args.metric_filter,
                order_bys: request_args.order_bys,
                limit: Some(effective_limit),
                offset: Some(effective_offset),
                currency_code: request_args.currency_code,
                return_property_quota: request_args.return_property_quota,
            });

            response_options.push(TabularResponseOptions {
                query_hash,
                output_mode: request_args
                    .output_mode
                    .unwrap_or(TabularOutputMode::Rows)
                    .into(),
                summary_only: request_args.summary_only,
                max_cell_chars: request_args.max_cell_chars,
                cursor_offset: effective_offset,
            });
        }

        let request_count = requests.len();
        let batch_request = BatchRunReportsRequest {
            property_id: args.property_id,
            requests,
        };

        match self.client.batch_run_reports(batch_request).await {
            Ok(data) => {
                let reports = match data.get("reports").and_then(Value::as_array) {
                    Some(reports) => reports,
                    None => {
                        return Ok(contract_error(
                            AnalyticsError::Internal(
                                "batch_run_reports response missing 'reports' array".to_string(),
                            ),
                            started,
                        ));
                    }
                };
                if reports.len() != response_options.len() {
                    return Ok(contract_error(
                        AnalyticsError::Internal(format!(
                            "batch_run_reports response count mismatch: expected {}, got {}",
                            response_options.len(),
                            reports.len()
                        )),
                        started,
                    ));
                }

                let report_payloads = reports
                    .iter()
                    .enumerate()
                    .map(|(index, report)| {
                        let projection = project_ga_tabular_response(report, "batch_run_reports");
                        let (payload, mut meta) = ga_projection_payload_and_meta(
                            projection,
                            response_options[index].clone(),
                        );
                        if let Value::Object(ref mut map) = meta {
                            map.insert("batch_index".to_string(), json!(index));
                        }
                        json!({
                            "index": index,
                            "data": payload,
                            "meta": meta,
                        })
                    })
                    .collect::<Vec<_>>();

                Ok(contract::success_with_meta(
                    json!({
                        "reports": report_payloads,
                    }),
                    json!({
                        "batch": {
                            "property": property,
                            "request_count": request_count,
                            "response_count": reports.len(),
                        }
                    }),
                    contract::elapsed_ms(started),
                ))
            }
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Retrieve property custom dimensions and custom metrics.
    #[tool(
        name = "get_custom_dimensions_and_metrics",
        description = "Retrieves the custom dimensions and custom metrics for a specific property."
    )]
    async fn get_custom_dimensions_and_metrics(
        &self,
        Parameters(args): Parameters<PropertyArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        match self
            .client
            .get_custom_dimensions_and_metrics(&args.property_id)
            .await
        {
            Ok(data) => Ok(contract_success(data, started)),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Validate report dimension/metric compatibility and return custom inventory.
    #[tool(
        name = "check_report_compatibility",
        description = "Preflights report dimensions/metrics via GA checkCompatibility and returns compatibility reason codes plus custom dimension/metric inventory."
    )]
    async fn check_report_compatibility(
        &self,
        Parameters(args): Parameters<CheckReportCompatibilityArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if let Err(err) = validate_metric_lists(&args.dimensions, &args.metrics) {
            return Ok(contract_error(err, started));
        }
        let property = match args.property_id.to_resource_name() {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let compatibility = match self
            .client
            .check_report_compatibility(&args.property_id, &args.dimensions, &args.metrics)
            .await
        {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let custom_inventory = match self
            .client
            .get_custom_dimensions_and_metrics(&args.property_id)
            .await
        {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let summary = summarize_report_compatibility(&compatibility);

        Ok(contract_success(
            json!({
                "property_id": property,
                "request": {
                    "dimensions": args.dimensions,
                    "metrics": args.metrics
                },
                "compatibility": summary,
                "custom_inventory": custom_inventory,
                "raw": compatibility
            }),
            started,
        ))
    }

    /// Return current runtime scratchpad limit settings.
    #[tool(
        name = "scratchpad_get_runtime_limits",
        description = "Returns active runtime scratchpad limits (including max_sessions and max_tables_per_session) without restarting the server."
    )]
    async fn scratchpad_get_runtime_limits(
        &self,
        Parameters(_args): Parameters<ScratchpadRuntimeLimitsArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let config = self.scratchpad_sessions.config();
        let active_sessions = match self.scratchpad_sessions.active_session_count() {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let memory_pressure = collect_runtime_memory_pressure(
            self.scratchpad_sessions.as_ref(),
            active_sessions,
            MAX_MEMORY_PRESSURE_SAMPLE_SESSIONS,
        );
        Ok(contract_success(
            json!({
                "runtime_limits": {
                    "max_sessions": self.scratchpad_sessions.max_sessions_limit(),
                    "max_tables_per_session": self.scratchpad_sessions.max_tables_per_session_limit(),
                    "max_rows_per_session": config.max_rows_per_session,
                    "max_memory_mb": config.max_memory_mb,
                    "query_timeout_ms": config.query_timeout.as_millis(),
                    "max_sql_bytes": config.max_sql_bytes,
                },
                "active_sessions": active_sessions,
                "memory_pressure": memory_pressure
            }),
            started,
        ))
    }

    /// Update runtime scratchpad session-capacity limit without restart.
    #[tool(
        name = "scratchpad_set_runtime_limits",
        description = "Updates runtime scratchpad limits (currently max_sessions and max_tables_per_session) without restarting the server."
    )]
    async fn scratchpad_set_runtime_limits(
        &self,
        Parameters(args): Parameters<ScratchpadSetRuntimeLimitsArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if args.max_sessions.is_none() && args.max_tables_per_session.is_none() {
            return Ok(contract_error(
                AnalyticsError::invalid(
                    "runtime_limits",
                    "provide at least one of max_sessions or max_tables_per_session",
                ),
                started,
            ));
        }
        let previous_sessions = self.scratchpad_sessions.max_sessions_limit();
        let previous_tables = self.scratchpad_sessions.max_tables_per_session_limit();

        if let Some(max_sessions) = args.max_sessions
            && let Err(err) = self
                .scratchpad_sessions
                .set_max_sessions_limit(max_sessions)
        {
            return Ok(contract_error(err, started));
        }
        if let Some(max_tables_per_session) = args.max_tables_per_session
            && let Err(err) = self
                .scratchpad_sessions
                .set_max_tables_per_session_limit(max_tables_per_session)
        {
            return Ok(contract_error(err, started));
        }

        let active_sessions = match self.scratchpad_sessions.active_session_count() {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let updated_sessions = self.scratchpad_sessions.max_sessions_limit();
        let updated_tables = self.scratchpad_sessions.max_tables_per_session_limit();
        let note = if active_sessions > updated_sessions {
            Some(
                "active sessions currently exceed the new limit; existing sessions remain active but new sessions are blocked until usage drops"
                    .to_string(),
            )
        } else {
            None
        };

        Ok(contract_success(
            json!({
                "runtime_limits": {
                    "max_sessions": {
                        "previous": previous_sessions,
                        "updated": updated_sessions
                    },
                    "max_tables_per_session": {
                        "previous": previous_tables,
                        "updated": updated_tables
                    }
                },
                "active_sessions": active_sessions,
                "note": note
            }),
            started,
        ))
    }

    /// Open or touch a scratchpad session and return lifecycle metadata.
    #[tool(
        name = "scratchpad_open_session",
        description = "Opens or refreshes a DuckDB scratchpad session and returns lifecycle metadata."
    )]
    async fn scratchpad_open_session(
        &self,
        Parameters(args): Parameters<ScratchpadSessionArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        match self.scratchpad_sessions.open_session(&args.session_id) {
            Ok(info) => {
                let config = self.scratchpad_sessions.config();
                let query_timeout_ms =
                    u64::try_from(config.query_timeout.as_millis()).unwrap_or(u64::MAX);
                Ok(contract_success(
                    json!({
                        "session_id": info.session_id,
                        "usage": {
                            "tables_used": info.tables_used,
                            "tables_remaining": info.tables_remaining,
                            "rows_used": info.rows_used,
                            "rows_remaining": info.rows_remaining
                        },
                        "ttl_seconds": {
                            "default": config.session_ttl.as_secs(),
                            "remaining": info.ttl_seconds_remaining
                        },
                        "limits": {
                            "max_sessions": self.scratchpad_sessions.max_sessions_limit(),
                            "max_tables_per_session": self.scratchpad_sessions.max_tables_per_session_limit(),
                            "max_rows_per_session": config.max_rows_per_session,
                            "max_memory_mb": config.max_memory_mb,
                            "max_sql_bytes": config.max_sql_bytes,
                            "query_timeout_ms": query_timeout_ms
                        }
                    }),
                    started,
                ))
            }
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Close and remove a scratchpad session.
    #[tool(
        name = "scratchpad_close_session",
        description = "Closes a DuckDB scratchpad session and removes its temporary state."
    )]
    async fn scratchpad_close_session(
        &self,
        Parameters(args): Parameters<ScratchpadSessionArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        match self.scratchpad_sessions.release_session(&args.session_id) {
            Ok(closed) => Ok(contract_success(
                json!({
                    "session_id": args.session_id.trim(),
                    "closed": closed
                }),
                started,
            )),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// List active scratchpad sessions with bounded metadata.
    #[tool(
        name = "scratchpad_list_sessions",
        description = "Lists active DuckDB scratchpad sessions with bounded lifecycle/usage metadata."
    )]
    async fn scratchpad_list_sessions(
        &self,
        Parameters(args): Parameters<ScratchpadInventoryArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let limit = match resolve_scratchpad_list_limit(args.limit) {
            Ok(limit) => limit,
            Err(err) => return Ok(contract_error(err, started)),
        };

        match self.scratchpad_sessions.list_sessions(limit) {
            Ok(sessions) => Ok(contract_success(
                json!({
                    "sessions": sessions.iter().map(|session| json!({
                        "session_id": session.session_id,
                        "tables_used": session.tables_used,
                        "tables_remaining": session.tables_remaining,
                        "rows_used": session.rows_used,
                        "rows_remaining": session.rows_remaining,
                        "ttl_seconds_remaining": session.ttl_seconds_remaining
                    })).collect::<Vec<_>>(),
                    "returned": sessions.len(),
                    "limit": limit
                }),
                started,
            )),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// List tables in an existing scratchpad session.
    #[tool(
        name = "scratchpad_list_tables",
        description = "Lists table inventory for an existing DuckDB scratchpad session."
    )]
    async fn scratchpad_list_tables(
        &self,
        Parameters(args): Parameters<ScratchpadTableInventoryArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let limit = match resolve_scratchpad_list_limit(args.limit) {
            Ok(limit) => limit,
            Err(err) => return Ok(contract_error(err, started)),
        };

        match self
            .scratchpad_sessions
            .list_tables(&args.session_id, limit)
        {
            Ok(tables) => Ok(contract_success(
                json!({
                    "session_id": args.session_id.trim(),
                    "tables": tables.iter().map(scratchpad_table_info_to_json).collect::<Vec<_>>(),
                    "returned": tables.len(),
                    "limit": limit
                }),
                started,
            )),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Drop a scratchpad table and reclaim table-slot/row quota for the session.
    #[tool(
        name = "scratchpad_drop_table",
        description = "Drops a scratchpad table and reclaims its table slot and row quota for the session."
    )]
    async fn scratchpad_drop_table(
        &self,
        Parameters(args): Parameters<ScratchpadDropTableArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let normalized_table_name = match normalize_table_identifier(&args.table_name) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        match self.scratchpad_sessions.drop_table(
            &args.session_id,
            &normalized_table_name,
            args.if_exists,
        ) {
            Ok(stats) => Ok(contract_success(
                json!({
                    "session_id": args.session_id.trim(),
                    "table_name": {
                        "requested": args.table_name,
                        "normalized": normalized_table_name
                    },
                    "dropped": stats.dropped,
                    "rows_removed": stats.rows_removed,
                    "usage": {
                        "tables_used": stats.session_snapshot.tables_used,
                        "tables_remaining": stats.session_snapshot.tables_remaining,
                        "rows_used": stats.session_snapshot.rows_used,
                        "rows_remaining": stats.session_snapshot.rows_remaining
                    }
                }),
                started,
            )),
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Materialize GA report output into a scratchpad DuckDB table.
    #[tool(
        name = "scratchpad_ingest_report",
        description = "Runs a GA report and ingests the returned rows into a normalized scratchpad DuckDB table."
    )]
    async fn scratchpad_ingest_report(
        &self,
        Parameters(args): Parameters<ScratchpadIngestReportArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if args.report.cursor.is_some() {
            return Ok(contract_error(
                AnalyticsError::invalid(
                    "cursor",
                    "cursor is not supported for scratchpad_ingest_report; use offset/limit",
                ),
                started,
            ));
        }
        if args.report.summary_only {
            return Ok(contract_error(
                AnalyticsError::invalid(
                    "summary_only",
                    "summary_only=true is not supported for scratchpad_ingest_report",
                ),
                started,
            ));
        }
        if let Err(err) = validate_report_inputs(
            &args.report.dimensions,
            &args.report.metrics,
            &args.report.date_ranges,
            args.report.limit,
            args.report.offset,
        ) {
            return Ok(contract_error(err, started));
        }
        if let Err(err) = validate_tabular_controls(args.report.max_rows, None, None) {
            return Ok(contract_error(err, started));
        }

        let table_name = match normalize_table_identifier(&args.table_name) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        match collect_run_report_projection_for_ingest(self, &args.report).await {
            Ok((projection, pagination)) => {
                match ingest_projection_into_scratchpad(
                    self,
                    &args.session_id,
                    &args.table_name,
                    &table_name,
                    projection,
                    "scratchpad_ingest_report",
                    "run_report",
                    if args.append {
                        ScratchpadIngestMode::Append
                    } else {
                        ScratchpadIngestMode::Create
                    },
                    Some(pagination),
                ) {
                    Ok(payload) => Ok(contract_success(payload, started)),
                    Err(err) => Ok(contract_error(err, started)),
                }
            }
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Materialize GA realtime report output into a scratchpad DuckDB table.
    #[tool(
        name = "scratchpad_ingest_realtime_report",
        description = "Runs a GA realtime report and ingests the returned rows into a normalized scratchpad DuckDB table."
    )]
    async fn scratchpad_ingest_realtime_report(
        &self,
        Parameters(args): Parameters<ScratchpadIngestRealtimeArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        if args.realtime.cursor.is_some() {
            return Ok(contract_error(
                AnalyticsError::invalid(
                    "cursor",
                    "cursor is not supported for scratchpad_ingest_realtime_report; use offset/limit",
                ),
                started,
            ));
        }
        if args.realtime.summary_only {
            return Ok(contract_error(
                AnalyticsError::invalid(
                    "summary_only",
                    "summary_only=true is not supported for scratchpad_ingest_realtime_report",
                ),
                started,
            ));
        }
        if let Err(err) = validate_realtime_inputs(
            &args.realtime.dimensions,
            &args.realtime.metrics,
            args.realtime.limit,
            args.realtime.offset,
        ) {
            return Ok(contract_error(err, started));
        }
        if let Err(err) = validate_tabular_controls(args.realtime.max_rows, None, None) {
            return Ok(contract_error(err, started));
        }

        let table_name = match normalize_table_identifier(&args.table_name) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        match collect_run_realtime_projection_for_ingest(self, &args.realtime).await {
            Ok((projection, pagination)) => {
                match ingest_projection_into_scratchpad(
                    self,
                    &args.session_id,
                    &args.table_name,
                    &table_name,
                    projection,
                    "scratchpad_ingest_realtime_report",
                    "run_realtime_report",
                    if args.append {
                        ScratchpadIngestMode::Append
                    } else {
                        ScratchpadIngestMode::Create
                    },
                    Some(pagination),
                ) {
                    Ok(payload) => Ok(contract_success(payload, started)),
                    Err(err) => Ok(contract_error(err, started)),
                }
            }
            Err(err) => Ok(contract_error(err, started)),
        }
    }

    /// Execute a read-only SQL query against a scratchpad session.
    #[tool(
        name = "scratchpad_query",
        description = "Executes a read-only SQL query against a DuckDB scratchpad session with Contract V1 tabular metadata controls."
    )]
    async fn scratchpad_query(
        &self,
        Parameters(args): Parameters<ScratchpadQueryArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let controls = ScratchpadQueryControls {
            max_rows: args.max_rows,
            cursor: args.cursor,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
        };

        Ok(run_scratchpad_query_contract(
            self,
            started,
            "scratchpad_query",
            &args.session_id,
            &args.sql,
            controls,
            None,
        ))
    }

    /// Describe a scratchpad table schema using a helper query.
    #[tool(
        name = "scratchpad_describe_table",
        description = "Describes schema metadata for a scratchpad table using DuckDB DESCRIBE."
    )]
    async fn scratchpad_describe_table(
        &self,
        Parameters(args): Parameters<ScratchpadTableHelperArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let normalized_table_name = match normalize_table_identifier(&args.table_name) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let controls = ScratchpadQueryControls {
            max_rows: args.max_rows,
            cursor: args.cursor,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
        };
        let quoted_table_name = quote_sql_identifier(&normalized_table_name);
        let sql = format!("DESCRIBE SELECT * FROM {quoted_table_name}");

        Ok(run_scratchpad_query_contract(
            self,
            started,
            "scratchpad_describe_table",
            &args.session_id,
            &sql,
            controls,
            Some(json!({
                "helper": "scratchpad_describe_table",
                "table_name": {
                    "requested": args.table_name,
                    "normalized": normalized_table_name,
                }
            })),
        ))
    }

    /// Summarize column-level statistics for a scratchpad table using DuckDB SUMMARIZE.
    #[tool(
        name = "scratchpad_summarize_table",
        description = "Runs DuckDB SUMMARIZE over a scratchpad table and returns column profile statistics."
    )]
    async fn scratchpad_summarize_table(
        &self,
        Parameters(args): Parameters<ScratchpadTableHelperArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let normalized_table_name = match normalize_table_identifier(&args.table_name) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let controls = ScratchpadQueryControls {
            max_rows: args.max_rows,
            cursor: args.cursor,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
        };
        let quoted_table_name = quote_sql_identifier(&normalized_table_name);
        let sql = format!("SUMMARIZE SELECT * FROM {quoted_table_name}");

        Ok(run_scratchpad_query_contract(
            self,
            started,
            "scratchpad_summarize_table",
            &args.session_id,
            &sql,
            controls,
            Some(json!({
                "helper": "scratchpad_summarize_table",
                "table_name": {
                    "requested": args.table_name,
                    "normalized": normalized_table_name,
                }
            })),
        ))
    }

    /// Compute pre/transition/post release diagnostics with instrumentation confidence flags.
    #[tool(
        name = "scratchpad_release_regression_report",
        description = "Builds release-window diagnostics (pre/transition/post) with confidence flags for likely instrumentation regressions."
    )]
    async fn scratchpad_release_regression_report(
        &self,
        Parameters(args): Parameters<ScratchpadReleaseRegressionArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let normalized_table_name = match normalize_table_identifier(&args.table_name) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let release_date = match parse_iso_date_literal(&args.release_date) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let anchor_event = args.anchor_event.trim();
        if anchor_event.is_empty() {
            return Ok(contract_error(
                AnalyticsError::invalid("anchor_event", "must not be empty"),
                started,
            ));
        }
        let comparison_event = args.comparison_event.trim();
        if comparison_event.is_empty() {
            return Ok(contract_error(
                AnalyticsError::invalid("comparison_event", "must not be empty"),
                started,
            ));
        }
        if anchor_event == comparison_event {
            return Ok(contract_error(
                AnalyticsError::invalid("comparison_event", "must be different from anchor_event"),
                started,
            ));
        }

        let pre_days = match resolve_release_window_days(
            args.pre_days,
            DEFAULT_RELEASE_PRE_DAYS,
            "pre_days",
        ) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let transition_days = match resolve_release_window_days(
            args.transition_days,
            DEFAULT_RELEASE_TRANSITION_DAYS,
            "transition_days",
        ) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let post_days = match resolve_release_window_days(
            args.post_days,
            DEFAULT_RELEASE_POST_DAYS,
            "post_days",
        ) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let date_column =
            normalize_sql_identifier(args.date_column.as_deref().unwrap_or("date"), "date");
        let event_column = normalize_sql_identifier(
            args.event_column.as_deref().unwrap_or("event_name"),
            "event",
        );
        let metric_column = args
            .metric_column
            .as_deref()
            .map(|value| normalize_sql_identifier(value, "metric"));

        let controls = ScratchpadQueryControls {
            max_rows: args.max_rows,
            cursor: args.cursor,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
        };

        let sql = build_release_regression_sql(
            &normalized_table_name,
            &release_date,
            anchor_event,
            comparison_event,
            &date_column,
            &event_column,
            metric_column.as_deref(),
            pre_days,
            transition_days,
            post_days,
        );

        Ok(run_scratchpad_query_contract(
            self,
            started,
            "scratchpad_release_regression_report",
            &args.session_id,
            &sql,
            controls,
            Some(json!({
                "helper": "scratchpad_release_regression_report",
                "table_name": {
                    "requested": args.table_name,
                    "normalized": normalized_table_name,
                },
                "release": {
                    "release_date": release_date,
                    "anchor_event": anchor_event,
                    "comparison_event": comparison_event,
                    "date_column": date_column,
                    "event_column": event_column,
                    "metric_column": metric_column,
                    "pre_days": pre_days,
                    "transition_days": transition_days,
                    "post_days": post_days,
                }
            })),
        ))
    }

    /// Analyze landing-page query-parameter shifts between pre and post windows.
    #[tool(
        name = "scratchpad_landing_param_shift_report",
        description = "Builds landing URL parameter shift diagnostics (pre vs post windows) with channel/source splits."
    )]
    async fn scratchpad_landing_param_shift_report(
        &self,
        Parameters(args): Parameters<ScratchpadLandingParamShiftArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let normalized_table_name = match normalize_table_identifier(&args.table_name) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let release_date = match parse_iso_date_literal(&args.release_date) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let pre_days = match resolve_release_window_days(
            args.pre_days,
            DEFAULT_RELEASE_PRE_DAYS,
            "pre_days",
        ) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let transition_days = match resolve_release_window_days(
            args.transition_days,
            DEFAULT_RELEASE_TRANSITION_DAYS,
            "transition_days",
        ) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let post_days = match resolve_release_window_days(
            args.post_days,
            DEFAULT_RELEASE_POST_DAYS,
            "post_days",
        ) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };
        let top_n = match resolve_landing_shift_top_n(args.top_n) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let date_column =
            normalize_sql_identifier(args.date_column.as_deref().unwrap_or("date_parsed"), "date");
        let landing_url_column = normalize_sql_identifier(
            args.landing_url_column
                .as_deref()
                .unwrap_or("landingpageplusquerystring"),
            "landing_url",
        );
        let channel_column = args
            .channel_column
            .as_deref()
            .map(|value| normalize_sql_identifier(value, "channel"));
        let source_medium_column = args
            .source_medium_column
            .as_deref()
            .map(|value| normalize_sql_identifier(value, "source_medium"));

        let controls = ScratchpadQueryControls {
            max_rows: args.max_rows,
            cursor: args.cursor,
            output_mode: args.output_mode.unwrap_or(TabularOutputMode::Rows).into(),
            summary_only: args.summary_only,
            max_cell_chars: args.max_cell_chars,
        };

        let sql = build_landing_param_shift_sql(
            &normalized_table_name,
            &release_date,
            &date_column,
            &landing_url_column,
            channel_column.as_deref(),
            source_medium_column.as_deref(),
            pre_days,
            transition_days,
            post_days,
            top_n,
        );

        Ok(run_scratchpad_query_contract(
            self,
            started,
            "scratchpad_landing_param_shift_report",
            &args.session_id,
            &sql,
            controls,
            Some(json!({
                "helper": "scratchpad_landing_param_shift_report",
                "table_name": {
                    "requested": args.table_name,
                    "normalized": normalized_table_name,
                },
                "release": {
                    "release_date": release_date,
                    "date_column": date_column,
                    "landing_url_column": landing_url_column,
                    "channel_column": channel_column,
                    "source_medium_column": source_medium_column,
                    "pre_days": pre_days,
                    "transition_days": transition_days,
                    "post_days": post_days,
                    "top_n": top_n,
                }
            })),
        ))
    }

    /// Export a compact evidence bundle for shareable traceability.
    #[tool(
        name = "scratchpad_export_evidence_bundle",
        description = "Exports a bounded JSON+Markdown evidence bundle from a scratchpad session (table inventory, sample query hashes, row counts, and sample rows)."
    )]
    async fn scratchpad_export_evidence_bundle(
        &self,
        Parameters(args): Parameters<ScratchpadEvidenceBundleArgs>,
    ) -> Result<CallToolResult, crate::McpError> {
        let started = Instant::now();
        let sample_rows_per_table = match resolve_evidence_sample_rows(args.sample_rows_per_table) {
            Ok(value) => value,
            Err(err) => return Ok(contract_error(err, started)),
        };

        let selected_tables = if let Some(requested_tables) = args.table_names.clone() {
            if requested_tables.is_empty() {
                return Ok(contract_error(
                    AnalyticsError::invalid("table_names", "must not be empty when provided"),
                    started,
                ));
            }
            if requested_tables.len() > MAX_EVIDENCE_TABLES {
                return Ok(contract_error(
                    AnalyticsError::invalid(
                        "table_names",
                        format!("must include <= {MAX_EVIDENCE_TABLES} entries"),
                    ),
                    started,
                ));
            }
            let mut dedupe = HashSet::new();
            let mut tables = Vec::new();
            for requested in requested_tables {
                let normalized = match normalize_table_identifier(&requested) {
                    Ok(value) => value,
                    Err(err) => return Ok(contract_error(err, started)),
                };
                if dedupe.insert(normalized.clone()) {
                    tables.push((requested, normalized));
                }
            }
            tables
        } else {
            match self
                .scratchpad_sessions
                .list_tables(&args.session_id, MAX_EVIDENCE_TABLES)
            {
                Ok(inventory) => inventory
                    .into_iter()
                    .map(|table| (table.name.clone(), table.name))
                    .collect::<Vec<_>>(),
                Err(err) => return Ok(contract_error(err, started)),
            }
        };

        if selected_tables.is_empty() {
            return Ok(contract_error(
                AnalyticsError::invalid(
                    "table_names",
                    "no scratchpad tables available; ingest or provide explicit table_names",
                ),
                started,
            ));
        }

        let mut table_bundles = Vec::new();
        for (requested_name, normalized_name) in &selected_tables {
            let sql = format!("SELECT * FROM {}", quote_sql_identifier(normalized_name));
            let projection = match execute_scratchpad_projection(
                self,
                &args.session_id,
                &sql,
                0,
                sample_rows_per_table as u64,
            ) {
                Ok(value) => value,
                Err(err) => return Ok(contract_error(err, started)),
            };
            let query_hash = match scratchpad_query_hash(
                "scratchpad_export_evidence_bundle",
                &args.session_id,
                &sql,
            ) {
                Ok(value) => value,
                Err(err) => return Ok(contract_error(err, started)),
            };

            table_bundles.push(json!({
                "table_name": {
                    "requested": requested_name,
                    "normalized": normalized_name
                },
                "sample_query": {
                    "sql": sql,
                    "query_hash": query_hash,
                },
                "row_count_total": projection.row_count_total,
                "row_count_sampled": projection.rows.len(),
                "sample_truncated": projection.row_count_total > projection.rows.len(),
                "columns": projection.columns,
                "sample_rows": projection.rows
            }));
        }

        let generated_at_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let mut markdown = format!(
            "# Scratchpad Evidence Bundle\n\n- Session: `{}`\n- Generated (epoch_ms): `{}`\n- Tables: `{}`\n- Sample rows per table: `{}`\n\n",
            args.session_id.trim(),
            generated_at_epoch_ms,
            table_bundles.len(),
            sample_rows_per_table
        );
        markdown.push_str("## Table Summary\n\n");
        for table in &table_bundles {
            let normalized = table
                .get("table_name")
                .and_then(|value| value.get("normalized"))
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let row_count_total = table
                .get("row_count_total")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let row_count_sampled = table
                .get("row_count_sampled")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let query_hash = table
                .get("sample_query")
                .and_then(|value| value.get("query_hash"))
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            markdown.push_str(&format!(
                "- `{normalized}`: total_rows={row_count_total}, sampled_rows={row_count_sampled}, query_hash={query_hash}\n"
            ));
        }

        Ok(contract_success(
            json!({
                "session_id": args.session_id.trim(),
                "generated_at_epoch_ms": generated_at_epoch_ms,
                "sample_rows_per_table": sample_rows_per_table,
                "tables": table_bundles,
                "markdown": markdown
            }),
            started,
        ))
    }
}

fn deserialize_optional_dimension_filter<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    normalize_optional_filter(value, "dimension_filter", true).map_err(serde::de::Error::custom)
}

fn deserialize_optional_metric_filter<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    normalize_optional_filter(value, "metric_filter", false).map_err(serde::de::Error::custom)
}

fn deserialize_optional_output_mode<'de, D>(
    deserializer: D,
) -> Result<Option<TabularOutputMode>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(raw) => TabularOutputMode::parse(&raw).map(Some).ok_or_else(|| {
            serde::de::Error::custom(TabularOutputMode::invalid_value_message(&raw))
        }),
    }
}

fn normalize_optional_filter(
    value: Option<Value>,
    field: &'static str,
    allow_expression_shorthand: bool,
) -> Result<Option<Value>, String> {
    let Some(value) = value else {
        return Ok(None);
    };

    match value {
        Value::Null => Ok(None),
        Value::Object(_) => Ok(Some(value)),
        Value::String(raw) => {
            parse_filter_string_value(&raw, field, allow_expression_shorthand).map(Some)
        }
        _ => Err(format!(
            "{field} must be an object, JSON-object string{}",
            if allow_expression_shorthand {
                ", or `field==value`"
            } else {
                ""
            }
        )),
    }
}

fn parse_filter_string_value(
    raw: &str,
    field: &'static str,
    allow_expression_shorthand: bool,
) -> Result<Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{field} must not be an empty string"));
    }
    if trimmed.starts_with('{') {
        return parse_filter_json_string(trimmed, field);
    }
    if allow_expression_shorthand {
        return parse_simple_dimension_filter_expression(trimmed, field);
    }
    Err(format!(
        "{field} string form must be a JSON object like `{{\"filter\": ...}}`"
    ))
}

fn parse_filter_json_string(raw: &str, field: &'static str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|err| format!("{field} JSON parse failed: {}", err.to_string().trim()))?;
    if !parsed.is_object() {
        return Err(format!("{field} JSON form must decode to an object"));
    }
    Ok(parsed)
}

fn parse_simple_dimension_filter_expression(
    raw: &str,
    field: &'static str,
) -> Result<Value, String> {
    let (raw_name, raw_value, operator) = if let Some((name, value)) = raw.split_once("==") {
        (name, value, "==")
    } else if let Some((name, value)) = raw.split_once('=') {
        (name, value, "=")
    } else {
        return Err(format!(
            "{field} expression must use `field==value` (or `field=value`)"
        ));
    };

    let field_name = raw_name.trim();
    if field_name.is_empty() {
        return Err(format!(
            "{field} expression has an empty field name before `{operator}`"
        ));
    }

    let raw_value = raw_value.trim();
    if raw_value.contains(char::is_whitespace) && !is_wrapped_in_matching_quotes(raw_value) {
        return Err(format!(
            "{field} values containing spaces should be quoted (for example `sessionDefaultChannelGroup==\"Paid Other\"`) or expressed as a JSON FilterExpression object"
        ));
    }
    let value = strip_optional_quotes(raw_value);
    if value.is_empty() {
        return Err(format!(
            "{field} expression has an empty value after `{operator}`"
        ));
    }

    Ok(json!({
        "filter": {
            "field_name": field_name,
            "string_filter": {
                "match_type": "EXACT",
                "value": value,
            }
        }
    }))
}

fn strip_optional_quotes(raw: &str) -> String {
    if is_wrapped_in_matching_quotes(raw) {
        return raw[1..raw.len() - 1].to_string();
    }
    raw.to_string()
}

fn is_wrapped_in_matching_quotes(raw: &str) -> bool {
    if raw.len() >= 2 {
        let bytes = raw.as_bytes();
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        return (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'');
    }
    false
}

fn validate_realtime_inputs(
    dimensions: &[String],
    metrics: &[String],
    limit: Option<u64>,
    offset: Option<u64>,
) -> Result<(), AnalyticsError> {
    validate_metric_lists(dimensions, metrics)?;
    validate_limit_offset(limit, offset)
}

fn validate_report_inputs(
    dimensions: &[String],
    metrics: &[String],
    date_ranges: &[Value],
    limit: Option<u64>,
    offset: Option<u64>,
) -> Result<(), AnalyticsError> {
    validate_metric_lists(dimensions, metrics)?;
    if date_ranges.is_empty() {
        return Err(AnalyticsError::invalid(
            "date_ranges",
            "must include at least one date range",
        ));
    }
    validate_date_ranges_shape(date_ranges)?;
    validate_limit_offset(limit, offset)
}

fn validate_conversions_report_inputs(
    args: &RunConversionsReportArgs,
) -> Result<(), AnalyticsError> {
    validate_report_inputs(
        &args.dimensions,
        &args.metrics,
        &args.date_ranges,
        args.limit,
        args.offset,
    )?;

    for dimension in &args.dimensions {
        if !CONVERSION_DIMENSIONS.contains(&dimension.as_str()) {
            return Err(AnalyticsError::invalid(
                "dimensions",
                format!(
                    "unsupported conversion-report dimension {:?}; allowed values: {}",
                    dimension,
                    CONVERSION_DIMENSIONS.join(", ")
                ),
            ));
        }
    }
    for metric in &args.metrics {
        if !CONVERSION_METRICS.contains(&metric.as_str()) {
            return Err(AnalyticsError::invalid(
                "metrics",
                format!(
                    "unsupported conversion-report metric {:?}; allowed values: {}",
                    metric,
                    CONVERSION_METRICS.join(", ")
                ),
            ));
        }
    }
    for (idx, action) in args.conversion_spec.conversion_actions.iter().enumerate() {
        let valid = action
            .strip_prefix("conversionActions/")
            .is_some_and(|id| !id.is_empty() && id.chars().all(|ch| ch.is_ascii_digit()));
        if !valid {
            return Err(AnalyticsError::invalid(
                "conversion_spec",
                format!("conversion_actions[{idx}] must use conversionActions/<numeric-id>"),
            ));
        }
    }
    Ok(())
}

fn validate_funnel_report_inputs(args: &RunFunnelReportArgs) -> Result<(), AnalyticsError> {
    if args.funnel_steps.is_empty() {
        return Err(AnalyticsError::invalid(
            "funnel_steps",
            "must include at least one funnel step",
        ));
    }
    for (idx, step) in args.funnel_steps.iter().enumerate() {
        if step
            .name
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(AnalyticsError::invalid(
                "funnel_steps",
                format!("funnel_steps[{idx}].name must not be empty"),
            ));
        }
        match (&step.event, &step.filter_expression) {
            (Some(event), None) if !event.trim().is_empty() => {
                if event != event.trim() {
                    return Err(AnalyticsError::invalid(
                        "funnel_steps",
                        format!(
                            "funnel_steps[{idx}].event must not have leading or trailing whitespace"
                        ),
                    ));
                }
            }
            (None, Some(filter)) if filter.as_object().is_some_and(|object| !object.is_empty()) => {
            }
            (Some(_), Some(_)) => {
                return Err(AnalyticsError::invalid(
                    "funnel_steps",
                    format!(
                        "funnel_steps[{idx}] must set exactly one of event or filter_expression"
                    ),
                ));
            }
            (Some(_), None) => {
                return Err(AnalyticsError::invalid(
                    "funnel_steps",
                    format!("funnel_steps[{idx}].event must not be empty"),
                ));
            }
            (None, Some(_)) => {
                return Err(AnalyticsError::invalid(
                    "funnel_steps",
                    format!("funnel_steps[{idx}].filter_expression must be an object"),
                ));
            }
            (None, None) => {
                return Err(AnalyticsError::invalid(
                    "funnel_steps",
                    format!("funnel_steps[{idx}] must set either event or filter_expression"),
                ));
            }
        }
        if idx == 0 && step.is_directly_followed_by == Some(true) {
            return Err(AnalyticsError::invalid(
                "funnel_steps",
                "funnel_steps[0].is_directly_followed_by must be false because there is no prior step",
            ));
        }
        if let Some(duration) = step.within_duration_from_prior_step.as_deref() {
            if idx == 0 {
                return Err(AnalyticsError::invalid(
                    "funnel_steps",
                    "funnel_steps[0].within_duration_from_prior_step is invalid because there is no prior step",
                ));
            }
            if duration.trim().is_empty() {
                return Err(AnalyticsError::invalid(
                    "funnel_steps",
                    format!(
                        "funnel_steps[{idx}].within_duration_from_prior_step must not be empty"
                    ),
                ));
            }
            if !is_valid_funnel_duration(duration) {
                return Err(AnalyticsError::invalid(
                    "funnel_steps",
                    format!(
                        "funnel_steps[{idx}].within_duration_from_prior_step must be a non-negative protobuf duration such as 3600s or 3.5s"
                    ),
                ));
            }
        }
    }

    if !args.date_ranges.is_empty() {
        validate_date_ranges_shape(&args.date_ranges)?;
    }
    if let Some(segments) = &args.segments {
        if segments.len() > MAX_FUNNEL_SEGMENTS {
            return Err(AnalyticsError::invalid(
                "segments",
                format!("must include at most {MAX_FUNNEL_SEGMENTS} segments"),
            ));
        }
        for (idx, segment) in segments.iter().enumerate() {
            if !segment.as_object().is_some_and(|object| !object.is_empty()) {
                return Err(AnalyticsError::invalid(
                    "segments",
                    format!("segments[{idx}] must be a non-empty object"),
                ));
            }
        }
    }
    if let Some(breakdown) = &args.funnel_breakdown {
        if breakdown.breakdown_dimension.trim().is_empty() {
            return Err(AnalyticsError::invalid(
                "funnel_breakdown",
                "breakdown_dimension must not be empty",
            ));
        }
        if breakdown.breakdown_dimension != breakdown.breakdown_dimension.trim() {
            return Err(AnalyticsError::invalid(
                "funnel_breakdown",
                "breakdown_dimension must not have leading or trailing whitespace",
            ));
        }
        validate_optional_sublimit(
            "funnel_breakdown.limit",
            breakdown.limit,
            MAX_FUNNEL_BREAKDOWN_LIMIT,
        )?;
    }
    if let Some(next_action) = &args.funnel_next_action {
        if next_action.next_action_dimension.trim().is_empty() {
            return Err(AnalyticsError::invalid(
                "funnel_next_action",
                "next_action_dimension must not be empty",
            ));
        }
        if next_action.next_action_dimension != next_action.next_action_dimension.trim() {
            return Err(AnalyticsError::invalid(
                "funnel_next_action",
                "next_action_dimension must not have leading or trailing whitespace",
            ));
        }
        validate_optional_sublimit(
            "funnel_next_action.limit",
            next_action.limit,
            MAX_FUNNEL_NEXT_ACTION_LIMIT,
        )?;
    }
    validate_limit_offset(args.limit, None)
}

fn is_valid_funnel_duration(value: &str) -> bool {
    let Some(number) = value.strip_suffix('s') else {
        return false;
    };
    let mut parts = number.split('.');
    let Some(seconds) = parts.next() else {
        return false;
    };
    if seconds.is_empty() || !seconds.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    match (parts.next(), parts.next()) {
        (None, None) => true,
        (Some(fraction), None) => {
            !fraction.is_empty()
                && fraction.len() <= 9
                && fraction.chars().all(|ch| ch.is_ascii_digit())
        }
        _ => false,
    }
}

fn validate_optional_sublimit(
    field: &'static str,
    value: Option<u64>,
    maximum: u64,
) -> Result<(), AnalyticsError> {
    if let Some(value) = value {
        if value == 0 || value > maximum {
            return Err(AnalyticsError::invalid(
                field,
                format!("must be between 1 and {maximum}"),
            ));
        }
    }
    Ok(())
}

fn validate_date_ranges_shape(date_ranges: &[Value]) -> Result<(), AnalyticsError> {
    for (idx, date_range) in date_ranges.iter().enumerate() {
        let object = date_range.as_object().ok_or_else(|| {
            AnalyticsError::invalid(
                "date_ranges",
                format!(
                    "date_ranges[{idx}] must be an object like {{\"start_date\":\"2026-02-01\",\"end_date\":\"2026-02-07\"}}"
                ),
            )
        })?;
        let start = object
            .get("start_date")
            .or_else(|| object.get("startDate"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let end = object
            .get("end_date")
            .or_else(|| object.get("endDate"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());

        if start.is_none() || end.is_none() {
            return Err(AnalyticsError::invalid(
                "date_ranges",
                format!(
                    "date_ranges[{idx}] must include non-empty start_date/startDate and end_date/endDate strings"
                ),
            ));
        }
    }
    Ok(())
}

fn validate_pivot_inputs(
    dimensions: &[String],
    metrics: &[String],
    date_ranges: &[Value],
    pivots: &[Value],
) -> Result<(), AnalyticsError> {
    validate_report_inputs(dimensions, metrics, date_ranges, None, None)?;
    validate_pivots(pivots, dimensions)
}

fn validate_batch_run_reports_inputs(
    requests: &[BatchRunReportItemArgs],
) -> Result<(), AnalyticsError> {
    if requests.is_empty() {
        return Err(AnalyticsError::invalid(
            "requests",
            "must include at least one report request",
        ));
    }
    if requests.len() > MAX_BATCH_REPORT_REQUESTS {
        return Err(AnalyticsError::invalid(
            "requests",
            format!("must include <= {MAX_BATCH_REPORT_REQUESTS} report requests"),
        ));
    }
    Ok(())
}

fn validate_pivots(pivots: &[Value], dimensions: &[String]) -> Result<(), AnalyticsError> {
    if pivots.is_empty() {
        return Err(AnalyticsError::invalid(
            "pivots",
            "must include at least one pivot definition",
        ));
    }

    let dimension_names = dimensions
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<HashSet<_>>();
    let mut pivot_limit_product: u128 = 1;

    for (idx, pivot) in pivots.iter().enumerate() {
        let object = pivot.as_object().ok_or_else(|| {
            AnalyticsError::invalid("pivots", format!("pivot[{idx}] must be an object"))
        })?;
        let field_names_value = object
            .get("field_names")
            .or_else(|| object.get("fieldNames"))
            .ok_or_else(|| {
                AnalyticsError::invalid("pivots", format!("pivot[{idx}] must include field_names"))
            })?;
        let field_names = field_names_value.as_array().ok_or_else(|| {
            AnalyticsError::invalid(
                "pivots",
                format!("pivot[{idx}].field_names must be an array"),
            )
        })?;
        if field_names.is_empty() {
            return Err(AnalyticsError::invalid(
                "pivots",
                format!("pivot[{idx}].field_names must not be empty"),
            ));
        }
        for field_name in field_names {
            let field_name = field_name.as_str().map(str::trim).ok_or_else(|| {
                AnalyticsError::invalid(
                    "pivots",
                    format!("pivot[{idx}].field_names entries must be strings"),
                )
            })?;
            if field_name.is_empty() {
                return Err(AnalyticsError::invalid(
                    "pivots",
                    format!("pivot[{idx}].field_names must not include empty entries"),
                ));
            }
            if !dimension_names.contains(field_name) {
                return Err(AnalyticsError::invalid(
                    "pivots",
                    format!("pivot[{idx}] field '{field_name}' must also be present in dimensions"),
                ));
            }
        }

        let pivot_limit = object
            .get("limit")
            .and_then(parse_positive_u64)
            .unwrap_or(10);
        pivot_limit_product = pivot_limit_product.saturating_mul(u128::from(pivot_limit));
        if pivot_limit_product > 100_000 {
            return Err(AnalyticsError::invalid(
                "pivots",
                "pivot limit product must be <= 100000",
            ));
        }
    }
    Ok(())
}

fn parse_positive_u64(value: &Value) -> Option<u64> {
    if let Some(raw) = value.as_u64() {
        if raw > 0 {
            return Some(raw);
        }
    }
    value
        .as_str()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
}

fn summarize_report_compatibility(compatibility: &Value) -> Value {
    let dimensions = compatibility
        .get("dimensionCompatibilities")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let metrics = compatibility
        .get("metricCompatibilities")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let (compatible_dimensions, incompatible_dimensions) =
        split_compatibility_names(&dimensions, "dimensionMetadata");
    let (compatible_metrics, incompatible_metrics) =
        split_compatibility_names(&metrics, "metricMetadata");

    let mut reason_codes = Vec::new();
    let missing_metadata = dimensions.is_empty() || metrics.is_empty();
    if missing_metadata {
        reason_codes.push("MISSING_COMPATIBILITY_METADATA".to_string());
    }
    if !incompatible_dimensions.is_empty() {
        reason_codes.push("INCOMPATIBLE_DIMENSIONS".to_string());
    }
    if !incompatible_metrics.is_empty() {
        reason_codes.push("INCOMPATIBLE_METRICS".to_string());
    }
    let is_fully_compatible =
        incompatible_dimensions.is_empty() && incompatible_metrics.is_empty() && !missing_metadata;
    if is_fully_compatible {
        reason_codes.push("COMPATIBLE".to_string());
    }

    json!({
        "is_fully_compatible": is_fully_compatible,
        "reason_codes": reason_codes,
        "compatible_dimensions": compatible_dimensions,
        "incompatible_dimensions": incompatible_dimensions,
        "compatible_metrics": compatible_metrics,
        "incompatible_metrics": incompatible_metrics,
    })
}

fn split_compatibility_names(entries: &[Value], metadata_key: &str) -> (Vec<String>, Vec<String>) {
    let mut compatible = Vec::new();
    let mut incompatible = Vec::new();

    for entry in entries {
        let compatibility = entry
            .get("compatibility")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .to_ascii_uppercase();
        let name = entry
            .get(metadata_key)
            .and_then(Value::as_object)
            .and_then(|meta| {
                meta.get("apiName")
                    .or_else(|| meta.get("uiName"))
                    .or_else(|| meta.get("name"))
            })
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "<unknown>".to_string());

        if compatibility == "COMPATIBLE" {
            compatible.push(name);
        } else {
            incompatible.push(name);
        }
    }

    compatible.sort();
    compatible.dedup();
    incompatible.sort();
    incompatible.dedup();
    (compatible, incompatible)
}

fn validate_metric_lists(dimensions: &[String], metrics: &[String]) -> Result<(), AnalyticsError> {
    if dimensions.is_empty() {
        return Err(AnalyticsError::invalid(
            "dimensions",
            "must include at least one dimension",
        ));
    }
    if metrics.is_empty() {
        return Err(AnalyticsError::invalid(
            "metrics",
            "must include at least one metric",
        ));
    }
    if dimensions.len() > MAX_DIMENSIONS {
        return Err(AnalyticsError::invalid(
            "dimensions",
            format!("must include at most {MAX_DIMENSIONS} entries"),
        ));
    }
    if metrics.len() > MAX_METRICS {
        return Err(AnalyticsError::invalid(
            "metrics",
            format!("must include at most {MAX_METRICS} entries"),
        ));
    }
    for value in dimensions {
        if value.trim().is_empty() {
            return Err(AnalyticsError::invalid(
                "dimensions",
                "contains an empty entry",
            ));
        }
    }
    for value in metrics {
        if value.trim().is_empty() {
            return Err(AnalyticsError::invalid(
                "metrics",
                "contains an empty entry",
            ));
        }
    }
    Ok(())
}

fn validate_limit_offset(limit: Option<u64>, offset: Option<u64>) -> Result<(), AnalyticsError> {
    if let Some(value) = limit {
        if value == 0 {
            return Err(AnalyticsError::invalid(
                "limit",
                "must be greater than zero",
            ));
        }
        if value > MAX_REPORT_LIMIT {
            return Err(AnalyticsError::invalid(
                "limit",
                format!("must be <= {MAX_REPORT_LIMIT}"),
            ));
        }
    }
    if let Some(value) = offset {
        if value > MAX_REPORT_LIMIT * 10 {
            return Err(AnalyticsError::invalid(
                "offset",
                "is unreasonably high for a single request",
            ));
        }
    }
    Ok(())
}

fn validate_tabular_controls(
    max_rows: Option<usize>,
    max_cell_chars: Option<usize>,
    cursor: Option<&str>,
) -> Result<(), AnalyticsError> {
    if let Some(value) = max_rows {
        if value == 0 {
            return Err(AnalyticsError::invalid(
                "max_rows",
                "must be greater than zero",
            ));
        }
        if value > MAX_TABULAR_MAX_ROWS {
            return Err(AnalyticsError::invalid(
                "max_rows",
                format!("must be <= {MAX_TABULAR_MAX_ROWS}"),
            ));
        }
    }
    if let Some(value) = max_cell_chars {
        if value == 0 {
            return Err(AnalyticsError::invalid(
                "max_cell_chars",
                "must be greater than zero",
            ));
        }
        if value > MAX_CELL_CHARS_LIMIT {
            return Err(AnalyticsError::invalid(
                "max_cell_chars",
                format!("must be <= {MAX_CELL_CHARS_LIMIT}"),
            ));
        }
    }
    if let Some(raw) = cursor {
        if raw.trim().is_empty() {
            return Err(AnalyticsError::invalid_cursor(
                "cursor must not be an empty string",
            ));
        }
    }
    Ok(())
}

fn resolve_scratchpad_list_limit(limit: Option<usize>) -> Result<usize, AnalyticsError> {
    let limit = limit.unwrap_or(DEFAULT_SCRATCHPAD_LIST_LIMIT);
    if limit == 0 {
        return Err(AnalyticsError::invalid(
            "limit",
            "must be greater than zero",
        ));
    }
    if limit > MAX_SCRATCHPAD_LIST_LIMIT {
        return Err(AnalyticsError::invalid(
            "limit",
            format!("must be <= {MAX_SCRATCHPAD_LIST_LIMIT}"),
        ));
    }
    Ok(limit)
}

#[derive(Debug, Clone)]
struct IngestColumnMapping {
    source_name: String,
    target_name: String,
    logical_type: String,
    transform: IngestValueTransform,
}

#[derive(Debug, Clone, Copy)]
enum IngestValueTransform {
    Identity,
    ParseGaDate,
    ParseGaDateHour,
}

#[derive(Debug, Clone)]
struct IngestPaginationStats {
    pages_fetched: usize,
    page_size: u64,
    initial_offset: u64,
    final_offset: u64,
    requested_limit: Option<u64>,
    complete: bool,
}

fn ingest_mode_label(mode: ScratchpadIngestMode) -> &'static str {
    match mode {
        ScratchpadIngestMode::Create => "create",
        ScratchpadIngestMode::Append => "append",
    }
}

fn resolve_ingest_page_size(max_rows: Option<usize>, requested_limit: Option<u64>) -> u64 {
    let mut page_size = max_rows
        .map(|value| value as u64)
        .unwrap_or(DEFAULT_SCRATCHPAD_INGEST_PAGE_SIZE)
        .max(1)
        .min(MAX_TABULAR_MAX_ROWS as u64);
    if let Some(limit) = requested_limit {
        page_size = page_size.min(limit.max(1));
    }
    page_size
}

async fn collect_run_report_projection_for_ingest(
    server: &AnalyticsMcp,
    report: &RunReportArgs,
) -> Result<(GaTabularProjection, IngestPaginationStats), AnalyticsError> {
    let page_size = resolve_ingest_page_size(report.max_rows, report.limit);
    let requested_limit = report.limit;
    let initial_offset = report.offset.unwrap_or(0);
    let mut next_offset = initial_offset;
    let mut remaining = requested_limit;
    let mut pages_fetched = 0usize;
    let mut collected_rows = Vec::new();
    let mut columns: Option<Vec<contract::ColumnMeta>> = None;
    let mut ga_meta: Option<Value> = None;
    let mut row_count_total = 0usize;

    loop {
        if pages_fetched >= MAX_SCRATCHPAD_INGEST_PAGES {
            return Err(AnalyticsError::scratchpad_limit(
                "limit",
                format!(
                    "ingest requires more than {MAX_SCRATCHPAD_INGEST_PAGES} API pages; narrow date_ranges or set a lower limit"
                ),
            ));
        }

        let request_limit = match remaining {
            Some(0) => {
                break;
            }
            Some(value) => value.min(page_size),
            None => page_size,
        };
        if request_limit == 0 {
            break;
        }

        let request = RunReportRequest {
            property_id: report.property_id.clone(),
            date_ranges: report.date_ranges.clone(),
            dimensions: report.dimensions.clone(),
            metrics: report.metrics.clone(),
            dimension_filter: report.dimension_filter.clone(),
            metric_filter: report.metric_filter.clone(),
            order_bys: report.order_bys.clone(),
            limit: Some(request_limit),
            offset: Some(next_offset),
            currency_code: report.currency_code.clone(),
            return_property_quota: report.return_property_quota,
        };
        let data = server.client.run_report(request).await?;
        let projection = project_ga_tabular_response(&data, "run_report");
        pages_fetched = pages_fetched.saturating_add(1);
        if columns.is_none() {
            columns = Some(projection.columns.clone());
        }
        if ga_meta.is_none() {
            ga_meta = Some(projection.ga_meta.clone());
        }
        row_count_total = row_count_total.max(projection.row_count_total);
        let page_rows = projection.rows;
        let row_count_returned = page_rows.len();

        if collected_rows.len().saturating_add(row_count_returned) > MAX_SCRATCHPAD_INGEST_ROWS {
            return Err(AnalyticsError::scratchpad_limit(
                "rows",
                format!(
                    "ingest would exceed {MAX_SCRATCHPAD_INGEST_ROWS} rows; narrow date_ranges or set a lower limit"
                ),
            ));
        }

        collected_rows.extend(page_rows);
        next_offset = next_offset.saturating_add(row_count_returned as u64);
        if let Some(value) = remaining.as_mut() {
            *value = value.saturating_sub(row_count_returned as u64);
        }

        let reached_upstream_end = (next_offset as usize) >= row_count_total;
        let short_page = (row_count_returned as u64) < request_limit;
        let exhausted_requested_limit = remaining == Some(0);
        if row_count_returned == 0
            || reached_upstream_end
            || short_page
            || exhausted_requested_limit
        {
            break;
        }
    }

    row_count_total = row_count_total.max(collected_rows.len());
    let complete = remaining == Some(0) || (next_offset as usize) >= row_count_total;
    let pagination = IngestPaginationStats {
        pages_fetched,
        page_size,
        initial_offset,
        final_offset: next_offset,
        requested_limit,
        complete,
    };

    let mut ga_meta = ga_meta.unwrap_or_else(|| json!({}));
    if let Value::Object(ref mut map) = ga_meta {
        map.insert(
            "ingest_pagination".to_string(),
            json!({
                "pages_fetched": pagination.pages_fetched,
                "page_size": pagination.page_size,
                "initial_offset": pagination.initial_offset,
                "final_offset": pagination.final_offset,
                "requested_limit": pagination.requested_limit,
                "complete": pagination.complete,
            }),
        );
    }

    Ok((
        GaTabularProjection {
            rows: collected_rows,
            row_count_total,
            columns: columns.unwrap_or_default(),
            ga_meta,
        },
        pagination,
    ))
}

async fn collect_run_realtime_projection_for_ingest(
    server: &AnalyticsMcp,
    request_args: &RunRealtimeReportArgs,
) -> Result<(GaTabularProjection, IngestPaginationStats), AnalyticsError> {
    let page_size = resolve_ingest_page_size(request_args.max_rows, request_args.limit);
    let requested_limit = request_args.limit;
    let initial_offset = request_args.offset.unwrap_or(0);
    let mut next_offset = initial_offset;
    let mut remaining = requested_limit;
    let mut pages_fetched = 0usize;
    let mut collected_rows = Vec::new();
    let mut columns: Option<Vec<contract::ColumnMeta>> = None;
    let mut ga_meta: Option<Value> = None;
    let mut row_count_total = 0usize;

    loop {
        if pages_fetched >= MAX_SCRATCHPAD_INGEST_PAGES {
            return Err(AnalyticsError::scratchpad_limit(
                "limit",
                format!(
                    "ingest requires more than {MAX_SCRATCHPAD_INGEST_PAGES} API pages; narrow dimensions/filters or set a lower limit"
                ),
            ));
        }

        let request_limit = match remaining {
            Some(0) => {
                break;
            }
            Some(value) => value.min(page_size),
            None => page_size,
        };
        if request_limit == 0 {
            break;
        }

        let request = RunRealtimeReportRequest {
            property_id: request_args.property_id.clone(),
            dimensions: request_args.dimensions.clone(),
            metrics: request_args.metrics.clone(),
            dimension_filter: request_args.dimension_filter.clone(),
            metric_filter: request_args.metric_filter.clone(),
            order_bys: request_args.order_bys.clone(),
            limit: Some(request_limit),
            offset: Some(next_offset),
            return_property_quota: request_args.return_property_quota,
        };
        let data = server.client.run_realtime_report(request).await?;
        let projection = project_ga_tabular_response(&data, "run_realtime_report");
        pages_fetched = pages_fetched.saturating_add(1);
        if columns.is_none() {
            columns = Some(projection.columns.clone());
        }
        if ga_meta.is_none() {
            ga_meta = Some(projection.ga_meta.clone());
        }
        row_count_total = row_count_total.max(projection.row_count_total);
        let page_rows = projection.rows;
        let row_count_returned = page_rows.len();

        if collected_rows.len().saturating_add(row_count_returned) > MAX_SCRATCHPAD_INGEST_ROWS {
            return Err(AnalyticsError::scratchpad_limit(
                "rows",
                format!(
                    "ingest would exceed {MAX_SCRATCHPAD_INGEST_ROWS} rows; narrow dimensions/filters or set a lower limit"
                ),
            ));
        }

        collected_rows.extend(page_rows);
        next_offset = next_offset.saturating_add(row_count_returned as u64);
        if let Some(value) = remaining.as_mut() {
            *value = value.saturating_sub(row_count_returned as u64);
        }

        let reached_upstream_end = (next_offset as usize) >= row_count_total;
        let short_page = (row_count_returned as u64) < request_limit;
        let exhausted_requested_limit = remaining == Some(0);
        if row_count_returned == 0
            || reached_upstream_end
            || short_page
            || exhausted_requested_limit
        {
            break;
        }
    }

    row_count_total = row_count_total.max(collected_rows.len());
    let complete = remaining == Some(0) || (next_offset as usize) >= row_count_total;
    let pagination = IngestPaginationStats {
        pages_fetched,
        page_size,
        initial_offset,
        final_offset: next_offset,
        requested_limit,
        complete,
    };

    let mut ga_meta = ga_meta.unwrap_or_else(|| json!({}));
    if let Value::Object(ref mut map) = ga_meta {
        map.insert(
            "ingest_pagination".to_string(),
            json!({
                "pages_fetched": pagination.pages_fetched,
                "page_size": pagination.page_size,
                "initial_offset": pagination.initial_offset,
                "final_offset": pagination.final_offset,
                "requested_limit": pagination.requested_limit,
                "complete": pagination.complete,
            }),
        );
    }

    Ok((
        GaTabularProjection {
            rows: collected_rows,
            row_count_total,
            columns: columns.unwrap_or_default(),
            ga_meta,
        },
        pagination,
    ))
}

fn ingest_projection_into_scratchpad(
    server: &AnalyticsMcp,
    session_id: &str,
    requested_table_name: &str,
    normalized_table_name: &str,
    projection: GaTabularProjection,
    ingest_tool_name: &'static str,
    report_kind: &'static str,
    ingest_mode: ScratchpadIngestMode,
    pagination: Option<IngestPaginationStats>,
) -> Result<Value, AnalyticsError> {
    let mappings = build_ingest_column_mappings(&projection.columns);
    if mappings.is_empty() {
        return Err(AnalyticsError::invalid(
            "columns",
            "ingest projection must include at least one column",
        ));
    }
    if mappings.len() > MAX_SCRATCHPAD_INGEST_COLUMNS {
        return Err(AnalyticsError::invalid(
            "columns",
            format!("must include <= {MAX_SCRATCHPAD_INGEST_COLUMNS} columns"),
        ));
    }

    let normalized_rows = remap_rows_for_ingest(&projection.rows, &mappings);
    if normalized_rows.len() > MAX_SCRATCHPAD_INGEST_ROWS {
        return Err(AnalyticsError::invalid(
            "rows",
            format!("must include <= {MAX_SCRATCHPAD_INGEST_ROWS} rows per ingest call"),
        ));
    }

    let ingest_columns = mappings
        .iter()
        .map(|mapping| ScratchpadIngestColumn {
            name: mapping.target_name.clone(),
            logical_type: mapping.logical_type.clone(),
        })
        .collect::<Vec<_>>();

    let ingest_started = Instant::now();
    let stats = match server.scratchpad_sessions.ingest_rows_with_mode(
        session_id,
        normalized_table_name,
        &ingest_columns,
        &normalized_rows,
        ingest_mode,
    ) {
        Ok(stats) => {
            emit_event(
                Level::INFO,
                "ga4_mcp.scratchpad.ingest",
                &EventContext::new()
                    .with_tool_name(ingest_tool_name)
                    .with_session_id(session_id),
                &[
                    safe_text("tool", ingest_tool_name),
                    safe_text("report_kind", report_kind),
                    safe_text("table_name", normalized_table_name),
                    safe_text("ingest_mode", ingest_mode_label(ingest_mode)),
                    safe_text("rows_inserted", stats.rows_inserted.to_string()),
                    safe_text("columns_inserted", stats.columns_inserted.to_string()),
                    safe_text(
                        "duration_ms",
                        contract::elapsed_ms(ingest_started).to_string(),
                    ),
                ],
            );
            stats
        }
        Err(err) => {
            emit_event(
                Level::WARN,
                "ga4_mcp.scratchpad.ingest.error",
                &EventContext::new()
                    .with_tool_name(ingest_tool_name)
                    .with_session_id(session_id),
                &[
                    safe_text("tool", ingest_tool_name),
                    safe_text("report_kind", report_kind),
                    safe_text("table_name", normalized_table_name),
                    safe_text("ingest_mode", ingest_mode_label(ingest_mode)),
                    safe_text(
                        "duration_ms",
                        contract::elapsed_ms(ingest_started).to_string(),
                    ),
                    safe_error("error", &err),
                ],
            );
            return Err(err);
        }
    };

    Ok(json!({
        "session_id": session_id.trim(),
        "table_name": {
            "requested": requested_table_name,
            "normalized": normalized_table_name
        },
        "ingest": {
            "rows_inserted": stats.rows_inserted,
            "columns_inserted": stats.columns_inserted
        },
        "session_usage": {
            "tables_used": stats.session_snapshot.tables_used,
            "tables_remaining": stats.session_snapshot.tables_remaining,
            "rows_used": stats.session_snapshot.rows_used,
            "rows_remaining": stats.session_snapshot.rows_remaining
        },
        "source": {
            "report_kind": report_kind,
            "ingest_mode": ingest_mode_label(ingest_mode),
            "row_count_total": projection.row_count_total,
            "row_count_returned": projection.rows.len(),
            "truncated": projection.row_count_total > projection.rows.len(),
            "pagination": pagination.as_ref().map(|stats| json!({
                "enabled": true,
                "pages_fetched": stats.pages_fetched,
                "page_size": stats.page_size,
                "initial_offset": stats.initial_offset,
                "final_offset": stats.final_offset,
                "requested_limit": stats.requested_limit,
                "complete": stats.complete
            }))
        },
        "column_mapping": mappings.iter().map(|mapping| json!({
            "source_name": mapping.source_name,
            "target_name": mapping.target_name,
            "logical_type": mapping.logical_type
        })).collect::<Vec<_>>(),
        "ga4": projection.ga_meta
    }))
}

fn build_ingest_column_mappings(columns: &[contract::ColumnMeta]) -> Vec<IngestColumnMapping> {
    let mut dedupe = HashMap::<String, usize>::new();
    let mut mappings = Vec::new();

    for (index, column) in columns.iter().enumerate() {
        let fallback = format!("col_{}", index + 1);
        let base = normalize_sql_identifier(&column.name, &fallback);
        let logical_type = column
            .logical_type
            .clone()
            .unwrap_or_else(|| "string".to_string());
        let raw_target = next_mapping_target(&mut dedupe, &base);
        mappings.push(IngestColumnMapping {
            source_name: column.name.clone(),
            target_name: raw_target,
            logical_type,
            transform: IngestValueTransform::Identity,
        });

        if let Some((derived_suffix, derived_type, derived_transform)) =
            temporal_mapping_for_source_name(&column.name)
        {
            let derived_base = format!("{base}_{derived_suffix}");
            let derived_target = next_mapping_target(&mut dedupe, &derived_base);
            mappings.push(IngestColumnMapping {
                source_name: column.name.clone(),
                target_name: derived_target,
                logical_type: derived_type.to_string(),
                transform: derived_transform,
            });
        }
    }

    mappings
}

fn next_mapping_target(dedupe: &mut HashMap<String, usize>, base: &str) -> String {
    let next = dedupe.entry(base.to_string()).or_insert(0);
    let target_name = if *next == 0 {
        base.to_string()
    } else {
        format!("{base}_{}", *next + 1)
    };
    *next += 1;
    target_name
}

fn temporal_mapping_for_source_name(
    source_name: &str,
) -> Option<(&'static str, &'static str, IngestValueTransform)> {
    let normalized = source_name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect::<String>();
    match normalized.as_str() {
        "date" => Some(("parsed", "date", IngestValueTransform::ParseGaDate)),
        "datehour" => Some(("parsed", "timestamp", IngestValueTransform::ParseGaDateHour)),
        _ => None,
    }
}

fn remap_rows_for_ingest(
    rows: &[Map<String, Value>],
    mappings: &[IngestColumnMapping],
) -> Vec<Map<String, Value>> {
    rows.iter()
        .map(|row| {
            let mut output = Map::new();
            for mapping in mappings {
                let mapped_value = remap_value_for_ingest_mapping(
                    row.get(&mapping.source_name),
                    mapping.transform,
                );
                output.insert(mapping.target_name.clone(), mapped_value);
            }
            output
        })
        .collect()
}

fn remap_value_for_ingest_mapping(value: Option<&Value>, transform: IngestValueTransform) -> Value {
    match transform {
        IngestValueTransform::Identity => value.cloned().unwrap_or(Value::Null),
        IngestValueTransform::ParseGaDate => value
            .and_then(value_as_trimmed_string)
            .and_then(|raw| normalize_ga_date_literal(&raw))
            .map(Value::String)
            .unwrap_or(Value::Null),
        IngestValueTransform::ParseGaDateHour => value
            .and_then(value_as_trimmed_string)
            .and_then(|raw| normalize_ga_date_hour_literal(&raw))
            .map(Value::String)
            .unwrap_or(Value::Null),
    }
}

fn value_as_trimmed_string(value: &Value) -> Option<String> {
    let raw = match value {
        Value::String(value) => value.trim().to_string(),
        Value::Number(value) => value.to_string(),
        _ => return None,
    };
    if raw.is_empty() { None } else { Some(raw) }
}

fn normalize_ga_date_literal(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let year = value[0..4].parse::<u32>().ok()?;
    let month = value[4..6].parse::<u32>().ok()?;
    let day = value[6..8].parse::<u32>().ok()?;
    if month == 0 || month > 12 {
        return None;
    }
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if day == 0 || day > max_day {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn normalize_ga_date_hour_literal(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.len() != 10 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let date = normalize_ga_date_literal(&value[0..8])?;
    let hour = value[8..10].parse::<u32>().ok()?;
    if hour > 23 {
        return None;
    }
    Some(format!("{date} {hour:02}:00:00"))
}

fn normalize_table_identifier(raw: &str) -> Result<String, AnalyticsError> {
    let normalized = normalize_sql_identifier(raw, "table");
    if normalized.len() > 63 {
        return Err(AnalyticsError::invalid(
            "table_name",
            "normalized table name must be <= 63 characters",
        ));
    }
    Ok(normalized)
}

fn normalize_sql_identifier(raw: &str, fallback: &str) -> String {
    let mut normalized = String::with_capacity(raw.len());
    let mut previous_underscore = false;

    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
            previous_underscore = false;
        } else if !previous_underscore {
            normalized.push('_');
            previous_underscore = true;
        }
    }

    let normalized = normalized.trim_matches('_').to_string();
    let mut normalized = if normalized.is_empty() {
        fallback.to_string()
    } else {
        normalized
    };
    if normalized
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_digit())
    {
        normalized.insert(0, '_');
    }
    normalized
}

fn parse_iso_date_literal(raw: &str) -> Result<String, AnalyticsError> {
    let value = raw.trim();
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(AnalyticsError::invalid(
            "release_date",
            "must use YYYY-MM-DD format",
        ));
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(idx, byte)| idx == 4 || idx == 7 || byte.is_ascii_digit())
    {
        return Err(AnalyticsError::invalid(
            "release_date",
            "must use YYYY-MM-DD format",
        ));
    }

    let year = value[0..4]
        .parse::<u32>()
        .map_err(|_| AnalyticsError::invalid("release_date", "year is invalid"))?;
    let month = value[5..7]
        .parse::<u32>()
        .map_err(|_| AnalyticsError::invalid("release_date", "month is invalid"))?;
    let day = value[8..10]
        .parse::<u32>()
        .map_err(|_| AnalyticsError::invalid("release_date", "day is invalid"))?;

    if month == 0 || month > 12 {
        return Err(AnalyticsError::invalid(
            "release_date",
            "month must be between 01 and 12",
        ));
    }
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 31,
    };
    if day == 0 || day > max_day {
        return Err(AnalyticsError::invalid(
            "release_date",
            "day is out of range for the given month",
        ));
    }

    Ok(value.to_string())
}

fn resolve_release_window_days(
    value: Option<u32>,
    default_value: u32,
    field: &'static str,
) -> Result<u32, AnalyticsError> {
    let resolved = value.unwrap_or(default_value);
    if resolved == 0 {
        return Err(AnalyticsError::invalid(field, "must be greater than zero"));
    }
    if resolved > MAX_RELEASE_WINDOW_DAYS {
        return Err(AnalyticsError::invalid(
            field,
            format!("must be <= {MAX_RELEASE_WINDOW_DAYS}"),
        ));
    }
    Ok(resolved)
}

fn resolve_landing_shift_top_n(value: Option<usize>) -> Result<usize, AnalyticsError> {
    let resolved = value.unwrap_or(DEFAULT_LANDING_SHIFT_TOP_N);
    if resolved == 0 {
        return Err(AnalyticsError::invalid(
            "top_n",
            "must be greater than zero",
        ));
    }
    if resolved > MAX_LANDING_SHIFT_TOP_N {
        return Err(AnalyticsError::invalid(
            "top_n",
            format!("must be <= {MAX_LANDING_SHIFT_TOP_N}"),
        ));
    }
    Ok(resolved)
}

fn resolve_evidence_sample_rows(value: Option<usize>) -> Result<usize, AnalyticsError> {
    let resolved = value.unwrap_or(DEFAULT_EVIDENCE_SAMPLE_ROWS);
    if resolved == 0 {
        return Err(AnalyticsError::invalid(
            "sample_rows_per_table",
            "must be greater than zero",
        ));
    }
    if resolved > MAX_EVIDENCE_SAMPLE_ROWS {
        return Err(AnalyticsError::invalid(
            "sample_rows_per_table",
            format!("must be <= {MAX_EVIDENCE_SAMPLE_ROWS}"),
        ));
    }
    Ok(resolved)
}

fn escape_sql_literal(raw: &str) -> String {
    raw.replace('\'', "''")
}

fn build_release_regression_sql(
    normalized_table_name: &str,
    release_date: &str,
    anchor_event: &str,
    comparison_event: &str,
    date_column: &str,
    event_column: &str,
    metric_column: Option<&str>,
    pre_days: u32,
    transition_days: u32,
    post_days: u32,
) -> String {
    let table = quote_sql_identifier(normalized_table_name);
    let date_col = quote_sql_identifier(date_column);
    let event_col = quote_sql_identifier(event_column);
    let metric_expr = metric_column.map_or_else(
        || "1.0".to_string(),
        |column| {
            format!(
                "COALESCE(TRY_CAST({} AS DOUBLE), 0.0)",
                quote_sql_identifier(column)
            )
        },
    );
    let release_literal = escape_sql_literal(release_date);
    let anchor_literal = escape_sql_literal(anchor_event);
    let comparison_literal = escape_sql_literal(comparison_event);

    format!(
        "WITH normalized AS (
            SELECT
                COALESCE(
                    TRY_CAST({date_col} AS DATE),
                    CAST(TRY_STRPTIME(CAST({date_col} AS VARCHAR), '%Y%m%d') AS DATE),
                    CAST(TRY_STRPTIME(CAST({date_col} AS VARCHAR), '%Y-%m-%d') AS DATE)
                ) AS event_date,
                CAST({event_col} AS VARCHAR) AS event_name,
                {metric_expr} AS metric_value
            FROM {table}
        ),
        windowed AS (
            SELECT
                CASE
                    WHEN event_date >= DATE '{release_literal}' - INTERVAL '{pre_days} day'
                     AND event_date < DATE '{release_literal}' THEN 'pre'
                    WHEN event_date >= DATE '{release_literal}'
                     AND event_date < DATE '{release_literal}' + INTERVAL '{transition_days} day' THEN 'transition'
                    WHEN event_date >= DATE '{release_literal}' + INTERVAL '{transition_days} day'
                     AND event_date < DATE '{release_literal}' + INTERVAL '{post_days} day' THEN 'post'
                    ELSE NULL
                END AS period,
                event_name,
                metric_value
            FROM normalized
            WHERE event_date IS NOT NULL
        ),
        daily AS (
            SELECT
                period,
                event_date,
                SUM(CASE WHEN event_name = '{anchor_literal}' THEN metric_value ELSE 0 END) AS anchor_value,
                SUM(CASE WHEN event_name = '{comparison_literal}' THEN metric_value ELSE 0 END) AS comparison_value
            FROM windowed
            WHERE period IS NOT NULL
            GROUP BY period, event_date
        ),
        daily_ratio AS (
            SELECT
                period,
                event_date,
                anchor_value,
                comparison_value,
                CASE WHEN comparison_value > 0 THEN anchor_value / comparison_value END AS ratio_value
            FROM daily
        ),
        stats AS (
            SELECT
                period,
                COUNT(*) FILTER (WHERE ratio_value IS NOT NULL) AS ratio_n_days,
                AVG(ratio_value) AS ratio_mean,
                STDDEV_SAMP(ratio_value) AS ratio_sd
            FROM daily_ratio
            GROUP BY period
        ),
        aggregated AS (
            SELECT
                period,
                SUM(CASE WHEN event_name = '{anchor_literal}' THEN metric_value ELSE 0 END) AS anchor_value,
                SUM(CASE WHEN event_name = '{comparison_literal}' THEN metric_value ELSE 0 END) AS comparison_value
            FROM windowed
            WHERE period IS NOT NULL
            GROUP BY period
        ),
        rolled AS (
            SELECT
                COALESCE(MAX(CASE WHEN period = 'pre' THEN anchor_value END), 0.0) AS anchor_pre,
                COALESCE(MAX(CASE WHEN period = 'transition' THEN anchor_value END), 0.0) AS anchor_transition,
                COALESCE(MAX(CASE WHEN period = 'post' THEN anchor_value END), 0.0) AS anchor_post,
                COALESCE(MAX(CASE WHEN period = 'pre' THEN comparison_value END), 0.0) AS comparison_pre,
                COALESCE(MAX(CASE WHEN period = 'transition' THEN comparison_value END), 0.0) AS comparison_transition,
                COALESCE(MAX(CASE WHEN period = 'post' THEN comparison_value END), 0.0) AS comparison_post,
                COALESCE(MAX(CASE WHEN period = 'pre' THEN ratio_n_days END), 0) AS ratio_n_pre,
                COALESCE(MAX(CASE WHEN period = 'post' THEN ratio_n_days END), 0) AS ratio_n_post,
                MAX(CASE WHEN period = 'pre' THEN ratio_mean END) AS ratio_mean_pre,
                MAX(CASE WHEN period = 'post' THEN ratio_mean END) AS ratio_mean_post,
                MAX(CASE WHEN period = 'pre' THEN ratio_sd END) AS ratio_sd_pre,
                MAX(CASE WHEN period = 'post' THEN ratio_sd END) AS ratio_sd_post
            FROM aggregated
            LEFT JOIN stats USING (period)
        ),
        ratioed AS (
            SELECT
                *,
                CASE WHEN comparison_pre > 0 THEN anchor_pre / comparison_pre END AS ratio_pre,
                CASE WHEN comparison_post > 0 THEN anchor_post / comparison_post END AS ratio_post,
                CASE
                    WHEN ratio_mean_pre IS NOT NULL AND ratio_mean_post IS NOT NULL
                        THEN ratio_mean_post - ratio_mean_pre
                    ELSE NULL
                END AS ratio_mean_delta,
                CASE
                    WHEN ratio_n_pre > 1 AND ratio_n_post > 1 THEN
                        SQRT(
                            (COALESCE(ratio_sd_pre, 0.0) * COALESCE(ratio_sd_pre, 0.0) / ratio_n_pre) +
                            (COALESCE(ratio_sd_post, 0.0) * COALESCE(ratio_sd_post, 0.0) / ratio_n_post)
                        )
                    ELSE NULL
                END AS ratio_mean_delta_se
            FROM rolled
        )
        SELECT
            DATE '{release_literal}' AS release_date,
            '{anchor_literal}' AS anchor_event,
            '{comparison_literal}' AS comparison_event,
            anchor_pre,
            anchor_transition,
            anchor_post,
            comparison_pre,
            comparison_transition,
            comparison_post,
            ratio_pre,
            ratio_post,
            ratio_n_pre,
            ratio_n_post,
            ratio_mean_pre,
            ratio_mean_post,
            ratio_sd_pre,
            ratio_sd_post,
            ratio_mean_delta,
            ratio_mean_delta_se,
            CASE
                WHEN ratio_mean_delta_se IS NOT NULL AND ratio_mean_delta_se > 0 AND ratio_mean_delta IS NOT NULL
                    THEN ratio_mean_delta / ratio_mean_delta_se
                ELSE NULL
            END AS ratio_mean_delta_z,
            CASE
                WHEN ratio_pre IS NOT NULL AND ratio_pre != 0 AND ratio_post IS NOT NULL
                    THEN ((ratio_post - ratio_pre) / ratio_pre) * 100
                ELSE NULL
            END AS ratio_delta_pct,
            CASE
                WHEN anchor_pre = 0 AND comparison_pre = 0 THEN 'insufficient_baseline'
                WHEN ratio_pre IS NULL OR ratio_post IS NULL THEN 'insufficient_compare_volume'
                WHEN anchor_post < anchor_pre * 0.65 AND comparison_post >= comparison_pre * 0.90
                    THEN 'likely_instrumentation_break'
                WHEN anchor_post < anchor_pre * 0.75 AND comparison_post < comparison_pre * 0.75
                    THEN 'likely_behavior_shift'
                WHEN ratio_pre > 0 AND ABS((ratio_post - ratio_pre) / ratio_pre) <= 0.20
                    THEN 'likely_stable'
                ELSE 'inconclusive'
            END AS confidence_flag
        FROM ratioed"
    )
}

fn build_landing_param_shift_sql(
    normalized_table_name: &str,
    release_date: &str,
    date_column: &str,
    landing_url_column: &str,
    channel_column: Option<&str>,
    source_medium_column: Option<&str>,
    pre_days: u32,
    transition_days: u32,
    post_days: u32,
    top_n: usize,
) -> String {
    let table = quote_sql_identifier(normalized_table_name);
    let date_col = quote_sql_identifier(date_column);
    let url_col = quote_sql_identifier(landing_url_column);
    let channel_expr = channel_column.map_or_else(
        || "'all'".to_string(),
        |column| {
            format!(
                "COALESCE(CAST({} AS VARCHAR), 'all')",
                quote_sql_identifier(column)
            )
        },
    );
    let source_medium_expr = source_medium_column.map_or_else(
        || "'all'".to_string(),
        |column| {
            format!(
                "COALESCE(CAST({} AS VARCHAR), 'all')",
                quote_sql_identifier(column)
            )
        },
    );
    let release_literal = escape_sql_literal(release_date);
    let post_window_days = transition_days + post_days;

    format!(
        "WITH normalized AS (
            SELECT
                COALESCE(
                    TRY_CAST({date_col} AS DATE),
                    CAST(TRY_STRPTIME(CAST({date_col} AS VARCHAR), '%Y%m%d') AS DATE),
                    CAST(TRY_STRPTIME(CAST({date_col} AS VARCHAR), '%Y-%m-%d') AS DATE)
                ) AS event_date,
                CAST({url_col} AS VARCHAR) AS landing_url,
                {channel_expr} AS channel,
                {source_medium_expr} AS source_medium
            FROM {table}
        ),
        windowed AS (
            SELECT
                CASE
                    WHEN event_date >= DATE '{release_literal}' - INTERVAL '{pre_days} day'
                     AND event_date < DATE '{release_literal}' THEN 'pre'
                    WHEN event_date >= DATE '{release_literal}' + INTERVAL '{transition_days} day'
                     AND event_date < DATE '{release_literal}' + INTERVAL '{post_window_days} day' THEN 'post'
                    ELSE NULL
                END AS period,
                channel,
                source_medium,
                COALESCE(NULLIF(split_part(landing_url, '?', 1), ''), '(none)') AS landing_path,
                split_part(landing_url, '?', 2) AS query_string
            FROM normalized
            WHERE event_date IS NOT NULL
        ),
        exploded AS (
            SELECT
                period,
                channel,
                source_medium,
                landing_path,
                CASE
                    WHEN strpos(param_pair, '=') > 0 THEN lower(split_part(param_pair, '=', 1))
                    ELSE lower(param_pair)
                END AS param_key,
                CASE
                    WHEN strpos(param_pair, '=') > 0 THEN NULLIF(split_part(param_pair, '=', 2), '')
                    ELSE NULL
                END AS param_value
            FROM windowed
            CROSS JOIN UNNEST(
                CASE
                    WHEN query_string IS NULL OR query_string = '' THEN ['']
                    ELSE string_split(query_string, '&')
                END
            ) AS pairs(param_pair)
            WHERE period IN ('pre', 'post')
        ),
        aggregated AS (
            SELECT
                channel,
                source_medium,
                landing_path,
                param_key,
                COALESCE(param_value, '(empty)') AS param_value,
                SUM(CASE WHEN period = 'pre' THEN 1 ELSE 0 END) AS pre_rows,
                SUM(CASE WHEN period = 'post' THEN 1 ELSE 0 END) AS post_rows
            FROM exploded
            WHERE param_key IS NOT NULL AND param_key <> ''
            GROUP BY
                channel,
                source_medium,
                landing_path,
                param_key,
                COALESCE(param_value, '(empty)')
        ),
        scored AS (
            SELECT
                channel,
                source_medium,
                landing_path,
                param_key,
                param_value,
                md5(param_key || '=' || param_value) AS param_signature,
                pre_rows,
                post_rows,
                post_rows - pre_rows AS delta_rows,
                CASE
                    WHEN pre_rows > 0 THEN ((post_rows - pre_rows) * 100.0) / pre_rows
                    ELSE NULL
                END AS delta_pct,
                CASE
                    WHEN pre_rows = 0 AND post_rows > 0 THEN 'new_in_post'
                    WHEN pre_rows > 0 AND post_rows = 0 THEN 'disappeared_post'
                    ELSE 'shifted'
                END AS shift_type
            FROM aggregated
        ),
        ranked AS (
            SELECT
                *,
                ROW_NUMBER() OVER (
                    ORDER BY ABS(delta_rows) DESC, post_rows DESC, pre_rows DESC, channel, source_medium, landing_path, param_key, param_value
                ) AS rank_by_abs_delta
            FROM scored
        )
        SELECT
            DATE '{release_literal}' AS release_date,
            channel,
            source_medium,
            landing_path,
            param_key,
            param_value,
            param_signature,
            pre_rows,
            post_rows,
            delta_rows,
            delta_pct,
            shift_type,
            rank_by_abs_delta
        FROM ranked
        WHERE rank_by_abs_delta <= {top_n}
        ORDER BY rank_by_abs_delta"
    )
}

fn run_scratchpad_query_contract(
    server: &AnalyticsMcp,
    started: Instant,
    tool_name: &'static str,
    session_id: &str,
    sql: &str,
    controls: ScratchpadQueryControls,
    source_meta: Option<Value>,
) -> CallToolResult {
    if let Err(err) = validate_tabular_controls(
        controls.max_rows,
        controls.max_cell_chars,
        controls.cursor.as_deref(),
    ) {
        return contract_error(err, started);
    }

    let trimmed_sql = sql.trim();
    if trimmed_sql.is_empty() {
        return contract_error(AnalyticsError::invalid("sql", "must not be empty"), started);
    }

    let query_hash = match scratchpad_query_hash(tool_name, session_id, trimmed_sql) {
        Ok(value) => value,
        Err(err) => return contract_error(err, started),
    };

    let (offset, page_size) = match resolve_cursor_window(
        &query_hash,
        controls.cursor.as_deref(),
        None,
        controls.max_rows,
        None,
    ) {
        Ok(values) => {
            emit_pagination_window(
                tool_name,
                &query_hash,
                controls.cursor.is_some(),
                values.0,
                values.1,
            );
            values
        }
        Err(err) => {
            emit_cursor_error(tool_name, &err);
            return contract_error(err, started);
        }
    };

    let query_started = Instant::now();
    let projection =
        match execute_scratchpad_projection(server, session_id, trimmed_sql, offset, page_size) {
            Ok(value) => {
                emit_event(
                    Level::INFO,
                    "ga4_mcp.scratchpad.query.duration",
                    &EventContext::new()
                        .with_tool_name(tool_name)
                        .with_session_id(session_id),
                    &[
                        safe_text("tool", tool_name),
                        safe_text("query_hash", summarize_query_hash(&query_hash)),
                        safe_text("pagination_mode", value.pagination_mode),
                        safe_text("row_count_total", value.row_count_total.to_string()),
                        safe_text("row_count_returned", value.rows.len().to_string()),
                        safe_text(
                            "duration_ms",
                            contract::elapsed_ms(query_started).to_string(),
                        ),
                    ],
                );
                value
            }
            Err(err) => {
                emit_event(
                    Level::WARN,
                    "ga4_mcp.scratchpad.query.error",
                    &EventContext::new()
                        .with_tool_name(tool_name)
                        .with_session_id(session_id),
                    &[
                        safe_text("tool", tool_name),
                        safe_text("query_hash", summarize_query_hash(&query_hash)),
                        safe_text(
                            "duration_ms",
                            contract::elapsed_ms(query_started).to_string(),
                        ),
                        safe_error("error", &err),
                    ],
                );
                return contract_error(err, started);
            }
        };

    let tabular_projection = GaTabularProjection {
        rows: projection.rows,
        row_count_total: projection.row_count_total,
        columns: projection.columns,
        ga_meta: Value::Null,
    };
    let (payload, mut tabular_meta) = project_payload_for_mode(
        &tabular_projection,
        controls.output_mode,
        controls.summary_only,
        controls.max_cell_chars,
    );

    tabular_meta.query_hash = Some(query_hash.clone());
    let next_offset = offset.saturating_add(tabular_meta.row_count_returned as u64);
    tabular_meta.next_cursor = if (next_offset as usize) < tabular_projection.row_count_total {
        Some(encode_cursor(&query_hash, next_offset))
    } else {
        None
    };

    let hints = merge_query_hints(
        tabular_meta.query_hints.take().unwrap_or_default(),
        projection.query_hints,
    );
    if !hints.is_empty() {
        tabular_meta.query_hints = Some(hints);
    }

    let mut meta = serde_json::to_value(&tabular_meta).unwrap_or_else(|_| json!({}));
    if let Value::Object(ref mut map) = meta {
        map.insert(
            "scratchpad".to_string(),
            json!({
                "session_id": session_id.trim(),
                "pagination_mode": projection.pagination_mode,
                "query_sql_bytes": trimmed_sql.len(),
                "page_size": page_size,
                "offset": offset,
            }),
        );
        if let Some(extra) = source_meta {
            map.insert("source".to_string(), extra);
        }
    }

    contract::success_with_meta(payload, meta, contract::elapsed_ms(started))
}

fn execute_scratchpad_projection(
    server: &AnalyticsMcp,
    session_id: &str,
    sql: &str,
    offset: u64,
    page_size: u64,
) -> Result<ScratchpadTabularProjection, AnalyticsError> {
    let hooks = server.scratchpad_sessions.default_execution_hooks();
    server
        .scratchpad_sessions
        .run_guarded(session_id, sql, hooks, |conn| {
            if supports_wrapped_pagination(sql) {
                let count_sql = wrap_query_for_row_count(sql);
                let row_count_total = query_row_count(conn, &count_sql)?;
                let page_sql = wrap_query_for_page(sql, offset, page_size);
                let (columns, rows) = execute_duckdb_query_rows(conn, &page_sql)?;

                Ok(ScratchpadTabularProjection {
                    rows,
                    row_count_total,
                    columns,
                    query_hints: Vec::new(),
                    pagination_mode: "wrapped_sql",
                })
            } else {
                let (columns, rows) = execute_duckdb_query_rows(conn, sql)?;
                let row_count_total = rows.len();
                let offset = usize::try_from(offset).unwrap_or(usize::MAX);
                let limit = usize::try_from(page_size).unwrap_or(usize::MAX);
                let rows = rows
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .collect::<Vec<_>>();
                let mut hints = Vec::new();
                if offset > 0 || row_count_total > rows.len() {
                    hints.push(
                        "non-SELECT/WITH query used in-memory pagination after execution"
                            .to_string(),
                    );
                }

                Ok(ScratchpadTabularProjection {
                    rows,
                    row_count_total,
                    columns,
                    query_hints: hints,
                    pagination_mode: "in_memory",
                })
            }
        })
}

fn query_row_count(conn: &duckdb::Connection, sql: &str) -> Result<usize, AnalyticsError> {
    let mut stmt = conn.prepare(sql).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to prepare scratchpad row-count query: {err}"
        ))
    })?;
    let mut rows = stmt.query([]).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to execute scratchpad row-count query: {err}"
        ))
    })?;
    let Some(row) = rows.next().map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!("failed to fetch scratchpad row-count row: {err}"))
    })?
    else {
        return Ok(0);
    };

    let value = row.get_ref(0).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to decode scratchpad row-count value: {err}"
        ))
    })?;
    Ok(duck_value_to_usize(DuckValue::from(value)))
}

fn execute_duckdb_query_rows(
    conn: &duckdb::Connection,
    sql: &str,
) -> Result<(Vec<contract::ColumnMeta>, Vec<Map<String, Value>>), AnalyticsError> {
    let mut stmt = conn.prepare(sql).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!("failed to prepare scratchpad query: {err}"))
    })?;
    let mut rows = stmt.query([]).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!("failed to execute scratchpad query: {err}"))
    })?;

    let stmt_ref = rows
        .as_ref()
        .ok_or_else(|| AnalyticsError::Internal("missing scratchpad statement metadata".into()))?;
    let column_count = stmt_ref.column_count();
    let column_names = dedupe_column_names(stmt_ref.column_names());
    let column_types = (0..column_count)
        .map(|idx| stmt_ref.column_type(idx))
        .collect::<Vec<_>>();
    let columns = (0..column_count)
        .map(|idx| {
            contract::ColumnMeta::new(
                column_names
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| format!("column_{}", idx + 1)),
            )
            .with_logical_type(duckdb_type_to_logical_type(&column_types[idx]))
            .with_nullable(true)
        })
        .collect::<Vec<_>>();

    let mut projected_rows = Vec::new();
    while let Some(row) = rows.next().map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!("failed to fetch scratchpad query row: {err}"))
    })? {
        let mut projected = Map::new();
        for idx in 0..column_count {
            let value = row.get_ref(idx).map_err(|err| {
                AnalyticsError::ScratchpadEngine(format!(
                    "failed to decode scratchpad column value: {err}"
                ))
            })?;
            projected.insert(
                column_names[idx].clone(),
                duck_value_to_json(DuckValue::from(value)),
            );
        }
        projected_rows.push(projected);
    }

    Ok((columns, projected_rows))
}

fn dedupe_column_names(raw: Vec<String>) -> Vec<String> {
    let mut counts = HashMap::<String, usize>::new();
    raw.into_iter()
        .enumerate()
        .map(|(idx, name)| {
            let base = normalize_sql_identifier(&name, &format!("column_{}", idx + 1));
            let count = counts.entry(base.clone()).or_insert(0);
            let deduped = if *count == 0 {
                base.clone()
            } else {
                format!("{base}_{}", *count + 1)
            };
            *count += 1;
            deduped
        })
        .collect()
}

fn merge_query_hints(mut defaults: Vec<String>, extras: Vec<String>) -> Vec<String> {
    for hint in extras {
        if !defaults.iter().any(|existing| existing == &hint) {
            defaults.push(hint);
        }
    }
    defaults
}

fn supports_wrapped_pagination(sql: &str) -> bool {
    let upper = trim_sql_for_subquery(sql).to_ascii_uppercase();
    upper.starts_with("SELECT") || upper.starts_with("WITH")
}

fn trim_sql_for_subquery(sql: &str) -> &str {
    let mut trimmed = sql.trim();
    while let Some(next) = trimmed.strip_suffix(';') {
        trimmed = next.trim_end();
    }
    trimmed
}

fn wrap_query_for_row_count(sql: &str) -> String {
    let inner = trim_sql_for_subquery(sql);
    format!("SELECT COUNT(*) AS row_count_total FROM ({inner}) AS {SCRATCHPAD_QUERY_ALIAS}")
}

fn wrap_query_for_page(sql: &str, offset: u64, page_size: u64) -> String {
    let inner = trim_sql_for_subquery(sql);
    format!("SELECT * FROM ({inner}) AS {SCRATCHPAD_QUERY_ALIAS} LIMIT {page_size} OFFSET {offset}")
}

fn quote_sql_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn scratchpad_query_hash(
    tool_name: &str,
    session_id: &str,
    sql: &str,
) -> Result<String, AnalyticsError> {
    stable_query_hash(&json!({
        "tool": tool_name,
        "session_id": session_id.trim(),
        "sql": trim_sql_for_subquery(sql),
    }))
}

fn duckdb_type_to_logical_type(data_type: &DuckDataType) -> &'static str {
    match data_type {
        DuckDataType::Boolean => "boolean",
        DuckDataType::Int8
        | DuckDataType::Int16
        | DuckDataType::Int32
        | DuckDataType::Int64
        | DuckDataType::UInt8
        | DuckDataType::UInt16
        | DuckDataType::UInt32
        | DuckDataType::UInt64 => "integer",
        DuckDataType::Float16
        | DuckDataType::Float32
        | DuckDataType::Float64
        | DuckDataType::Decimal128(_, _)
        | DuckDataType::Decimal256(_, _) => "number",
        DuckDataType::Date32 | DuckDataType::Date64 => "date",
        DuckDataType::Timestamp(_, _) => "datetime",
        DuckDataType::Time32(_) | DuckDataType::Time64(_) => "time",
        DuckDataType::Binary | DuckDataType::LargeBinary | DuckDataType::FixedSizeBinary(_) => {
            "blob"
        }
        DuckDataType::List(_)
        | DuckDataType::LargeList(_)
        | DuckDataType::FixedSizeList(_, _)
        | DuckDataType::Struct(_)
        | DuckDataType::Map(_, _)
        | DuckDataType::Union(_, _)
        | DuckDataType::Dictionary(_, _) => "json",
        _ => "string",
    }
}

fn duck_value_to_json(value: DuckValue) -> Value {
    match value {
        DuckValue::Null => Value::Null,
        DuckValue::Boolean(raw) => Value::Bool(raw),
        DuckValue::TinyInt(raw) => json!(raw),
        DuckValue::SmallInt(raw) => json!(raw),
        DuckValue::Int(raw) => json!(raw),
        DuckValue::BigInt(raw) => json!(raw),
        DuckValue::HugeInt(raw) => Value::String(raw.to_string()),
        DuckValue::UTinyInt(raw) => json!(raw),
        DuckValue::USmallInt(raw) => json!(raw),
        DuckValue::UInt(raw) => json!(raw),
        DuckValue::UBigInt(raw) => json!(raw),
        DuckValue::Float(raw) => serde_json::Number::from_f64(raw as f64)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(raw.to_string())),
        DuckValue::Double(raw) => serde_json::Number::from_f64(raw)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(raw.to_string())),
        DuckValue::Decimal(raw) => Value::String(raw.to_string()),
        DuckValue::Timestamp(_, raw) => json!(raw),
        DuckValue::Text(raw) => Value::String(raw),
        DuckValue::Blob(raw) => Value::String(format!("0x{}", bytes_to_hex(&raw))),
        DuckValue::Date32(raw) => json!(raw),
        DuckValue::Time64(_, raw) => json!(raw),
        DuckValue::Interval {
            months,
            days,
            nanos,
        } => json!({
            "months": months,
            "days": days,
            "nanos": nanos
        }),
        DuckValue::List(raw) | DuckValue::Array(raw) => {
            Value::Array(raw.into_iter().map(duck_value_to_json).collect())
        }
        DuckValue::Struct(raw) => Value::Object(
            raw.iter()
                .map(|(key, value)| (key.clone(), duck_value_to_json(value.clone())))
                .collect(),
        ),
        DuckValue::Map(raw) => Value::Array(
            raw.iter()
                .map(|(key, value)| {
                    json!({
                        "key": duck_value_to_json(key.clone()),
                        "value": duck_value_to_json(value.clone()),
                    })
                })
                .collect(),
        ),
        DuckValue::Union(raw) => duck_value_to_json(*raw),
        DuckValue::Enum(raw) => Value::String(raw),
    }
}

fn duck_value_to_usize(value: DuckValue) -> usize {
    match value {
        DuckValue::TinyInt(raw) => raw.max(0) as usize,
        DuckValue::SmallInt(raw) => raw.max(0) as usize,
        DuckValue::Int(raw) => raw.max(0) as usize,
        DuckValue::BigInt(raw) => raw.max(0) as usize,
        DuckValue::HugeInt(raw) => usize::try_from(raw).unwrap_or(usize::MAX),
        DuckValue::UTinyInt(raw) => raw as usize,
        DuckValue::USmallInt(raw) => raw as usize,
        DuckValue::UInt(raw) => raw as usize,
        DuckValue::UBigInt(raw) => usize::try_from(raw).unwrap_or(usize::MAX),
        DuckValue::Float(raw) => raw.max(0.0) as usize,
        DuckValue::Double(raw) => raw.max(0.0) as usize,
        DuckValue::Text(raw) => raw.trim().parse::<usize>().unwrap_or(0),
        _ => 0,
    }
}

fn bytes_to_hex(raw: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(raw.len() * 2);
    for byte in raw {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn build_run_report_preview_payload(
    report: &RunReportArgs,
    query_hash: &str,
    effective_offset: u64,
    effective_limit: u64,
) -> Result<Value, AnalyticsError> {
    let property = report.property_id.to_resource_name()?;
    let request_payload = sort_object(json!({
        "property": property,
        "date_ranges": &report.date_ranges,
        "dimensions": &report.dimensions,
        "metrics": &report.metrics,
        "dimension_filter": &report.dimension_filter,
        "metric_filter": &report.metric_filter,
        "order_bys": &report.order_bys,
        "limit": effective_limit,
        "offset": effective_offset,
        "currency_code": &report.currency_code,
        "return_property_quota": report.return_property_quota,
    }));

    let tabular_columns = preview_report_columns(&report.dimensions, &report.metrics);
    let ingest_mappings = build_ingest_column_mappings(&tabular_columns);

    Ok(json!({
        "preview": {
            "tool": "run_report",
            "query_hash": query_hash,
            "request": request_payload,
            "pagination": {
                "requested_limit": report.limit,
                "requested_offset": report.offset.unwrap_or(0),
                "max_rows": report.max_rows,
                "effective_limit": effective_limit,
                "effective_offset": effective_offset,
                "cursor_supplied": report.cursor.is_some(),
            }
        },
        "projection": {
            "tabular_columns": tabular_columns
                .iter()
                .map(column_meta_to_json)
                .collect::<Vec<_>>(),
            "ingest_columns": ingest_mappings
                .iter()
                .map(|mapping| json!({
                    "source_name": mapping.source_name,
                    "target_name": mapping.target_name,
                    "logical_type": mapping.logical_type,
                    "transform": ingest_transform_label(mapping.transform),
                }))
                .collect::<Vec<_>>(),
        },
        "hints": [
            "preview_report_request does not execute an upstream GA API call",
            "use check_report_compatibility before large pulls to preflight dimension/metric compatibility"
        ],
    }))
}

fn preview_report_columns(dimensions: &[String], metrics: &[String]) -> Vec<contract::ColumnMeta> {
    let mut columns = Vec::with_capacity(dimensions.len() + metrics.len());
    for dimension in dimensions {
        columns.push(
            contract::ColumnMeta::new(dimension.clone())
                .with_logical_type("string")
                .with_nullable(true),
        );
    }
    for metric in metrics {
        columns.push(
            contract::ColumnMeta::new(metric.clone())
                .with_logical_type("number")
                .with_nullable(true),
        );
    }
    columns
}

fn column_meta_to_json(column: &contract::ColumnMeta) -> Value {
    json!({
        "name": column.name,
        "logical_type": column.logical_type,
        "nullable": column.nullable,
    })
}

fn ingest_transform_label(transform: IngestValueTransform) -> &'static str {
    match transform {
        IngestValueTransform::Identity => "identity",
        IngestValueTransform::ParseGaDate => "parse_ga_date",
        IngestValueTransform::ParseGaDateHour => "parse_ga_date_hour",
    }
}

fn run_report_query_hash(args: &RunReportArgs) -> Result<String, AnalyticsError> {
    let property = args.property_id.to_resource_name()?;
    let signature = json!({
        "tool": "run_report",
        "property": property,
        "date_ranges": &args.date_ranges,
        "dimensions": &args.dimensions,
        "metrics": &args.metrics,
        "dimension_filter": &args.dimension_filter,
        "metric_filter": &args.metric_filter,
        "order_bys": &args.order_bys,
        "currency_code": &args.currency_code,
        "return_property_quota": args.return_property_quota,
    });
    stable_query_hash(&signature)
}

fn run_conversions_report_query_hash(
    args: &RunConversionsReportArgs,
) -> Result<String, AnalyticsError> {
    let property = args.property_id.to_resource_name()?;
    let dimensions = args
        .dimensions
        .iter()
        .map(|name| name.trim())
        .collect::<Vec<_>>();
    let metrics = args
        .metrics
        .iter()
        .map(|name| name.trim())
        .collect::<Vec<_>>();
    let currency_code = args.currency_code.as_deref().map(str::trim);
    let signature = snake_to_camel_json(json!({
        "tool": "run_conversions_report",
        "property": property,
        "date_ranges": &args.date_ranges,
        "dimensions": dimensions,
        "metrics": metrics,
        "conversion_spec": &args.conversion_spec,
        "dimension_filter": &args.dimension_filter,
        "metric_filter": &args.metric_filter,
        "order_bys": &args.order_bys,
        "currency_code": currency_code,
        "return_property_quota": args.return_property_quota,
    }));
    stable_query_hash(&signature)
}

fn run_funnel_report_query_hash(args: &RunFunnelReportArgs) -> Result<String, AnalyticsError> {
    let property = args.property_id.to_resource_name()?;
    let funnel_steps = args
        .funnel_steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            serde_json::to_value(step)
                .map(|value| normalize_funnel_step(value, index))
                .map_err(|err| {
                    AnalyticsError::Internal(format!(
                        "failed to serialize funnel step for query hash: {err}"
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let signature = snake_to_camel_json(json!({
        "tool": "run_funnel_report",
        "property": property,
        "funnel_steps": funnel_steps,
        "is_open_funnel": args.is_open_funnel,
        "date_ranges": &args.date_ranges,
        "funnel_breakdown": &args.funnel_breakdown,
        "funnel_next_action": &args.funnel_next_action,
        "funnel_visualization_type": &args.funnel_visualization_type,
        "segments": &args.segments,
        "dimension_filter": &args.dimension_filter,
        "return_property_quota": args.return_property_quota,
    }));
    stable_query_hash(&signature)
}

fn run_realtime_query_hash(args: &RunRealtimeReportArgs) -> Result<String, AnalyticsError> {
    let property = args.property_id.to_resource_name()?;
    let signature = json!({
        "tool": "run_realtime_report",
        "property": property,
        "dimensions": &args.dimensions,
        "metrics": &args.metrics,
        "dimension_filter": &args.dimension_filter,
        "metric_filter": &args.metric_filter,
        "order_bys": &args.order_bys,
        "return_property_quota": args.return_property_quota,
    });
    stable_query_hash(&signature)
}

fn run_pivot_query_hash(args: &RunPivotReportArgs) -> Result<String, AnalyticsError> {
    let property = args.property_id.to_resource_name()?;
    let signature = json!({
        "tool": "run_pivot_report",
        "property": property,
        "date_ranges": &args.date_ranges,
        "dimensions": &args.dimensions,
        "metrics": &args.metrics,
        "pivots": &args.pivots,
        "dimension_filter": &args.dimension_filter,
        "metric_filter": &args.metric_filter,
        "order_bys": &args.order_bys,
        "currency_code": &args.currency_code,
        "keep_empty_rows": args.keep_empty_rows,
        "return_property_quota": args.return_property_quota,
    });
    stable_query_hash(&signature)
}

fn run_property_access_query_hash(
    args: &RunAccessReportPropertyArgs,
) -> Result<String, AnalyticsError> {
    let property = args.property_id.to_resource_name()?;
    let signature = json!({
        "tool": "run_property_access_report",
        "property": property,
        "date_ranges": &args.date_ranges,
        "dimensions": &args.dimensions,
        "metrics": &args.metrics,
        "dimension_filter": &args.dimension_filter,
        "metric_filter": &args.metric_filter,
        "order_bys": &args.order_bys,
        "time_zone": &args.time_zone,
    });
    stable_query_hash(&signature)
}

fn run_account_access_query_hash(
    args: &RunAccessReportAccountArgs,
) -> Result<String, AnalyticsError> {
    let account = args.account_id.to_resource_name()?;
    let signature = json!({
        "tool": "run_account_access_report",
        "account": account,
        "date_ranges": &args.date_ranges,
        "dimensions": &args.dimensions,
        "metrics": &args.metrics,
        "dimension_filter": &args.dimension_filter,
        "metric_filter": &args.metric_filter,
        "order_bys": &args.order_bys,
        "time_zone": &args.time_zone,
    });
    stable_query_hash(&signature)
}

fn batch_run_report_query_hash(
    property: &str,
    index: usize,
    args: &BatchRunReportItemArgs,
) -> Result<String, AnalyticsError> {
    let signature = json!({
        "tool": "batch_run_reports",
        "property": property,
        "index": index,
        "date_ranges": &args.date_ranges,
        "dimensions": &args.dimensions,
        "metrics": &args.metrics,
        "dimension_filter": &args.dimension_filter,
        "metric_filter": &args.metric_filter,
        "order_bys": &args.order_bys,
        "currency_code": &args.currency_code,
        "return_property_quota": args.return_property_quota,
    });
    stable_query_hash(&signature)
}

fn stable_query_hash(signature: &Value) -> Result<String, AnalyticsError> {
    let canonical = serde_json::to_string(&sort_object(signature.clone())).map_err(|err| {
        AnalyticsError::Internal(format!("failed to serialize query signature: {err}"))
    })?;
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

fn resolve_cursor_window(
    query_hash: &str,
    cursor: Option<&str>,
    requested_offset: Option<u64>,
    max_rows: Option<usize>,
    requested_limit: Option<u64>,
) -> Result<(u64, u64), AnalyticsError> {
    let mut page_size = max_rows.unwrap_or(DEFAULT_TABULAR_MAX_ROWS).max(1);
    if let Some(limit) = requested_limit {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        page_size = page_size.min(limit);
    }
    page_size = page_size.max(1).min(MAX_TABULAR_MAX_ROWS);

    let offset = if let Some(raw_cursor) = cursor {
        if requested_offset.is_some() {
            return Err(AnalyticsError::invalid(
                "offset",
                "must be omitted when cursor is supplied",
            ));
        }
        let token = decode_cursor(raw_cursor)?;
        if token.query_hash != query_hash {
            return Err(AnalyticsError::CursorQueryMismatch);
        }
        if token.offset > MAX_REPORT_LIMIT * 10 {
            return Err(AnalyticsError::invalid(
                "offset",
                "is unreasonably high for a single request",
            ));
        }
        token.offset
    } else {
        requested_offset.unwrap_or(0)
    };

    Ok((offset, page_size as u64))
}

fn resolve_funnel_report_limit(max_rows: Option<usize>, requested_limit: Option<u64>) -> u64 {
    let response_limit = max_rows
        .unwrap_or(DEFAULT_TABULAR_MAX_ROWS)
        .clamp(1, MAX_TABULAR_MAX_ROWS) as u64;
    requested_limit
        .unwrap_or(response_limit)
        .min(response_limit)
        .max(1)
}

fn decode_cursor(raw: &str) -> Result<CursorToken, AnalyticsError> {
    let parts = raw.splitn(3, ':').collect::<Vec<_>>();
    if parts.len() != 3 || parts[0] != "v1" {
        return Err(AnalyticsError::invalid_cursor(
            "expected cursor format v1:<query_hash>:<offset>",
        ));
    }
    let offset = parts[2].parse::<u64>().map_err(|_| {
        AnalyticsError::invalid_cursor("offset component must be a non-negative integer")
    })?;
    if parts[1].trim().is_empty() {
        return Err(AnalyticsError::invalid_cursor(
            "query hash component must not be empty",
        ));
    }
    Ok(CursorToken {
        query_hash: parts[1].to_string(),
        offset,
    })
}

fn encode_cursor(query_hash: &str, offset: u64) -> String {
    format!("v1:{query_hash}:{offset}")
}

fn emit_pagination_window(
    tool_name: &'static str,
    query_hash: &str,
    cursor_supplied: bool,
    offset: u64,
    page_size: u64,
) {
    emit_event(
        Level::INFO,
        "ga4_mcp.pagination.window",
        &EventContext::new().with_tool_name(tool_name),
        &[
            safe_text("tool", tool_name),
            safe_text("query_hash", summarize_query_hash(query_hash)),
            safe_text(
                "cursor_supplied",
                if cursor_supplied { "true" } else { "false" },
            ),
            safe_text("offset", offset.to_string()),
            safe_text("page_size", page_size.to_string()),
        ],
    );
}

fn emit_cursor_error(tool_name: &'static str, err: &AnalyticsError) {
    if err.reason() != "invalid_cursor" {
        return;
    }
    emit_event(
        Level::WARN,
        "ga4_mcp.pagination.cursor_error",
        &EventContext::new().with_tool_name(tool_name),
        &[
            safe_text("tool", tool_name),
            safe_text("error_code", err.code()),
            safe_text("error_reason", err.reason()),
            safe_error("error", err),
        ],
    );
}

fn summarize_query_hash(query_hash: &str) -> String {
    let hash = query_hash.trim();
    if hash.len() <= 12 {
        hash.to_string()
    } else {
        format!("{}..", &hash[..12])
    }
}

fn contract_success_ga_tabular(
    data: Value,
    started: Instant,
    report_kind: &'static str,
    options: TabularResponseOptions,
) -> CallToolResult {
    let projection = project_ga_tabular_response(&data, report_kind);
    contract_success_ga_projection(projection, started, options)
}

fn validate_ga_tabular_response_shape(
    response: &Value,
    report_kind: &'static str,
) -> Result<(), AnalyticsError> {
    let Some(response) = response.as_object() else {
        return Err(AnalyticsError::Internal(format!(
            "Google {report_kind} response must be an object"
        )));
    };

    if let Some(row_count) = response.get("rowCount") {
        if row_count.as_u64().is_none() {
            return Err(AnalyticsError::Internal(format!(
                "Google {report_kind} response field rowCount must be a non-negative integer"
            )));
        }
    }

    for field_name in ["dimensionHeaders", "metricHeaders", "rows"] {
        let Some(raw_values) = response.get(field_name) else {
            continue;
        };
        let Some(values) = raw_values.as_array() else {
            return Err(AnalyticsError::Internal(format!(
                "Google {report_kind} response field {field_name} must be an array"
            )));
        };
        for (index, value) in values.iter().enumerate() {
            if !value.is_object() {
                return Err(AnalyticsError::Internal(format!(
                    "Google {report_kind} response {field_name}[{index}] must be an object"
                )));
            }
        }
    }

    if let Some(rows) = response.get("rows").and_then(Value::as_array) {
        for (row_index, row) in rows.iter().enumerate() {
            for field_name in ["dimensionValues", "metricValues"] {
                let Some(raw_values) = row.get(field_name) else {
                    continue;
                };
                let Some(values) = raw_values.as_array() else {
                    return Err(AnalyticsError::Internal(format!(
                        "Google {report_kind} response rows[{row_index}].{field_name} must be an array"
                    )));
                };
                for (value_index, value) in values.iter().enumerate() {
                    if !value.is_object() {
                        return Err(AnalyticsError::Internal(format!(
                            "Google {report_kind} response rows[{row_index}].{field_name}[{value_index}] must be an object"
                        )));
                    }
                }
            }
        }
    }

    Ok(())
}

fn contract_success_ga_funnel(
    data: Value,
    started: Instant,
    options: FunnelResponseOptions,
) -> CallToolResult {
    let Some(funnel_table) = data.get("funnelTable").filter(|value| value.is_object()) else {
        return contract_error(
            AnalyticsError::Internal(
                "Google funnel response did not include an object-valued funnelTable subreport"
                    .to_string(),
            ),
            started,
        );
    };
    if let Err(err) = validate_funnel_subreport_shape(funnel_table, "funnelTable") {
        return contract_error(err, started);
    }
    let Some(funnel_visualization) = data
        .get("funnelVisualization")
        .filter(|value| value.is_object())
    else {
        return contract_error(
            AnalyticsError::Internal(
                "Google funnel response did not include an object-valued funnelVisualization subreport"
                    .to_string(),
            ),
            started,
        );
    };
    if let Err(err) = validate_funnel_subreport_shape(funnel_visualization, "funnelVisualization") {
        return contract_error(err, started);
    }

    let (table_payload, table_meta, table_truncated) = project_funnel_subreport(
        funnel_table,
        "funnel_table",
        options.output_mode,
        options.summary_only,
        options.max_cell_chars,
        options.effective_limit,
    );
    let (visualization_payload, visualization_meta, visualization_truncated) =
        project_funnel_subreport(
            funnel_visualization,
            "funnel_visualization",
            options.output_mode,
            options.summary_only,
            options.max_cell_chars,
            options.effective_limit,
        );

    let payload = if options.summary_only {
        Value::Null
    } else {
        json!({
            "funnel_table": table_payload,
            "funnel_visualization": visualization_payload,
        })
    };
    let meta = json!({
        "output_mode": options.output_mode,
        "summary_only": options.summary_only,
        "query_hash": options.query_hash,
        "requested_limit": options.requested_limit,
        "effective_limit": options.effective_limit,
        "truncated": table_truncated || visualization_truncated,
        "row_count_total_known": false,
        "subreports": {
            "funnel_table": table_meta,
            "funnel_visualization": visualization_meta,
        },
        "ga4": {
            "report_kind": "run_funnel_report",
            "kind": data.get("kind").cloned(),
            "property_quota": data.get("propertyQuota").cloned(),
        },
    });
    contract::success_with_meta(payload, meta, contract::elapsed_ms(started))
}

fn validate_funnel_subreport_shape(
    subreport: &Value,
    subreport_name: &str,
) -> Result<(), AnalyticsError> {
    let Some(subreport) = subreport.as_object() else {
        return Err(AnalyticsError::Internal(format!(
            "Google funnel response included a non-object {subreport_name} subreport"
        )));
    };

    for field_name in ["dimensionHeaders", "metricHeaders", "rows"] {
        let Some(raw_values) = subreport.get(field_name) else {
            continue;
        };
        let Some(values) = raw_values.as_array() else {
            return Err(AnalyticsError::Internal(format!(
                "Google funnel response {subreport_name}.{field_name} must be an array"
            )));
        };
        for (index, value) in values.iter().enumerate() {
            if !value.is_object() {
                return Err(AnalyticsError::Internal(format!(
                    "Google funnel response {subreport_name}.{field_name}[{index}] must be an object"
                )));
            }
        }
    }

    for (row_index, row) in subreport
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        for field_name in ["dimensionValues", "metricValues"] {
            let Some(values) = row.get(field_name) else {
                continue;
            };
            let Some(values) = values.as_array() else {
                return Err(AnalyticsError::Internal(format!(
                    "Google funnel response {subreport_name}.rows[{row_index}].{field_name} must be an array"
                )));
            };
            for (value_index, value) in values.iter().enumerate() {
                if !value.is_object() {
                    return Err(AnalyticsError::Internal(format!(
                        "Google funnel response {subreport_name}.rows[{row_index}].{field_name}[{value_index}] must be an object"
                    )));
                }
            }
        }
    }

    Ok(())
}

fn project_funnel_subreport(
    subreport: &Value,
    report_kind: &'static str,
    output_mode: contract::OutputMode,
    summary_only: bool,
    max_cell_chars: Option<usize>,
    effective_limit: u64,
) -> (Value, Value, bool) {
    let mut projection = project_ga_tabular_response(subreport, report_kind);
    let upstream_row_count_returned = projection.rows.len();
    let local_limit = usize::try_from(effective_limit).unwrap_or(usize::MAX);
    projection.rows.truncate(local_limit);
    let row_count_returned = projection.rows.len();
    let (payload, tabular_meta) =
        project_payload_for_mode(&projection, output_mode, summary_only, max_cell_chars);
    let locally_truncated = upstream_row_count_returned > row_count_returned;
    let possibly_truncated = locally_truncated || (row_count_returned as u64) >= effective_limit;
    let meta = json!({
        "output_mode": tabular_meta.output_mode,
        "summary_only": tabular_meta.summary_only,
        "row_count_returned": row_count_returned,
        "upstream_row_count_returned": upstream_row_count_returned,
        "row_count_total_known": false,
        "truncated": possibly_truncated,
        "truncation_basis": if locally_truncated {
            "local_response_cap"
        } else if possibly_truncated {
            "response_filled_effective_limit"
        } else {
            "response_below_effective_limit"
        },
        "columns": tabular_meta.columns,
        "cell_clipping": tabular_meta.cell_clipping,
        "ga4": projection.ga_meta,
    });
    (payload, meta, possibly_truncated)
}

fn contract_success_ga_projection(
    projection: GaTabularProjection,
    started: Instant,
    options: TabularResponseOptions,
) -> CallToolResult {
    let (payload, meta) = ga_projection_payload_and_meta(projection, options);
    contract::success_with_meta(payload, meta, contract::elapsed_ms(started))
}

fn ga_projection_payload_and_meta(
    projection: GaTabularProjection,
    options: TabularResponseOptions,
) -> (Value, Value) {
    let (payload, mut tabular_meta) = project_payload_for_mode(
        &projection,
        options.output_mode,
        options.summary_only,
        options.max_cell_chars,
    );
    tabular_meta.query_hash = Some(options.query_hash);
    let next_offset = options
        .cursor_offset
        .saturating_add(tabular_meta.row_count_returned as u64);
    tabular_meta.next_cursor =
        if tabular_meta.row_count_returned > 0 && next_offset < projection.row_count_total as u64 {
            Some(encode_cursor(
                tabular_meta.query_hash.as_deref().unwrap_or_default(),
                next_offset,
            ))
        } else {
            None
        };

    let mut meta = serde_json::to_value(&tabular_meta).unwrap_or_else(|_| json!({}));
    if let Value::Object(ref mut map) = meta {
        map.insert("ga4".to_string(), projection.ga_meta);
    }
    (payload, meta)
}

fn apply_local_window_to_projection(
    mut projection: GaTabularProjection,
    offset: u64,
    page_size: u64,
    pagination_mode: &'static str,
) -> GaTabularProjection {
    let upstream_row_count_total = projection.row_count_total;
    let available_row_count = projection.rows.len();
    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
    let page_size = usize::try_from(page_size).unwrap_or(usize::MAX);
    let rows = projection
        .rows
        .into_iter()
        .skip(offset)
        .take(page_size)
        .collect::<Vec<_>>();

    projection.rows = rows;
    projection.row_count_total = available_row_count;
    if let Value::Object(ref mut map) = projection.ga_meta {
        map.insert("pagination_mode".to_string(), json!(pagination_mode));
        map.insert(
            "available_row_count".to_string(),
            json!(available_row_count),
        );
        map.insert(
            "upstream_row_count_total".to_string(),
            json!(upstream_row_count_total),
        );
    }
    projection
}

fn project_ga_tabular_response(response: &Value, report_kind: &'static str) -> GaTabularProjection {
    let dimension_columns = extract_dimension_columns(response);
    let metric_columns = extract_metric_columns(response);

    let mut all_columns = Vec::with_capacity(dimension_columns.len() + metric_columns.len());
    all_columns.extend(dimension_columns.iter().cloned());
    all_columns.extend(metric_columns.iter().cloned());

    let row_objects = project_ga_rows_to_objects(response, &dimension_columns, &metric_columns);
    let row_count_returned = row_objects.len();
    let row_count_total = extract_row_count(response).unwrap_or(row_count_returned);

    GaTabularProjection {
        rows: row_objects,
        row_count_total,
        columns: all_columns,
        ga_meta: json!({
            "report_kind": report_kind,
            "kind": response.get("kind").cloned(),
            "metadata": response.get("metadata").cloned(),
            "property_quota": response.get("propertyQuota").cloned(),
            "quota": response.get("quota").cloned(),
            "totals": response.get("totals").cloned(),
            "maximums": response.get("maximums").cloned(),
            "minimums": response.get("minimums").cloned(),
            "aggregates": response.get("aggregates").cloned(),
            "pivot_headers": response.get("pivotHeaders").cloned(),
            "comparisons": response.get("comparisons").cloned(),
        }),
    }
}

fn extract_dimension_columns(response: &Value) -> Vec<contract::ColumnMeta> {
    response
        .get("dimensionHeaders")
        .and_then(Value::as_array)
        .map(|headers| {
            headers
                .iter()
                .enumerate()
                .map(|(idx, header)| {
                    let name = header
                        .get("name")
                        .or_else(|| header.get("dimensionName"))
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| fallback_column_name("dimension_", idx));
                    let mut column = contract::ColumnMeta::new(name).with_nullable(true);
                    if let Some(logical_type) = header.get("type").and_then(Value::as_str) {
                        column = column.with_logical_type(logical_type);
                    } else {
                        column = column.with_logical_type("string");
                    }
                    column
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_metric_columns(response: &Value) -> Vec<contract::ColumnMeta> {
    response
        .get("metricHeaders")
        .and_then(Value::as_array)
        .map(|headers| {
            headers
                .iter()
                .enumerate()
                .map(|(idx, header)| {
                    let name = header
                        .get("name")
                        .or_else(|| header.get("metricName"))
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| fallback_column_name("metric_", idx));
                    let logical_type = header
                        .get("type")
                        .and_then(Value::as_str)
                        .map(metric_header_logical_type)
                        .unwrap_or("number")
                        .to_string();
                    contract::ColumnMeta::new(name)
                        .with_logical_type(logical_type)
                        .with_nullable(true)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn metric_header_logical_type(metric_type: &str) -> &'static str {
    match metric_type {
        "TYPE_INTEGER" | "TYPE_STANDARD" | "TYPE_HOURS" | "TYPE_SECONDS" | "TYPE_MILLISECONDS" => {
            "integer"
        }
        "TYPE_FLOAT" => "number",
        "TYPE_CURRENCY" => "currency",
        "TYPE_FEET" | "TYPE_MILES" | "TYPE_METERS" | "TYPE_KILOMETERS" => "distance",
        "TYPE_MINUTES" => "duration_minutes",
        _ => "number",
    }
}

fn fallback_column_name(prefix: &str, idx: usize) -> String {
    let mut name = String::from(prefix);
    name.push_str(&idx.to_string());
    name
}

fn project_ga_rows_to_objects(
    response: &Value,
    dimension_columns: &[contract::ColumnMeta],
    metric_columns: &[contract::ColumnMeta],
) -> Vec<Map<String, Value>> {
    let rows = match response.get("rows").and_then(Value::as_array) {
        Some(rows) => rows,
        None => return Vec::new(),
    };

    rows.iter()
        .map(|row| {
            let dimension_values = row
                .get("dimensionValues")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let metric_values = row
                .get("metricValues")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            let mut projected = Map::new();
            for (idx, column) in dimension_columns.iter().enumerate() {
                let value = dimension_values
                    .get(idx)
                    .and_then(|entry| entry.get("value"))
                    .cloned()
                    .unwrap_or(Value::Null);
                projected.insert(column.name.clone(), value);
            }
            for (idx, column) in metric_columns.iter().enumerate() {
                let value = metric_values
                    .get(idx)
                    .and_then(|entry| entry.get("value"))
                    .cloned()
                    .unwrap_or(Value::Null);
                projected.insert(column.name.clone(), value);
            }
            projected
        })
        .collect()
}

fn extract_row_count(response: &Value) -> Option<usize> {
    let value = response.get("rowCount")?;
    if let Some(raw) = value.as_u64() {
        return Some(raw as usize);
    }
    value
        .as_str()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
}

fn project_payload_for_mode(
    projection: &GaTabularProjection,
    output_mode: contract::OutputMode,
    summary_only: bool,
    max_cell_chars: Option<usize>,
) -> (Value, contract::TabularMeta) {
    let (rows, cell_clipping) = if let Some(max_chars) = max_cell_chars {
        let (rows, clipped_cells) = clip_rows(&projection.rows, max_chars);
        let clipping = json!({
            "enabled": true,
            "max_cell_chars": max_chars,
            "clipped_cells": clipped_cells,
            "applied": clipped_cells > 0,
        });
        (rows, Some(clipping))
    } else {
        (projection.rows.clone(), None)
    };

    let row_count_returned = rows.len();
    let mut meta = contract::TabularMeta::rows(
        projection.row_count_total,
        row_count_returned,
        projection.columns.clone(),
    );
    meta.output_mode = output_mode;
    meta.summary_only = summary_only;
    meta.cell_clipping = cell_clipping;
    if projection.row_count_total > row_count_returned {
        meta.query_hints = Some(vec![
            "additional rows are available; continue with meta.next_cursor".to_string(),
        ]);
    }

    let payload = if summary_only {
        Value::Null
    } else {
        payload_by_mode(&rows, &projection.columns, output_mode)
    };
    (payload, meta)
}

fn payload_by_mode(
    rows: &[Map<String, Value>],
    columns: &[contract::ColumnMeta],
    output_mode: contract::OutputMode,
) -> Value {
    match output_mode {
        contract::OutputMode::Rows => {
            Value::Array(rows.iter().cloned().map(Value::Object).collect::<Vec<_>>())
        }
        contract::OutputMode::Tuples => rows_to_tuples(rows, columns),
        contract::OutputMode::Scalar => first_scalar(rows, columns),
        contract::OutputMode::Compact => json!({
            "columns": columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            "tuples": rows_to_tuples(rows, columns),
            "row_count": rows.len(),
        }),
    }
}

fn rows_to_tuples(rows: &[Map<String, Value>], columns: &[contract::ColumnMeta]) -> Value {
    Value::Array(
        rows.iter()
            .map(|row| {
                Value::Array(
                    columns
                        .iter()
                        .map(|column| row.get(&column.name).cloned().unwrap_or(Value::Null))
                        .collect(),
                )
            })
            .collect(),
    )
}

fn first_scalar(rows: &[Map<String, Value>], columns: &[contract::ColumnMeta]) -> Value {
    let Some(first_column) = columns.first() else {
        return Value::Null;
    };
    rows.first()
        .and_then(|row| row.get(&first_column.name))
        .cloned()
        .unwrap_or(Value::Null)
}

fn clip_rows(rows: &[Map<String, Value>], max_chars: usize) -> (Vec<Map<String, Value>>, usize) {
    let mut clipped_cells = 0usize;
    let clipped_rows = rows
        .iter()
        .map(|row| {
            let mut output = Map::new();
            for (key, value) in row {
                let (next, clipped) = clip_value(value, max_chars);
                if clipped {
                    clipped_cells += 1;
                }
                output.insert(key.clone(), next);
            }
            output
        })
        .collect();
    (clipped_rows, clipped_cells)
}

fn clip_value(value: &Value, max_chars: usize) -> (Value, bool) {
    match value {
        Value::String(raw) => {
            if raw.chars().count() <= max_chars {
                return (Value::String(raw.clone()), false);
            }
            let clipped = raw.chars().take(max_chars).collect::<String>();
            (Value::String(format!("{clipped}...")), true)
        }
        other => (other.clone(), false),
    }
}

fn scratchpad_table_info_to_json(table: &ScratchpadTableInfo) -> Value {
    json!({
        "schema": table.schema,
        "name": table.name,
        "table_type": table.table_type,
        "schema_summary": {
            "column_count": table.column_count,
            "columns_returned": table.columns.len(),
            "columns_truncated": table.columns_truncated,
            "columns": table
                .columns
                .iter()
                .map(|column| json!({
                    "name": column.name,
                    "logical_type": column.logical_type,
                    "nullable": column.nullable
                }))
                .collect::<Vec<_>>(),
        }
    })
}

fn collect_runtime_memory_pressure(
    sessions: &crate::scratchpad::ScratchpadSessionManager,
    active_sessions: usize,
    sample_limit: usize,
) -> Value {
    let safe_sample_limit = sample_limit.clamp(1, MAX_MEMORY_PRESSURE_SAMPLE_SESSIONS);
    let configured_limit_bytes_per_session = (sessions.config().max_memory_mb as u64)
        .saturating_mul(1024)
        .saturating_mul(1024);
    let configured_limit_bytes_total = configured_limit_bytes_per_session
        .saturating_mul(u64::try_from(active_sessions).unwrap_or(u64::MAX));

    if active_sessions == 0 {
        return json!({
            "status": "ok",
            "sampled_sessions": 0,
            "sample_limit": safe_sample_limit,
            "probe_errors": 0,
            "configured_limit_bytes_per_session": configured_limit_bytes_per_session,
            "configured_limit_bytes_total": configured_limit_bytes_total,
            "used_bytes_total": 0,
            "limit_bytes_total": 0,
            "used_pct_of_reported_limit": null,
            "pressure_level": "normal",
            "high_pressure": false,
            "sessions": [],
            "note": "no active scratchpad sessions"
        });
    }

    let sampled_sessions_target = active_sessions.min(safe_sample_limit);
    let session_infos = match sessions.list_sessions(sampled_sessions_target) {
        Ok(value) => value,
        Err(err) => {
            return json!({
                "status": "unavailable",
                "sampled_sessions": 0,
                "sample_limit": safe_sample_limit,
                "probe_errors": 1,
                "configured_limit_bytes_per_session": configured_limit_bytes_per_session,
                "configured_limit_bytes_total": configured_limit_bytes_total,
                "used_bytes_total": null,
                "limit_bytes_total": null,
                "used_pct_of_reported_limit": null,
                "pressure_level": "unknown",
                "high_pressure": false,
                "sessions": [],
                "note": format!("failed to enumerate sessions for memory probes (code={})", err.code())
            });
        }
    };

    let mut probe_rows = Vec::with_capacity(session_infos.len());
    let mut probe_errors = 0usize;
    let mut used_bytes_total = 0u64;
    let mut limit_bytes_total = 0u64;
    let mut max_session_used_pct = None::<f64>;

    for info in session_infos {
        match sessions
            .open_connection(&info.session_id)
            .and_then(|conn| probe_duckdb_memory_usage(&conn))
        {
            Ok((used_bytes, limit_bytes)) => {
                used_bytes_total = used_bytes_total.saturating_add(used_bytes);
                limit_bytes_total = limit_bytes_total.saturating_add(limit_bytes);
                let used_pct = percent_from_ratio(used_bytes, limit_bytes);
                if let Some(value) = used_pct {
                    max_session_used_pct =
                        Some(max_session_used_pct.map_or(value, |current| current.max(value)));
                }
                probe_rows.push(json!({
                    "session_id": info.session_id,
                    "status": "ok",
                    "used_bytes": used_bytes,
                    "limit_bytes": limit_bytes,
                    "used_pct": used_pct
                }));
            }
            Err(err) => {
                probe_errors = probe_errors.saturating_add(1);
                probe_rows.push(json!({
                    "session_id": info.session_id,
                    "status": "probe_error",
                    "used_bytes": null,
                    "limit_bytes": null,
                    "used_pct": null,
                    "note": format!("memory probe failed (code={})", err.code()),
                    "error_reason": err.reason()
                }));
            }
        }
    }

    let used_pct_of_reported_limit = percent_from_ratio(used_bytes_total, limit_bytes_total);
    let (pressure_level, high_pressure) = classify_memory_pressure(used_pct_of_reported_limit);
    let status = if probe_errors == 0 {
        "ok"
    } else if probe_errors < probe_rows.len() {
        "partial"
    } else {
        "unavailable"
    };
    let note = if active_sessions > sampled_sessions_target {
        Some(format!(
            "sampled {sampled_sessions_target} sessions out of {active_sessions} active sessions"
        ))
    } else if probe_errors > 0 {
        Some(format!(
            "memory probes failed for {probe_errors} session(s); inspect per-session notes"
        ))
    } else {
        None
    };

    json!({
        "status": status,
        "sampled_sessions": probe_rows.len(),
        "sample_limit": safe_sample_limit,
        "probe_errors": probe_errors,
        "configured_limit_bytes_per_session": configured_limit_bytes_per_session,
        "configured_limit_bytes_total": configured_limit_bytes_total,
        "used_bytes_total": used_bytes_total,
        "limit_bytes_total": limit_bytes_total,
        "used_pct_of_reported_limit": used_pct_of_reported_limit,
        "max_session_used_pct": max_session_used_pct,
        "pressure_level": pressure_level,
        "high_pressure": high_pressure,
        "sessions": probe_rows,
        "note": note
    })
}

fn probe_duckdb_memory_usage(conn: &duckdb::Connection) -> Result<(u64, u64), AnalyticsError> {
    let mut stmt = conn
        .prepare(
            "SELECT CAST(memory_usage AS VARCHAR), CAST(memory_limit AS VARCHAR)
             FROM pragma_database_size()",
        )
        .map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to prepare duckdb memory usage probe: {err}"
            ))
        })?;
    let mut rows = stmt.query([]).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!("failed to execute duckdb memory probe: {err}"))
    })?;

    let Some(row) = rows.next().map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!("failed to read duckdb memory probe row: {err}"))
    })?
    else {
        return Err(AnalyticsError::ScratchpadEngine(
            "duckdb memory probe returned no rows".to_string(),
        ));
    };

    let used_raw = row.get::<_, String>(0).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!("failed to decode duckdb memory usage: {err}"))
    })?;
    let limit_raw = row.get::<_, String>(1).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!("failed to decode duckdb memory limit: {err}"))
    })?;
    let used_bytes = parse_duckdb_size_bytes(&used_raw).ok_or_else(|| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to parse duckdb memory usage value '{used_raw}'"
        ))
    })?;
    let limit_bytes = parse_duckdb_size_bytes(&limit_raw).ok_or_else(|| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to parse duckdb memory limit value '{limit_raw}'"
        ))
    })?;

    Ok((used_bytes, limit_bytes))
}

fn parse_duckdb_size_bytes(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = trimmed.parse::<u64>() {
        return Some(value);
    }

    let normalized = trimmed.to_ascii_lowercase();
    let normalized = normalized.strip_suffix("bytes").unwrap_or(&normalized);
    let normalized = normalized.strip_suffix("byte").unwrap_or(normalized);
    let normalized = normalized.trim();
    let mut parts = normalized.split_whitespace();
    let number = parts.next()?.parse::<f64>().ok()?;
    let unit = parts.next().unwrap_or("b");
    let multiplier = match unit {
        "b" => 1f64,
        "kb" | "kib" => 1024f64,
        "mb" | "mib" => 1024f64 * 1024f64,
        "gb" | "gib" => 1024f64 * 1024f64 * 1024f64,
        "tb" | "tib" => 1024f64 * 1024f64 * 1024f64 * 1024f64,
        _ => return None,
    };
    let bytes = number * multiplier;
    if !bytes.is_finite() || bytes < 0.0 {
        return None;
    }
    Some(bytes.round() as u64)
}

fn percent_from_ratio(numerator: u64, denominator: u64) -> Option<f64> {
    if denominator == 0 {
        return None;
    }
    Some(((numerator as f64) * 100.0) / (denominator as f64))
}

fn classify_memory_pressure(used_pct: Option<f64>) -> (&'static str, bool) {
    let Some(value) = used_pct else {
        return ("unknown", false);
    };
    if value >= MEMORY_PRESSURE_CRITICAL_PCT {
        return ("critical", true);
    }
    if value >= MEMORY_PRESSURE_HIGH_PCT {
        return ("high", true);
    }
    if value >= MEMORY_PRESSURE_MODERATE_PCT {
        return ("moderate", false);
    }
    ("normal", false)
}

fn contract_success(data: Value, started: Instant) -> CallToolResult {
    contract::success(data, contract::elapsed_ms(started))
}

fn contract_error(err: AnalyticsError, started: Instant) -> CallToolResult {
    contract::error(err, contract::elapsed_ms(started))
}

fn redact_tool_error_message(err: &impl std::fmt::Display) -> String {
    contract::redact_secret_text(&err.to_string())
}

fn auth_env_presence() -> Value {
    json!({
        "GOOGLE_APPLICATION_CREDENTIALS": std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").is_some(),
        "GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON": std::env::var_os("GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON").is_some(),
        "GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN": std::env::var_os("GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN").is_some(),
        "GOOGLE_ANALYTICS_MCP_QUOTA_PROJECT": std::env::var_os("GOOGLE_ANALYTICS_MCP_QUOTA_PROJECT").is_some(),
        "CLOUDSDK_CONFIG": std::env::var_os("CLOUDSDK_CONFIG").is_some(),
    })
}

fn credential_material_detected_for_auth_source(
    auth_source: AuthSource,
    local_detected: bool,
) -> bool {
    local_detected || !matches!(auth_source, AuthSource::GoogleDefaultProviderChain)
}

fn login_scope_for_mcp_command(current_scope: &str) -> &str {
    if scope_allows_analytics_read(current_scope) {
        current_scope
    } else {
        DEFAULT_ANALYTICS_SCOPE
    }
}

fn after_login_instruction(
    upstream_token_source: UpstreamTokenSource,
    current_scope: &str,
    login_scope: &str,
) -> String {
    let ambient_scope = std::env::var("GOOGLE_ANALYTICS_MCP_SCOPE").ok();
    after_login_instruction_with_env(
        upstream_token_source,
        current_scope,
        login_scope,
        ambient_scope.as_deref(),
    )
}

fn after_login_instruction_with_env(
    upstream_token_source: UpstreamTokenSource,
    current_scope: &str,
    login_scope: &str,
    ambient_scope: Option<&str>,
) -> String {
    if current_scope != login_scope
        || ambient_scope
            .filter(|scope| !scope.is_empty())
            .is_some_and(|scope| scope != login_scope)
    {
        format!(
            "Unset GOOGLE_ANALYTICS_MCP_SCOPE, set GOOGLE_ANALYTICS_MCP_SCOPE={login_scope}, or update any MCP launcher `--analytics-scope` argument before restarting stdio MCP clients; stale scope configuration overrides the login scope."
        )
    } else if upstream_token_source == UpstreamTokenSource::RequestHeader {
        "For local ADC fallback, set GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header_or_config and GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=authorization before restarting; keep request_header for hosted per-user services.".to_string()
    } else {
        "Restart stdio or HTTP MCP clients that keep long-lived server processes, then call ga4_auth_status with verify_token=true or run ga4-mcp auth status --verify-token.".to_string()
    }
}

fn auth_next_steps(
    upstream_token_source: UpstreamTokenSource,
    scope: &str,
    verified: bool,
    token_ok: Option<bool>,
) -> Vec<String> {
    let setup_plan = google_provider_auth_config(scope).adc_setup_plan();
    let missing_analytics_scope = !scope_allows_analytics_read(scope);
    let read_scope_step = format!(
        "Set GOOGLE_ANALYTICS_MCP_SCOPE={DEFAULT_ANALYTICS_SCOPE} or start the MCP server with `--analytics-scope {DEFAULT_ANALYTICS_SCOPE}`."
    );
    let login_command = if missing_analytics_scope {
        format!("ga4-mcp --analytics-scope {DEFAULT_ANALYTICS_SCOPE} auth login")
    } else {
        "ga4-mcp auth login".to_string()
    };
    match (verified, token_ok) {
        (false, _) => {
            let mut steps = Vec::new();
            if missing_analytics_scope {
                steps.push(read_scope_step);
            }
            steps.push("Run ga4-mcp auth status --verify-token, or call ga4_auth_status with verify_token=true, when you are ready to prove credentials.".to_string());
            steps.push(format!(
                "If credentials are missing, run {login_command} or call ga4_auth_login_command for a copyable login command."
            ));
            if upstream_token_source == UpstreamTokenSource::RequestHeader {
                steps.push("For local ADC fallback, switch GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE to request_header_or_config; keep request_header for hosted per-user services.".to_string());
            }
            steps.push("Call get_account_summaries after auth is verified to discover accessible GA4 account/property ids.".to_string());
            steps
        }
        (true, Some(true)) => {
            let mut steps = Vec::new();
            if missing_analytics_scope {
                steps.push(read_scope_step);
            }
            if upstream_token_source == UpstreamTokenSource::RequestHeader {
                steps.push("Runtime is in request_header mode; this is correct for hosted per-user OAuth. For local ADC fallback, use request_header_or_config.".to_string());
            }
            steps.push(
                "Restart MCP clients that keep long-lived stdio or HTTP server processes."
                    .to_string(),
            );
            steps.push(
                "Call get_account_summaries to discover accessible GA4 accounts and properties."
                    .to_string(),
            );
            steps
        }
        (true, Some(false)) | (true, None) => {
            let mut steps = vec![
                format!("Run {login_command} for local browser login."),
                format!(
                    "If the token check reports that local ADC requires a quota project, run `{}`.",
                    setup_plan.quota_project.shell
                ),
                "Call ga4_auth_login_command if you need a copyable login command inside MCP."
                    .to_string(),
                "For unattended deployments, set GOOGLE_APPLICATION_CREDENTIALS or OAuth refresh-token env configuration.".to_string(),
                "Ensure the authenticated Google principal has access to the GA4 account/property."
                    .to_string(),
            ];
            if let Some(api_enable) = setup_plan.api_enable.as_ref() {
                steps.insert(
                    1,
                    format!(
                        "Enable the required Analytics APIs with `{}`.",
                        api_enable.shell
                    ),
                );
            }
            if missing_analytics_scope {
                steps.insert(0, read_scope_step);
            }
            if upstream_token_source == UpstreamTokenSource::RequestHeader {
                steps.push("For local ADC fallback, switch GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE to request_header_or_config; keep request_header for hosted per-user services.".to_string());
            }
            steps
        }
    }
}

fn scope_allows_analytics_read(scope: &str) -> bool {
    scope.split([',', ' ', '\n', '\t']).any(|item| {
        item == DEFAULT_ANALYTICS_SCOPE || item == "https://www.googleapis.com/auth/analytics"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct OutputModeTestArgs {
        #[serde(default, deserialize_with = "deserialize_optional_output_mode")]
        output_mode: Option<TabularOutputMode>,
    }

    #[test]
    fn run_report_defaults_return_property_quota_to_true() {
        let args: RunReportArgs = serde_json::from_value(json!({
            "property_id": "properties/123456789",
            "date_ranges": [{"start_date":"2026-02-01","end_date":"2026-02-07"}],
            "dimensions": ["country"],
            "metrics": ["activeUsers"]
        }))
        .expect("run_report args should deserialize");
        assert!(args.return_property_quota);
    }

    #[test]
    fn run_report_respects_explicit_return_property_quota_false() {
        let args: RunReportArgs = serde_json::from_value(json!({
            "property_id": "properties/123456789",
            "date_ranges": [{"start_date":"2026-02-01","end_date":"2026-02-07"}],
            "dimensions": ["country"],
            "metrics": ["activeUsers"],
            "return_property_quota": false
        }))
        .expect("run_report args should deserialize");
        assert!(!args.return_property_quota);
    }

    #[test]
    fn alpha_report_args_keep_read_only_safe_defaults() {
        let conversions: RunConversionsReportArgs = serde_json::from_value(json!({
            "property_id": "properties/123456789",
            "date_ranges": [{"start_date":"2026-04-01","end_date":"2026-04-30"}],
            "dimensions": ["campaignName"],
            "metrics": ["allConversionsByConversionDate"],
            "conversion_spec": {}
        }))
        .expect("conversion args should deserialize");
        assert!(conversions.conversion_spec.conversion_actions.is_empty());
        assert!(conversions.conversion_spec.attribution_model.is_none());
        assert!(!conversions.return_property_quota);

        let funnel: RunFunnelReportArgs = serde_json::from_value(json!({
            "property_id": "properties/123456789",
            "funnel_steps": [{"event":"page_view"}]
        }))
        .expect("funnel args should deserialize");
        assert!(!funnel.is_open_funnel);
        assert!(funnel.date_ranges.is_empty());
        assert!(!funnel.return_property_quota);
    }

    #[test]
    fn alpha_report_args_reject_unknown_enum_values() {
        let conversion_err = serde_json::from_value::<RunConversionsReportArgs>(json!({
            "property_id": "properties/123456789",
            "date_ranges": [{"start_date":"2026-04-01","end_date":"2026-04-30"}],
            "dimensions": ["campaignName"],
            "metrics": ["allConversionsByConversionDate"],
            "conversion_spec": {"attribution_model":"LINEAR"}
        }))
        .expect_err("unsupported attribution model must fail deserialization");
        assert!(conversion_err.to_string().contains("DATA_DRIVEN"));

        let funnel_err = serde_json::from_value::<RunFunnelReportArgs>(json!({
            "property_id": "properties/123456789",
            "funnel_steps": [{"event":"page_view"}],
            "funnel_visualization_type": "FREE_FORM"
        }))
        .expect_err("unsupported funnel visualization type must fail deserialization");
        assert!(funnel_err.to_string().contains("STANDARD_FUNNEL"));
    }

    #[test]
    fn run_realtime_defaults_return_property_quota_to_true() {
        let args: RunRealtimeReportArgs = serde_json::from_value(json!({
            "property_id": "properties/123456789",
            "dimensions": ["country"],
            "metrics": ["activeUsers"]
        }))
        .expect("run_realtime args should deserialize");
        assert!(args.return_property_quota);
    }

    #[test]
    fn run_pivot_defaults_return_property_quota_to_true() {
        let args: RunPivotReportArgs = serde_json::from_value(json!({
            "property_id": "properties/123456789",
            "date_ranges": [{"start_date":"2026-02-01","end_date":"2026-02-07"}],
            "dimensions": ["country"],
            "metrics": ["activeUsers"],
            "pivots": [{"field_names":["country"],"limit":10}]
        }))
        .expect("run_pivot args should deserialize");
        assert!(args.return_property_quota);
    }

    #[test]
    fn batch_report_item_defaults_return_property_quota_to_true() {
        let args: BatchRunReportItemArgs = serde_json::from_value(json!({
            "date_ranges": [{"start_date":"2026-02-01","end_date":"2026-02-07"}],
            "dimensions": ["country"],
            "metrics": ["activeUsers"]
        }))
        .expect("batch report item args should deserialize");
        assert!(args.return_property_quota);
    }

    #[test]
    fn build_run_report_preview_payload_is_deterministic() {
        let report: RunReportArgs = serde_json::from_value(json!({
            "property_id": "properties/123456789",
            "date_ranges": [{"start_date":"2026-02-01","end_date":"2026-02-07"}],
            "dimensions": ["date", "country"],
            "metrics": ["activeUsers"]
        }))
        .expect("run_report args should deserialize");
        let query_hash = run_report_query_hash(&report).expect("query hash should resolve");
        let (effective_offset, effective_limit) = resolve_cursor_window(
            &query_hash,
            None,
            report.offset,
            report.max_rows,
            report.limit,
        )
        .expect("window should resolve");

        let first = build_run_report_preview_payload(
            &report,
            &query_hash,
            effective_offset,
            effective_limit,
        )
        .expect("preview payload should build");
        let second = build_run_report_preview_payload(
            &report,
            &query_hash,
            effective_offset,
            effective_limit,
        )
        .expect("preview payload should build");

        assert_eq!(first, second);
        assert_eq!(first["preview"]["tool"], json!("run_report"));
        assert_eq!(first["preview"]["query_hash"], json!(query_hash));
        assert_eq!(
            first["preview"]["request"]["property"],
            json!("properties/123456789")
        );
        assert_eq!(
            first["preview"]["request"]["return_property_quota"],
            json!(true)
        );
        assert_eq!(
            first["projection"]["ingest_columns"]
                .as_array()
                .expect("ingest columns should be an array")
                .iter()
                .any(|column| column["target_name"] == json!("date_parsed")
                    && column["transform"] == json!("parse_ga_date")),
            true
        );
    }

    #[test]
    fn preview_report_request_args_deserialize_with_defaults() {
        let args: PreviewReportRequestArgs = serde_json::from_value(json!({
            "property_id": "properties/123456789",
            "date_ranges": [{"start_date":"2026-02-01","end_date":"2026-02-07"}],
            "dimensions": ["country"],
            "metrics": ["activeUsers"]
        }))
        .expect("preview report args should deserialize");
        assert!(args.report.return_property_quota);
    }

    #[test]
    fn preview_report_request_accepts_dimension_filter_shorthand() {
        let args: PreviewReportRequestArgs = serde_json::from_value(json!({
            "property_id": "properties/123456789",
            "date_ranges": [{"start_date":"2026-02-01","end_date":"2026-02-07"}],
            "dimensions": ["sessionDefaultChannelGroup"],
            "metrics": ["sessions"],
            "dimension_filter": "sessionDefaultChannelGroup==\"Paid Other\""
        }))
        .expect("preview report args should deserialize");
        let query_hash = run_report_query_hash(&args.report).expect("query hash should resolve");
        let (effective_offset, effective_limit) = resolve_cursor_window(
            &query_hash,
            args.report.cursor.as_deref(),
            args.report.offset,
            args.report.max_rows,
            args.report.limit,
        )
        .expect("window should resolve");
        let payload = build_run_report_preview_payload(
            &args.report,
            &query_hash,
            effective_offset,
            effective_limit,
        )
        .expect("preview payload should build");

        assert_eq!(
            payload["preview"]["request"]["dimension_filter"]["filter"]["field_name"],
            json!("sessionDefaultChannelGroup")
        );
        assert_eq!(
            payload["preview"]["request"]["dimension_filter"]["filter"]["string_filter"]["value"],
            json!("Paid Other")
        );
    }

    #[test]
    fn preview_report_request_accepts_dimension_filter_json_expression() {
        let args: PreviewReportRequestArgs = serde_json::from_value(json!({
            "property_id": "properties/123456789",
            "date_ranges": [{"start_date":"2026-02-01","end_date":"2026-02-07"}],
            "dimensions": ["sessionSource", "country"],
            "metrics": ["sessions"],
            "dimension_filter": {
                "and_group": {
                    "expressions": [
                        {
                            "filter": {
                                "field_name": "sessionSource",
                                "string_filter": {
                                    "match_type": "EXACT",
                                    "value": "google"
                                }
                            }
                        }
                    ]
                }
            }
        }))
        .expect("preview report args should deserialize");
        let query_hash = run_report_query_hash(&args.report).expect("query hash should resolve");
        let (effective_offset, effective_limit) = resolve_cursor_window(
            &query_hash,
            args.report.cursor.as_deref(),
            args.report.offset,
            args.report.max_rows,
            args.report.limit,
        )
        .expect("window should resolve");
        let payload = build_run_report_preview_payload(
            &args.report,
            &query_hash,
            effective_offset,
            effective_limit,
        )
        .expect("preview payload should build");

        assert_eq!(
            payload["preview"]["request"]["dimension_filter"]["and_group"]["expressions"][0]["filter"]
                ["field_name"],
            json!("sessionSource")
        );
        assert_eq!(
            payload["preview"]["request"]["dimension_filter"]["and_group"]["expressions"][0]["filter"]
                ["string_filter"]["value"],
            json!("google")
        );
    }

    #[test]
    fn preview_report_request_rejects_invalid_dimension_filter_shorthand() {
        let err = serde_json::from_value::<PreviewReportRequestArgs>(json!({
            "property_id": "properties/123456789",
            "date_ranges": [{"start_date":"2026-02-01","end_date":"2026-02-07"}],
            "dimensions": ["sessionDefaultChannelGroup"],
            "metrics": ["sessions"],
            "dimension_filter": "sessionDefaultChannelGroup==Paid Other"
        }))
        .expect_err("unquoted shorthand values should be rejected");
        assert!(err.to_string().contains("should be quoted"));
    }

    #[test]
    fn scratchpad_table_info_to_json_includes_schema_summary() {
        let table = ScratchpadTableInfo {
            schema: "main".to_string(),
            name: "events".to_string(),
            table_type: "BASE TABLE".to_string(),
            column_count: 3,
            columns: vec![
                crate::scratchpad::ScratchpadTableColumnInfo {
                    name: "event_name".to_string(),
                    logical_type: "string".to_string(),
                    nullable: true,
                },
                crate::scratchpad::ScratchpadTableColumnInfo {
                    name: "event_count".to_string(),
                    logical_type: "integer".to_string(),
                    nullable: false,
                },
            ],
            columns_truncated: true,
        };

        let payload = scratchpad_table_info_to_json(&table);
        assert_eq!(payload["schema"], json!("main"));
        assert_eq!(payload["name"], json!("events"));
        assert_eq!(payload["table_type"], json!("BASE TABLE"));
        assert_eq!(payload["schema_summary"]["column_count"], json!(3));
        assert_eq!(payload["schema_summary"]["columns_returned"], json!(2));
        assert_eq!(payload["schema_summary"]["columns_truncated"], json!(true));
        assert_eq!(
            payload["schema_summary"]["columns"][0]["name"],
            json!("event_name")
        );
        assert_eq!(
            payload["schema_summary"]["columns"][1]["logical_type"],
            json!("integer")
        );
        assert_eq!(
            payload["schema_summary"]["columns"][1]["nullable"],
            json!(false)
        );
    }

    #[test]
    fn scratchpad_drop_table_args_default_if_exists_false() {
        let args: ScratchpadDropTableArgs = serde_json::from_value(json!({
            "session_id": "session_a",
            "table_name": "events"
        }))
        .expect("drop args should deserialize");
        assert!(!args.if_exists);
    }

    #[test]
    fn parse_duckdb_size_bytes_supports_common_units() {
        assert_eq!(parse_duckdb_size_bytes("0 bytes"), Some(0));
        assert_eq!(parse_duckdb_size_bytes("1 KiB"), Some(1024));
        assert_eq!(parse_duckdb_size_bytes("2 MiB"), Some(2 * 1024 * 1024));
        assert_eq!(parse_duckdb_size_bytes("1.5 GiB"), Some(1610612736));
        assert_eq!(parse_duckdb_size_bytes("not-a-size"), None);
    }

    #[test]
    fn classify_memory_pressure_applies_thresholds() {
        assert_eq!(classify_memory_pressure(None), ("unknown", false));
        assert_eq!(classify_memory_pressure(Some(10.0)), ("normal", false));
        assert_eq!(classify_memory_pressure(Some(70.0)), ("moderate", false));
        assert_eq!(classify_memory_pressure(Some(85.0)), ("high", true));
        assert_eq!(classify_memory_pressure(Some(97.0)), ("critical", true));
    }

    #[test]
    fn collect_runtime_memory_pressure_handles_zero_sessions() {
        let engine: crate::scratchpad::SharedScratchpadEngine =
            std::sync::Arc::new(crate::scratchpad::DuckDbEngine::new().expect("engine"));
        let root_dir = std::env::temp_dir().join("ga4-mcp-tools-memory-zero-sessions");
        let manager = crate::scratchpad::ScratchpadSessionManager::new(
            engine,
            crate::scratchpad::ScratchpadSessionConfig::new(
                std::time::Duration::from_secs(60),
                4,
                4,
                100,
                128,
            )
            .with_root_dir(root_dir.clone()),
        )
        .expect("manager");

        let payload = collect_runtime_memory_pressure(&manager, 0, 16);
        assert_eq!(payload["status"], json!("ok"));
        assert_eq!(payload["sampled_sessions"], json!(0));
        assert_eq!(payload["probe_errors"], json!(0));
        assert_eq!(payload["pressure_level"], json!("normal"));
        assert_eq!(payload["high_pressure"], json!(false));

        let _ = std::fs::remove_dir_all(root_dir);
    }

    use serde_json::json;

    #[test]
    fn validate_report_requires_non_empty_dimensions() {
        let err =
            validate_report_inputs(&[], &["activeUsers".to_string()], &[json!({})], None, None)
                .expect_err("empty dimensions must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
    }

    #[test]
    fn validate_report_rejects_empty_date_ranges() {
        let err = validate_report_inputs(
            &["country".to_string()],
            &["activeUsers".to_string()],
            &[],
            None,
            None,
        )
        .expect_err("empty date ranges must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
    }

    #[test]
    fn validate_report_rejects_non_object_date_range_entries() {
        let err = validate_report_inputs(
            &["country".to_string()],
            &["activeUsers".to_string()],
            &[json!("2026-01-01/2026-01-31")],
            None,
            None,
        )
        .expect_err("date range entries must be objects");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(err.to_string().contains("date_ranges[0] must be an object"));
    }

    #[test]
    fn validate_report_rejects_date_range_entries_missing_end_date() {
        let err = validate_report_inputs(
            &["country".to_string()],
            &["activeUsers".to_string()],
            &[json!({"start_date": "2026-01-01"})],
            None,
            None,
        )
        .expect_err("date range entries must include start and end fields");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(
            err.to_string().contains(
                "must include non-empty start_date/startDate and end_date/endDate strings"
            )
        );
    }

    #[test]
    fn validate_realtime_accepts_valid_input() {
        let result = validate_realtime_inputs(
            &["country".to_string()],
            &["activeUsers".to_string()],
            Some(100),
            Some(0),
        );
        assert!(result.is_ok());
    }

    fn valid_conversions_report_args() -> RunConversionsReportArgs {
        RunConversionsReportArgs {
            property_id: PropertyId::Number(1234),
            date_ranges: vec![json!({
                "start_date": "2026-04-01",
                "end_date": "2026-04-30",
            })],
            dimensions: vec!["campaignName".to_string()],
            metrics: vec!["allConversionsByConversionDate".to_string()],
            conversion_spec: ConversionSpecArgs {
                conversion_actions: vec!["conversionActions/1234".to_string()],
                attribution_model: Some(AttributionModel::DataDriven),
            },
            dimension_filter: None,
            metric_filter: None,
            order_bys: None,
            limit: Some(100),
            offset: Some(0),
            currency_code: None,
            return_property_quota: false,
            max_rows: Some(50),
            cursor: None,
            output_mode: None,
            summary_only: false,
            max_cell_chars: None,
        }
    }

    #[test]
    fn validate_conversions_report_accepts_documented_fields() {
        assert!(validate_conversions_report_inputs(&valid_conversions_report_args()).is_ok());
    }

    #[test]
    fn validate_conversions_report_rejects_unsupported_dimension() {
        let mut args = valid_conversions_report_args();
        args.dimensions = vec!["city".to_string()];
        let err = validate_conversions_report_inputs(&args)
            .expect_err("unsupported conversion dimension must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(err.to_string().contains("campaignName"));
    }

    #[test]
    fn validate_conversions_report_rejects_malformed_action_resource() {
        let mut args = valid_conversions_report_args();
        args.conversion_spec.conversion_actions = vec!["purchase".to_string()];
        let err = validate_conversions_report_inputs(&args)
            .expect_err("malformed conversion action must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(err.to_string().contains("conversionActions/<numeric-id>"));
    }

    #[test]
    fn conversions_query_hash_matches_outbound_json_normalization_and_trimmed_currency() {
        let mut snake_case = valid_conversions_report_args();
        snake_case.dimensions = vec![" campaignName ".to_string()];
        snake_case.metrics = vec![" totalRevenueByConversionDate ".to_string()];
        snake_case.dimension_filter = Some(json!({
            "and_group": {
                "expressions": [{
                    "filter": { "field_name": "campaignName" }
                }]
            }
        }));
        snake_case.order_bys = Some(vec![json!({
            "dimension": { "dimension_name": "campaignName" },
            "desc": true
        })]);
        snake_case.currency_code = Some(" USD ".to_string());

        let mut camel_case = valid_conversions_report_args();
        camel_case.dimensions = vec!["campaignName".to_string()];
        camel_case.metrics = vec!["totalRevenueByConversionDate".to_string()];
        camel_case.date_ranges = vec![json!({
            "startDate": "2026-04-01",
            "endDate": "2026-04-30",
        })];
        camel_case.dimension_filter = Some(json!({
            "andGroup": {
                "expressions": [{
                    "filter": { "fieldName": "campaignName" }
                }]
            }
        }));
        camel_case.order_bys = Some(vec![json!({
            "dimension": { "dimensionName": "campaignName" },
            "desc": true
        })]);
        camel_case.currency_code = Some("USD".to_string());

        assert_eq!(
            run_conversions_report_query_hash(&snake_case).expect("snake hash"),
            run_conversions_report_query_hash(&camel_case).expect("camel hash")
        );
    }

    fn valid_funnel_report_args() -> RunFunnelReportArgs {
        RunFunnelReportArgs {
            property_id: PropertyId::Number(1234),
            funnel_steps: vec![
                FunnelStepArgs {
                    name: Some("Read".to_string()),
                    event: Some("page_view".to_string()),
                    filter_expression: None,
                    is_directly_followed_by: None,
                    within_duration_from_prior_step: None,
                },
                FunnelStepArgs {
                    name: Some("Subscribe".to_string()),
                    event: Some("sign_up".to_string()),
                    filter_expression: None,
                    is_directly_followed_by: Some(false),
                    within_duration_from_prior_step: Some("3600s".to_string()),
                },
            ],
            is_open_funnel: false,
            date_ranges: vec![json!({
                "start_date": "7daysAgo",
                "end_date": "yesterday",
            })],
            funnel_breakdown: Some(FunnelBreakdownArgs {
                breakdown_dimension: "deviceCategory".to_string(),
                limit: Some(5),
            }),
            funnel_next_action: Some(FunnelNextActionArgs {
                next_action_dimension: "eventName".to_string(),
                limit: Some(5),
            }),
            funnel_visualization_type: Some(FunnelVisualizationType::StandardFunnel),
            segments: None,
            dimension_filter: None,
            limit: Some(100),
            return_property_quota: false,
            max_rows: Some(50),
            output_mode: None,
            summary_only: false,
            max_cell_chars: None,
        }
    }

    #[test]
    fn validate_funnel_report_accepts_event_shorthand() {
        assert!(validate_funnel_report_inputs(&valid_funnel_report_args()).is_ok());
    }

    #[test]
    fn funnel_query_hash_matches_outbound_json_normalization() {
        let mut snake_case = valid_funnel_report_args();
        snake_case.segments = Some(vec![json!({
            "segment_filter": {
                "and_group": {
                    "expressions": [{
                        "filter": { "field_name": "country" }
                    }]
                }
            }
        })]);
        snake_case.dimension_filter = Some(json!({
            "filter": { "field_name": "deviceCategory" }
        }));

        let mut camel_case = valid_funnel_report_args();
        camel_case.date_ranges = vec![json!({
            "startDate": "7daysAgo",
            "endDate": "yesterday",
        })];
        camel_case.segments = Some(vec![json!({
            "segmentFilter": {
                "andGroup": {
                    "expressions": [{
                        "filter": { "fieldName": "country" }
                    }]
                }
            }
        })]);
        camel_case.dimension_filter = Some(json!({
            "filter": { "fieldName": "deviceCategory" }
        }));

        assert_eq!(
            run_funnel_report_query_hash(&snake_case).expect("snake hash"),
            run_funnel_report_query_hash(&camel_case).expect("camel hash")
        );
    }

    #[test]
    fn funnel_query_hash_matches_outbound_step_expansion_and_default_name() {
        let mut shorthand = valid_funnel_report_args();
        shorthand.funnel_steps[0].name = None;

        let mut expanded = valid_funnel_report_args();
        expanded.funnel_steps[0].name = Some("Step 1".to_string());
        expanded.funnel_steps[0].event = None;
        expanded.funnel_steps[0].filter_expression = Some(json!({
            "funnelEventFilter": { "eventName": "page_view" }
        }));

        assert_eq!(
            run_funnel_report_query_hash(&shorthand).expect("shorthand hash"),
            run_funnel_report_query_hash(&expanded).expect("expanded hash")
        );
    }

    #[test]
    fn validate_funnel_report_rejects_conflicting_step_selectors() {
        let mut args = valid_funnel_report_args();
        args.funnel_steps[0].filter_expression = Some(json!({
            "funnel_event_filter": { "event_name": "page_view" }
        }));
        let err = validate_funnel_report_inputs(&args)
            .expect_err("event and filter_expression together must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(err.to_string().contains("exactly one"));
    }

    #[test]
    fn validate_funnel_report_rejects_direct_follow_on_first_step() {
        let mut args = valid_funnel_report_args();
        args.funnel_steps[0].is_directly_followed_by = Some(true);
        let err = validate_funnel_report_inputs(&args)
            .expect_err("the first funnel step has no prior step to follow directly");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(err.to_string().contains("no prior step"));
    }

    #[test]
    fn validate_funnel_report_rejects_malformed_step_duration() {
        let mut args = valid_funnel_report_args();
        args.funnel_steps[1].within_duration_from_prior_step = Some("one hour".to_string());
        let err = validate_funnel_report_inputs(&args)
            .expect_err("step duration must use protobuf duration syntax");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(err.to_string().contains("protobuf duration"));
    }

    #[test]
    fn validate_funnel_report_rejects_more_than_four_segments() {
        let mut args = valid_funnel_report_args();
        args.segments = Some(vec![json!({}); MAX_FUNNEL_SEGMENTS + 1]);
        let err =
            validate_funnel_report_inputs(&args).expect_err("too many funnel segments must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(err.to_string().contains("at most 4"));
    }

    #[test]
    fn validate_funnel_report_rejects_invalid_nested_limits() {
        let mut args = valid_funnel_report_args();
        args.funnel_breakdown.as_mut().expect("breakdown").limit = Some(16);
        let err = validate_funnel_report_inputs(&args)
            .expect_err("breakdown limit above Google maximum must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(err.to_string().contains("between 1 and 15"));
    }

    #[test]
    fn validate_funnel_report_rejects_edge_whitespace_but_preserves_internal_spaces() {
        let mut event_args = valid_funnel_report_args();
        event_args.funnel_steps[0].event = Some(" page view".to_string());
        let err = validate_funnel_report_inputs(&event_args)
            .expect_err("event shorthand with leading whitespace must fail");
        assert!(
            err.to_string()
                .contains("event must not have leading or trailing whitespace")
        );

        let mut breakdown_args = valid_funnel_report_args();
        breakdown_args
            .funnel_breakdown
            .as_mut()
            .expect("breakdown")
            .breakdown_dimension = " device category".to_string();
        let err = validate_funnel_report_inputs(&breakdown_args)
            .expect_err("breakdown dimension with leading whitespace must fail");
        assert!(
            err.to_string()
                .contains("breakdown_dimension must not have leading or trailing whitespace")
        );

        let mut next_action_args = valid_funnel_report_args();
        next_action_args
            .funnel_next_action
            .as_mut()
            .expect("next action")
            .next_action_dimension = "eventName ".to_string();
        let err = validate_funnel_report_inputs(&next_action_args)
            .expect_err("next-action dimension with trailing whitespace must fail");
        assert!(
            err.to_string()
                .contains("next_action_dimension must not have leading or trailing whitespace")
        );

        let mut internal_space_args = valid_funnel_report_args();
        internal_space_args.funnel_steps[0].event = Some("page view".to_string());
        internal_space_args
            .funnel_breakdown
            .as_mut()
            .expect("breakdown")
            .breakdown_dimension = "device category".to_string();
        internal_space_args
            .funnel_next_action
            .as_mut()
            .expect("next action")
            .next_action_dimension = "event name".to_string();
        assert!(validate_funnel_report_inputs(&internal_space_args).is_ok());
    }

    #[test]
    fn resolve_funnel_report_limit_applies_response_cap() {
        assert_eq!(resolve_funnel_report_limit(None, None), 200);
        assert_eq!(resolve_funnel_report_limit(Some(50), Some(100)), 50);
        assert_eq!(resolve_funnel_report_limit(Some(500), Some(25)), 25);
    }

    #[test]
    fn funnel_contract_projects_both_subreports_without_total_overclaim() {
        let response = json!({
            "funnelTable": {
                "dimensionHeaders": [{ "name": "funnelStepName" }],
                "metricHeaders": [{ "name": "activeUsers", "type": "TYPE_INTEGER" }],
                "rows": [
                    {
                        "dimensionValues": [{ "value": "1. Read" }],
                        "metricValues": [{ "value": "20" }]
                    },
                    {
                        "dimensionValues": [{ "value": "2. Subscribe" }],
                        "metricValues": [{ "value": "5" }]
                    }
                ],
                "metadata": { "samplingMetadatas": [] }
            },
            "funnelVisualization": {
                "dimensionHeaders": [{ "name": "funnelStepName" }],
                "metricHeaders": [{ "name": "activeUsers", "type": "TYPE_INTEGER" }],
                "rows": [{
                    "dimensionValues": [{ "value": "1. Read" }],
                    "metricValues": [{ "value": "20" }]
                }],
                "metadata": { "samplingMetadatas": [] }
            },
            "propertyQuota": { "tokensPerDay": { "remaining": 999 } },
            "kind": "analyticsData#runFunnelReport"
        });
        let result = contract_success_ga_funnel(
            response,
            Instant::now(),
            FunnelResponseOptions {
                query_hash: "abcd".to_string(),
                output_mode: contract::OutputMode::Rows,
                summary_only: false,
                max_cell_chars: None,
                effective_limit: 2,
                requested_limit: Some(2),
            },
        );
        let payload = result
            .structured_content
            .expect("funnel response must be structured");

        assert_eq!(payload["ok"], json!(true));
        assert_eq!(
            payload["data"]["funnel_table"][0]["funnelStepName"],
            json!("1. Read")
        );
        assert_eq!(
            payload["meta"]["subreports"]["funnel_table"]["row_count_returned"],
            json!(2)
        );
        assert_eq!(
            payload["meta"]["subreports"]["funnel_table"]["row_count_total_known"],
            json!(false)
        );
        assert_eq!(
            payload["meta"]["subreports"]["funnel_table"]["truncated"],
            json!(true)
        );
        assert_eq!(
            payload["meta"]["subreports"]["funnel_visualization"]["truncated"],
            json!(false)
        );
        assert!(
            payload["meta"]["subreports"]["funnel_table"]
                .get("row_count_total")
                .is_none()
        );
    }

    #[test]
    fn funnel_contract_rejects_missing_subreports() {
        let result = contract_success_ga_funnel(
            json!({
                "funnelTable": {
                    "dimensionHeaders": [],
                    "metricHeaders": [],
                    "rows": []
                }
            }),
            Instant::now(),
            FunnelResponseOptions {
                query_hash: "abcd".to_string(),
                output_mode: contract::OutputMode::Rows,
                summary_only: false,
                max_cell_chars: None,
                effective_limit: 10,
                requested_limit: None,
            },
        );
        let payload = result
            .structured_content
            .expect("funnel error must be structured");
        assert_eq!(payload["ok"], json!(false));
        assert!(
            payload["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("funnelVisualization"))
        );
    }

    #[test]
    fn funnel_contract_rejects_malformed_subreport_tabular_shapes() {
        let valid_subreport = || {
            json!({
                "dimensionHeaders": [],
                "metricHeaders": [],
                "rows": []
            })
        };
        let malformed_subreports = vec![
            json!({
                "dimensionHeaders": {},
                "metricHeaders": [],
                "rows": []
            }),
            json!({
                "dimensionHeaders": [],
                "metricHeaders": [],
                "rows": [null]
            }),
            json!({
                "dimensionHeaders": [],
                "metricHeaders": [],
                "rows": [{"metricValues": {}}]
            }),
        ];

        for malformed in malformed_subreports {
            let result = contract_success_ga_funnel(
                json!({
                    "funnelTable": malformed,
                    "funnelVisualization": valid_subreport()
                }),
                Instant::now(),
                FunnelResponseOptions {
                    query_hash: "abcd".to_string(),
                    output_mode: contract::OutputMode::Rows,
                    summary_only: false,
                    max_cell_chars: None,
                    effective_limit: 10,
                    requested_limit: None,
                },
            );
            let payload = result
                .structured_content
                .expect("malformed funnel response must be structured");
            assert_eq!(payload["ok"], json!(false));
            assert!(payload["error"]["message"].is_string());
        }
    }

    #[test]
    fn funnel_contract_accepts_omitted_repeated_fields_as_empty() {
        let result = contract_success_ga_funnel(
            json!({
                "funnelTable": {},
                "funnelVisualization": {}
            }),
            Instant::now(),
            FunnelResponseOptions {
                query_hash: "abcd".to_string(),
                output_mode: contract::OutputMode::Rows,
                summary_only: false,
                max_cell_chars: None,
                effective_limit: 10,
                requested_limit: None,
            },
        );
        let payload = result
            .structured_content
            .expect("omitted repeated fields should still be structured");
        assert_eq!(payload["ok"], json!(true));
        assert_eq!(payload["data"]["funnel_table"], json!([]));
        assert_eq!(payload["data"]["funnel_visualization"], json!([]));
    }

    #[test]
    fn conversions_contract_validates_tabular_response_shapes_fail_closed() {
        assert!(validate_ga_tabular_response_shape(&json!({}), "run_conversions_report").is_ok());

        let malformed_responses = vec![
            json!({"dimensionHeaders": null}),
            json!({"metricHeaders": {}}),
            json!({"rows": "not-an-array"}),
            json!({"dimensionHeaders": [null]}),
            json!({"metricHeaders": ["not-an-object"]}),
            json!({"rows": [null]}),
            json!({"rows": [{"dimensionValues": {}}]}),
            json!({"rows": [{"metricValues": [null]}]}),
        ];

        for response in malformed_responses {
            let err = validate_ga_tabular_response_shape(&response, "run_conversions_report")
                .expect_err("malformed conversion response shape must fail closed");
            assert!(err.to_string().contains("Google run_conversions_report"));
        }
    }

    #[test]
    fn validate_pivot_requires_pivots() {
        let err = validate_pivot_inputs(
            &["country".to_string()],
            &["activeUsers".to_string()],
            &[json!({"start_date": "2026-01-01", "end_date": "2026-01-31"})],
            &[],
        )
        .expect_err("empty pivots must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
    }

    #[test]
    fn validate_pivot_requires_pivot_fields_in_dimensions() {
        let err = validate_pivot_inputs(
            &["country".to_string()],
            &["activeUsers".to_string()],
            &[json!({"start_date": "2026-01-01", "end_date": "2026-01-31"})],
            &[json!({"field_names": ["city"]})],
        )
        .expect_err("pivot fields outside dimensions must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
    }

    #[test]
    fn validate_batch_run_reports_limits_request_count() {
        let one_request = BatchRunReportItemArgs {
            date_ranges: vec![json!({"start_date": "2026-01-01", "end_date": "2026-01-31"})],
            dimensions: vec!["country".to_string()],
            metrics: vec!["activeUsers".to_string()],
            dimension_filter: None,
            metric_filter: None,
            order_bys: None,
            limit: Some(10),
            offset: Some(0),
            currency_code: None,
            return_property_quota: false,
            max_rows: None,
            cursor: None,
            output_mode: None,
            summary_only: false,
            max_cell_chars: None,
        };
        let over_limit = vec![one_request; MAX_BATCH_REPORT_REQUESTS + 1];
        let err = validate_batch_run_reports_inputs(&over_limit)
            .expect_err("request count over max must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
    }

    #[test]
    fn apply_local_window_to_projection_updates_row_counts() {
        let projection = GaTabularProjection {
            rows: vec![
                Map::from_iter([("country".to_string(), json!("US"))]),
                Map::from_iter([("country".to_string(), json!("AU"))]),
                Map::from_iter([("country".to_string(), json!("DE"))]),
            ],
            row_count_total: 50,
            columns: vec![contract::ColumnMeta::new("country")],
            ga_meta: json!({}),
        };

        let windowed = apply_local_window_to_projection(projection, 1, 1, "run_pivot_report");
        assert_eq!(windowed.rows.len(), 1);
        assert_eq!(windowed.rows[0]["country"], json!("AU"));
        assert_eq!(windowed.row_count_total, 3);
        assert_eq!(
            windowed.ga_meta["pagination_mode"],
            json!("run_pivot_report")
        );
        assert_eq!(windowed.ga_meta["available_row_count"], json!(3));
        assert_eq!(windowed.ga_meta["upstream_row_count_total"], json!(50));
    }

    #[test]
    fn project_ga_tabular_response_builds_rows_and_meta() {
        let response = json!({
            "dimensionHeaders": [{ "name": "country" }],
            "metricHeaders": [{ "name": "activeUsers", "type": "TYPE_INTEGER" }],
            "rows": [
                {
                    "dimensionValues": [{ "value": "US" }],
                    "metricValues": [{ "value": "12" }]
                },
                {
                    "dimensionValues": [{ "value": "AU" }],
                    "metricValues": [{ "value": "8" }]
                }
            ],
            "rowCount": 5,
            "kind": "analyticsData#runReport"
        });

        let projection = project_ga_tabular_response(&response, "run_report");
        let (payload, meta) =
            project_payload_for_mode(&projection, contract::OutputMode::Rows, false, None);

        let rows = payload.as_array().expect("rows array expected");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["country"], json!("US"));
        assert_eq!(rows[0]["activeUsers"], json!("12"));
        assert_eq!(meta.row_count_total, 5);
        assert_eq!(meta.row_count_returned, 2);
        assert!(meta.truncated);
        assert_eq!(meta.output_mode, contract::OutputMode::Rows);
        assert_eq!(meta.columns.len(), 2);
    }

    #[test]
    fn project_ga_tabular_response_falls_back_when_row_count_missing() {
        let response = json!({
            "rows": [
                {
                    "dimensionValues": [],
                    "metricValues": []
                }
            ]
        });

        let projection = project_ga_tabular_response(&response, "run_realtime_report");
        let (payload, meta) =
            project_payload_for_mode(&projection, contract::OutputMode::Rows, false, None);
        assert_eq!(payload.as_array().map(Vec::len), Some(1));
        assert_eq!(meta.row_count_total, 1);
        assert_eq!(meta.row_count_returned, 1);
        assert!(!meta.truncated);
    }

    #[test]
    fn project_ga_tabular_response_supports_access_report_headers() {
        let response = json!({
            "dimensionHeaders": [{ "dimensionName": "userEmail" }],
            "metricHeaders": [{ "metricName": "accessCount" }],
            "rows": [
                {
                    "dimensionValues": [{ "value": "analyst@example.com" }],
                    "metricValues": [{ "value": "5" }]
                }
            ],
            "rowCount": 1,
            "quota": { "tokensPerDay": { "consumed": 1, "remaining": 9 } }
        });

        let projection = project_ga_tabular_response(&response, "run_property_access_report");
        let (payload, meta) =
            project_payload_for_mode(&projection, contract::OutputMode::Rows, false, None);

        let rows = payload.as_array().expect("rows expected");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["userEmail"], json!("analyst@example.com"));
        assert_eq!(rows[0]["accessCount"], json!("5"));
        assert_eq!(meta.columns[0].name, "userEmail");
        assert_eq!(meta.columns[1].name, "accessCount");
        assert_eq!(
            projection.ga_meta["quota"]["tokensPerDay"]["remaining"],
            json!(9)
        );
    }

    #[test]
    fn decode_cursor_rejects_invalid_shape() {
        let err = decode_cursor("bad-token").expect_err("invalid cursor should fail");
        assert_eq!(err.code(), "INVALID_CURSOR");
        assert_eq!(err.reason(), "invalid_cursor");
    }

    #[test]
    fn resolve_cursor_window_rejects_unreasonably_high_decoded_offset() {
        let raw_cursor = format!(
            "v1:deadbeef:{}",
            MAX_REPORT_LIMIT.saturating_mul(10).saturating_add(1)
        );
        let err = resolve_cursor_window("deadbeef", Some(&raw_cursor), None, Some(100), None)
            .expect_err("decoded cursor offset above the direct offset cap must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(err.to_string().contains("unreasonably high"));
    }

    #[test]
    fn ga_projection_does_not_repeat_cursor_for_empty_or_out_of_range_pages() {
        let empty_projection = GaTabularProjection {
            rows: Vec::new(),
            row_count_total: 5,
            columns: vec![contract::ColumnMeta::new("country")],
            ga_meta: json!({}),
        };
        let (_, empty_meta) = ga_projection_payload_and_meta(
            empty_projection,
            TabularResponseOptions {
                query_hash: "abcd".to_string(),
                output_mode: contract::OutputMode::Rows,
                summary_only: false,
                max_cell_chars: None,
                cursor_offset: 2,
            },
        );
        assert!(!matches!(
            empty_meta.get("next_cursor"),
            Some(Value::String(_))
        ));

        let out_of_range_projection = GaTabularProjection {
            rows: vec![Map::from_iter([("country".to_string(), json!("US"))])],
            row_count_total: 1,
            columns: vec![contract::ColumnMeta::new("country")],
            ga_meta: json!({}),
        };
        let (_, out_of_range_meta) = ga_projection_payload_and_meta(
            out_of_range_projection,
            TabularResponseOptions {
                query_hash: "abcd".to_string(),
                output_mode: contract::OutputMode::Rows,
                summary_only: false,
                max_cell_chars: None,
                cursor_offset: 1,
            },
        );
        assert!(!matches!(
            out_of_range_meta.get("next_cursor"),
            Some(Value::String(_))
        ));
    }

    #[test]
    fn resolve_cursor_window_rejects_hash_mismatch() {
        let err = resolve_cursor_window("deadbeef", Some("v1:feedface:10"), None, Some(100), None)
            .expect_err("mismatch should fail");
        assert_eq!(err.code(), "CURSOR_QUERY_MISMATCH");
        assert_eq!(err.reason(), "invalid_cursor");
    }

    #[test]
    fn normalize_optional_filter_accepts_json_string() {
        let parsed = normalize_optional_filter(
            Some(json!("{\"filter\":{\"field_name\":\"eventName\"}}")),
            "dimension_filter",
            true,
        )
        .expect("json string should parse");
        assert_eq!(
            parsed.expect("filter should exist")["filter"]["field_name"],
            json!("eventName")
        );
    }

    #[test]
    fn normalize_optional_filter_accepts_expression_shorthand() {
        let parsed = normalize_optional_filter(
            Some(json!("eventName==signup_complete")),
            "dimension_filter",
            true,
        )
        .expect("expression should parse")
        .expect("filter should be present");
        assert_eq!(parsed["filter"]["field_name"], json!("eventName"));
        assert_eq!(
            parsed["filter"]["string_filter"]["value"],
            json!("signup_complete")
        );
    }

    #[test]
    fn normalize_optional_filter_supports_quoted_whitespace_values() {
        let parsed = normalize_optional_filter(
            Some(json!("sessionDefaultChannelGroup==\"Paid Other\"")),
            "dimension_filter",
            true,
        )
        .expect("expression should parse")
        .expect("filter should be present");
        assert_eq!(
            parsed["filter"]["string_filter"]["value"],
            json!("Paid Other")
        );
    }

    #[test]
    fn normalize_optional_filter_rejects_unquoted_whitespace_values() {
        let err = normalize_optional_filter(
            Some(json!("sessionDefaultChannelGroup==Paid Other")),
            "dimension_filter",
            true,
        )
        .expect_err("unquoted whitespace values should fail");
        assert!(err.contains("should be quoted"));
    }

    #[test]
    fn normalize_optional_filter_rejects_metric_expression() {
        let err = normalize_optional_filter(Some(json!("activeUsers>100")), "metric_filter", false)
            .expect_err("metric filter shorthand should be rejected");
        assert!(err.contains("JSON object"));
    }

    #[test]
    fn deserialize_optional_output_mode_accepts_case_insensitive_values() {
        let parsed: OutputModeTestArgs = serde_json::from_value(json!({
            "output_mode": "ROWS"
        }))
        .expect("known mode should parse");
        assert_eq!(parsed.output_mode, Some(TabularOutputMode::Rows));
    }

    #[test]
    fn deserialize_optional_output_mode_rejects_unknown_values_with_example() {
        let err = serde_json::from_value::<OutputModeTestArgs>(json!({
            "output_mode": "table"
        }))
        .expect_err("unknown mode should fail");
        let message = err.to_string();
        assert!(message.contains("rows, tuples, scalar, compact"));
        assert!(message.contains("\"output_mode\":\"rows\""));
    }

    #[test]
    fn summarize_report_compatibility_flags_incompatibilities() {
        let payload = json!({
            "dimensionCompatibilities": [
                {
                    "dimensionMetadata": { "apiName": "sessionSource" },
                    "compatibility": "COMPATIBLE"
                },
                {
                    "dimensionMetadata": { "apiName": "landingPagePlusQueryString" },
                    "compatibility": "INCOMPATIBLE"
                }
            ],
            "metricCompatibilities": [
                {
                    "metricMetadata": { "apiName": "sessions" },
                    "compatibility": "COMPATIBLE"
                }
            ]
        });
        let summary = summarize_report_compatibility(&payload);
        assert_eq!(summary["is_fully_compatible"], json!(false));
        assert_eq!(
            summary["incompatible_dimensions"],
            json!(["landingPagePlusQueryString"])
        );
        assert_eq!(summary["incompatible_metrics"], json!([]));
        assert_eq!(summary["reason_codes"], json!(["INCOMPATIBLE_DIMENSIONS"]));
    }

    #[test]
    fn summarize_report_compatibility_reports_compatible_reason_code() {
        let payload = json!({
            "dimensionCompatibilities": [
                {
                    "dimensionMetadata": { "apiName": "sessionSource" },
                    "compatibility": "COMPATIBLE"
                }
            ],
            "metricCompatibilities": [
                {
                    "metricMetadata": { "apiName": "sessions" },
                    "compatibility": "COMPATIBLE"
                }
            ]
        });
        let summary = summarize_report_compatibility(&payload);
        assert_eq!(summary["is_fully_compatible"], json!(true));
        assert_eq!(summary["reason_codes"], json!(["COMPATIBLE"]));
    }

    #[test]
    fn resolve_ingest_page_size_defaults_to_max_page() {
        assert_eq!(
            resolve_ingest_page_size(None, None),
            DEFAULT_SCRATCHPAD_INGEST_PAGE_SIZE
        );
        assert_eq!(resolve_ingest_page_size(Some(5_000), Some(500)), 500);
    }

    #[test]
    fn parse_iso_date_literal_rejects_invalid_day() {
        let err = parse_iso_date_literal("2026-02-30").expect_err("invalid day should fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
    }

    #[test]
    fn build_release_regression_sql_embeds_core_fields() {
        let sql = build_release_regression_sql(
            "events_daily",
            "2026-02-24",
            "anchor_event",
            "compare_event",
            "date",
            "event_name",
            Some("event_count"),
            7,
            1,
            7,
        );
        assert!(sql.contains("TRY_STRPTIME"));
        assert!(sql.contains("likely_instrumentation_break"));
        assert!(sql.contains("anchor_event"));
        assert!(sql.contains("STDDEV_SAMP"));
        assert!(sql.contains("ratio_mean_delta_z"));
    }

    #[test]
    fn build_landing_param_shift_sql_embeds_core_fields() {
        let sql = build_landing_param_shift_sql(
            "landing_events",
            "2026-02-24",
            "date_parsed",
            "landingpageplusquerystring",
            Some("defaultchannelgroup"),
            Some("sessionsourcemedium"),
            7,
            1,
            7,
            50,
        );
        assert!(!sql.contains("scratchpad_landing_param_shift_report"));
        assert!(sql.contains("new_in_post"));
        assert!(sql.contains("rank_by_abs_delta"));
        assert!(sql.contains("landingpageplusquerystring"));
        assert!(sql.contains("md5(param_key || '=' || param_value)"));
    }

    #[test]
    fn resolve_landing_shift_top_n_rejects_out_of_range_values() {
        let err = resolve_landing_shift_top_n(Some(0)).expect_err("zero top_n must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
        let err = resolve_landing_shift_top_n(Some(MAX_LANDING_SHIFT_TOP_N + 1))
            .expect_err("top_n over max must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
    }

    #[test]
    fn resolve_evidence_sample_rows_rejects_out_of_range_values() {
        let err = resolve_evidence_sample_rows(Some(0)).expect_err("zero sample rows must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
        let err = resolve_evidence_sample_rows(Some(MAX_EVIDENCE_SAMPLE_ROWS + 1))
            .expect_err("sample rows over max must fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
    }

    #[test]
    fn payload_by_mode_scalar_returns_first_column() {
        let rows = vec![Map::from_iter([("a".to_string(), json!("x"))])];
        let columns = vec![contract::ColumnMeta::new("a")];
        let payload = payload_by_mode(&rows, &columns, contract::OutputMode::Scalar);
        assert_eq!(payload, json!("x"));
    }

    #[test]
    fn clip_rows_applies_max_cell_chars() {
        let rows = vec![Map::from_iter([(
            "message".to_string(),
            json!("abcdefghijklmnopqrstuvwxyz"),
        )])];
        let (clipped, count) = clip_rows(&rows, 5);
        assert_eq!(count, 1);
        assert_eq!(clipped[0]["message"], json!("abcde..."));
    }

    #[test]
    fn resolve_scratchpad_list_limit_uses_default() {
        assert_eq!(
            resolve_scratchpad_list_limit(None).expect("default should resolve"),
            50
        );
    }

    #[test]
    fn resolve_scratchpad_list_limit_rejects_zero() {
        let err = resolve_scratchpad_list_limit(Some(0)).expect_err("zero limit should fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
    }

    #[test]
    fn normalize_sql_identifier_is_deterministic() {
        let normalized = normalize_sql_identifier("Event Name (GA4)", "table");
        assert_eq!(normalized, "event_name_ga4");
    }

    #[test]
    fn build_ingest_column_mappings_deduplicates_target_names() {
        let columns = vec![
            contract::ColumnMeta::new("Event Name").with_logical_type("string"),
            contract::ColumnMeta::new("Event-Name").with_logical_type("string"),
        ];
        let mappings = build_ingest_column_mappings(&columns);
        assert_eq!(mappings[0].target_name, "event_name");
        assert_eq!(mappings[1].target_name, "event_name_2");
    }

    #[test]
    fn build_ingest_column_mappings_adds_temporal_parsed_columns() {
        let columns = vec![
            contract::ColumnMeta::new("date").with_logical_type("string"),
            contract::ColumnMeta::new("dateHour").with_logical_type("string"),
        ];
        let mappings = build_ingest_column_mappings(&columns);
        assert_eq!(mappings.len(), 4);
        assert_eq!(mappings[0].target_name, "date");
        assert_eq!(mappings[1].target_name, "date_parsed");
        assert_eq!(mappings[1].logical_type, "date");
        assert_eq!(mappings[2].target_name, "datehour");
        assert_eq!(mappings[3].target_name, "datehour_parsed");
        assert_eq!(mappings[3].logical_type, "timestamp");
    }

    #[test]
    fn remap_rows_for_ingest_populates_temporal_parsed_columns() {
        let columns = vec![
            contract::ColumnMeta::new("date").with_logical_type("string"),
            contract::ColumnMeta::new("dateHour").with_logical_type("string"),
        ];
        let mappings = build_ingest_column_mappings(&columns);
        let rows = vec![Map::from_iter([
            ("date".to_string(), json!("20260211")),
            ("dateHour".to_string(), json!("2026021121")),
        ])];

        let remapped = remap_rows_for_ingest(&rows, &mappings);
        assert_eq!(remapped.len(), 1);
        assert_eq!(remapped[0]["date"], json!("20260211"));
        assert_eq!(remapped[0]["date_parsed"], json!("2026-02-11"));
        assert_eq!(remapped[0]["datehour"], json!("2026021121"));
        assert_eq!(remapped[0]["datehour_parsed"], json!("2026-02-11 21:00:00"));
    }

    #[test]
    fn remap_rows_for_ingest_nulls_malformed_temporal_values() {
        let columns = vec![
            contract::ColumnMeta::new("date").with_logical_type("string"),
            contract::ColumnMeta::new("dateHour").with_logical_type("string"),
        ];
        let mappings = build_ingest_column_mappings(&columns);
        let rows = vec![Map::from_iter([
            ("date".to_string(), json!("20260230")),
            ("dateHour".to_string(), json!("2026021199")),
        ])];

        let remapped = remap_rows_for_ingest(&rows, &mappings);
        assert_eq!(remapped.len(), 1);
        assert_eq!(remapped[0]["date"], json!("20260230"));
        assert_eq!(remapped[0]["date_parsed"], Value::Null);
        assert_eq!(remapped[0]["datehour"], json!("2026021199"));
        assert_eq!(remapped[0]["datehour_parsed"], Value::Null);
    }

    #[test]
    fn scratchpad_query_hash_binds_session_and_sql() {
        let base = scratchpad_query_hash("scratchpad_query", "s1", "SELECT 1")
            .expect("hash should succeed");
        let with_other_tool = scratchpad_query_hash("scratchpad_summarize_table", "s1", "SELECT 1")
            .expect("hash should succeed");
        let with_other_session = scratchpad_query_hash("scratchpad_query", "s2", "SELECT 1")
            .expect("hash should succeed");
        let with_other_sql = scratchpad_query_hash("scratchpad_query", "s1", "SELECT 2")
            .expect("hash should succeed");
        assert_ne!(base, with_other_tool);
        assert_ne!(base, with_other_session);
        assert_ne!(base, with_other_sql);
    }

    #[test]
    fn supports_wrapped_pagination_detects_select_and_with() {
        assert!(supports_wrapped_pagination("SELECT * FROM events"));
        assert!(supports_wrapped_pagination(
            "  with x as (select 1) select * from x"
        ));
        assert!(!supports_wrapped_pagination(
            "DESCRIBE SELECT * FROM events"
        ));
        assert!(!supports_wrapped_pagination(
            "SUMMARIZE SELECT * FROM events"
        ));
    }

    #[test]
    fn trim_sql_for_subquery_removes_trailing_semicolons() {
        assert_eq!(trim_sql_for_subquery("SELECT 1;;;"), "SELECT 1");
        assert_eq!(trim_sql_for_subquery("  SELECT 1;  "), "SELECT 1");
    }

    #[test]
    fn wrap_query_for_page_includes_limit_and_offset() {
        let wrapped = wrap_query_for_page("SELECT * FROM events", 200, 50);
        assert!(wrapped.contains("LIMIT 50"));
        assert!(wrapped.contains("OFFSET 200"));
        assert!(wrapped.contains("FROM (SELECT * FROM events) AS ga4_scratchpad_query"));
    }

    #[test]
    fn dedupe_column_names_appends_suffixes() {
        let deduped = dedupe_column_names(vec![
            "Event Name".to_string(),
            "Event-Name".to_string(),
            "event_name".to_string(),
        ]);
        assert_eq!(deduped, vec!["event_name", "event_name_2", "event_name_3"]);
    }

    #[test]
    fn duck_value_to_json_handles_nested_values() {
        let value = DuckValue::Struct(duckdb::types::OrderedMap::from(vec![
            ("country".to_string(), DuckValue::Text("US".to_string())),
            (
                "metrics".to_string(),
                DuckValue::List(vec![DuckValue::Int(12), DuckValue::Double(1.25)]),
            ),
        ]));
        let json = duck_value_to_json(value);
        assert_eq!(json["country"], json!("US"));
        assert_eq!(json["metrics"][0], json!(12));
        assert_eq!(json["metrics"][1], json!(1.25));
    }

    #[test]
    fn duckdb_type_to_logical_type_maps_common_types() {
        assert_eq!(duckdb_type_to_logical_type(&DuckDataType::Int64), "integer");
        assert_eq!(
            duckdb_type_to_logical_type(&DuckDataType::Float64),
            "number"
        );
        assert_eq!(
            duckdb_type_to_logical_type(&DuckDataType::Timestamp(
                duckdb::arrow::datatypes::TimeUnit::Microsecond,
                None
            )),
            "datetime"
        );
    }
}
