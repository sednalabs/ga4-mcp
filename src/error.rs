//! # Error Types
//!
//! Shared error model for tool validation, auth, transport, and upstream API failures.

use thiserror::Error;

use crate::sql_safety::ScratchpadSqlPolicyCode;

#[derive(Debug, Error)]
pub enum AnalyticsError {
    #[error("invalid {field}: {message}")]
    InvalidArgument {
        field: &'static str,
        message: String,
    },

    #[error("invalid cursor: {0}")]
    InvalidCursor(String),

    #[error("cursor does not match query hash")]
    CursorQueryMismatch,

    #[error("authentication bootstrap failed: {0}")]
    AuthBootstrap(String),

    #[error("missing request access token in header {header}")]
    MissingRequestAccessToken { header: String },

    #[error("malformed request access token in header {header}: {message}")]
    MalformedRequestAccessToken { header: String, message: String },

    #[error("token provider error: {0}")]
    TokenProvider(#[from] gcp_auth::Error),

    #[error("http client transport error: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("failed to parse upstream JSON: {0}")]
    UpstreamJson(#[from] serde_json::Error),

    #[error("upstream API request failed with status {status}: {message}")]
    UpstreamApi { status: u16, message: String },

    #[error("scratchpad engine error: {0}")]
    ScratchpadEngine(String),

    #[error("scratchpad limit exceeded for {field}: {message}")]
    ScratchpadLimitExceeded {
        field: &'static str,
        message: String,
    },

    #[error("scratchpad sql rejected ({policy_code}): {message}")]
    ScratchpadSqlRejected {
        policy_code: ScratchpadSqlPolicyCode,
        message: String,
    },

    #[error("scratchpad query timed out after {timeout_ms}ms")]
    ScratchpadQueryTimeout { timeout_ms: u64 },

    #[error("scratchpad query cancelled")]
    ScratchpadQueryCancelled,

    #[error("scratchpad session not found: {session_id}")]
    ScratchpadSessionNotFound { session_id: String },

    #[error("tool '{tool}' is blocked by capability profile '{profile}'")]
    PolicyDenied { profile: String, tool: String },

    #[error("internal error: {0}")]
    Internal(String),
}

impl AnalyticsError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidArgument { .. } => "INVALID_PARAMS",
            Self::InvalidCursor(_) => "INVALID_CURSOR",
            Self::CursorQueryMismatch => "CURSOR_QUERY_MISMATCH",
            Self::AuthBootstrap(_)
            | Self::TokenProvider(_)
            | Self::MissingRequestAccessToken { .. }
            | Self::MalformedRequestAccessToken { .. } => "AUTHENTICATION_FAILED",
            Self::Transport(_) => "UPSTREAM_TRANSPORT_ERROR",
            Self::UpstreamJson(_) => "UPSTREAM_RESPONSE_PARSE_ERROR",
            Self::UpstreamApi { status, .. } if *status >= 500 => "UPSTREAM_UNAVAILABLE",
            Self::UpstreamApi { .. } => "UPSTREAM_REJECTED",
            Self::ScratchpadEngine(_) => "SCRATCHPAD_ENGINE_ERROR",
            Self::ScratchpadLimitExceeded { .. } => "SCRATCHPAD_LIMIT_EXCEEDED",
            Self::ScratchpadSqlRejected { .. } => "SCRATCHPAD_SQL_REJECTED",
            Self::ScratchpadQueryTimeout { .. } => "SCRATCHPAD_QUERY_TIMEOUT",
            Self::ScratchpadQueryCancelled => "SCRATCHPAD_QUERY_CANCELLED",
            Self::ScratchpadSessionNotFound { .. } => "SCRATCHPAD_SESSION_NOT_FOUND",
            Self::PolicyDenied { .. } => "POLICY_DENIED",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    pub fn reason(&self) -> &'static str {
        match self {
            Self::InvalidArgument { .. } => "invalid_params",
            Self::InvalidCursor(_) | Self::CursorQueryMismatch => "invalid_cursor",
            Self::AuthBootstrap(_)
            | Self::TokenProvider(_)
            | Self::MissingRequestAccessToken { .. }
            | Self::MalformedRequestAccessToken { .. } => "auth_failed",
            Self::Transport(_) => "upstream_transport",
            Self::UpstreamJson(_) => "upstream_response_invalid",
            Self::UpstreamApi { status, .. } if *status >= 500 => "upstream_unavailable",
            Self::UpstreamApi { .. } => "upstream_rejected",
            Self::ScratchpadLimitExceeded { .. } => "scratchpad_limit_exceeded",
            Self::ScratchpadEngine(_) => "scratchpad_engine_error",
            Self::ScratchpadSqlRejected { .. } => "scratchpad_sql_restricted",
            Self::ScratchpadQueryTimeout { .. } => "scratchpad_timeout",
            Self::ScratchpadQueryCancelled => "scratchpad_cancelled",
            Self::ScratchpadSessionNotFound { .. } => "scratchpad_session_not_found",
            Self::PolicyDenied { .. } => "policy_denied",
            Self::Internal(_) => "internal_error",
        }
    }

    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::UpstreamApi { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn category(&self) -> &'static str {
        match self {
            Self::InvalidArgument { .. } => "validation",
            Self::InvalidCursor(_) | Self::CursorQueryMismatch => "cursor",
            Self::AuthBootstrap(_)
            | Self::TokenProvider(_)
            | Self::MissingRequestAccessToken { .. }
            | Self::MalformedRequestAccessToken { .. } => "auth",
            Self::Transport(_) => "transport",
            Self::UpstreamJson(_) => "upstream_parse",
            Self::UpstreamApi { .. } => "upstream_api",
            Self::ScratchpadLimitExceeded { .. } => "scratchpad",
            Self::ScratchpadEngine(_) => "scratchpad",
            Self::ScratchpadSqlRejected { .. } => "scratchpad",
            Self::ScratchpadQueryTimeout { .. } => "scratchpad",
            Self::ScratchpadQueryCancelled => "scratchpad",
            Self::ScratchpadSessionNotFound { .. } => "scratchpad",
            Self::PolicyDenied { .. } => "policy",
            Self::Internal(_) => "internal",
        }
    }

    pub fn engine_code(&self) -> Option<String> {
        match self {
            Self::UpstreamApi { status, .. } => Some(format!("http_{status}")),
            Self::ScratchpadEngine(_) => Some("duckdb".to_string()),
            Self::ScratchpadSqlRejected { policy_code, .. } => {
                Some(policy_code.as_str().to_string())
            }
            _ => None,
        }
    }

    pub fn detail(&self) -> Option<String> {
        match self {
            Self::InvalidArgument { field, .. } => Some(format!("field={field}")),
            Self::InvalidCursor(message) => Some(message.clone()),
            Self::ScratchpadLimitExceeded { field, .. } => Some(format!("field={field}")),
            Self::ScratchpadQueryTimeout { timeout_ms } => Some(format!("timeout_ms={timeout_ms}")),
            Self::ScratchpadSessionNotFound { session_id } => {
                Some(format!("session_id={session_id}"))
            }
            Self::MissingRequestAccessToken { header } => Some(format!("header={header}")),
            Self::MalformedRequestAccessToken { header, .. } => Some(format!("header={header}")),
            Self::PolicyDenied { profile, tool } => Some(format!("profile={profile};tool={tool}")),
            _ => None,
        }
    }

    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::InvalidArgument { .. } => {
                Some("Check the tool argument schema and required fields.")
            }
            Self::InvalidCursor(_) => {
                Some("Use the opaque meta.next_cursor value from the previous response.")
            }
            Self::CursorQueryMismatch => {
                Some("Use cursor with the exact same query parameters that produced it.")
            }
            Self::AuthBootstrap(_) | Self::TokenProvider(_) => Some(
                "Ensure either ADC is configured with analytics.readonly scope or OAuth refresh-token auth settings are valid.",
            ),
            Self::MissingRequestAccessToken { .. } => Some(
                "Configure the MCP client OAuth flow so each request sends a Google access token in the configured upstream token header.",
            ),
            Self::MalformedRequestAccessToken { .. } => Some(
                "Send a valid OAuth access token in the configured upstream token header.",
            ),
            Self::Transport(_) => Some("Check network connectivity and upstream API availability."),
            Self::UpstreamJson(_) => {
                Some("Retry later; upstream payload may be transiently malformed.")
            }
            Self::UpstreamApi { status, .. } if *status == 403 => {
                Some("Verify property access and OAuth scope permissions.")
            }
            Self::UpstreamApi { status, .. } if *status == 429 => {
                Some("Retry with exponential backoff.")
            }
            Self::UpstreamApi { status, .. } if *status >= 500 => {
                Some("Upstream may be unavailable; retry with backoff.")
            }
            Self::ScratchpadEngine(_) => {
                Some("Check DuckDB runtime availability and server scratchpad configuration.")
            }
            Self::ScratchpadLimitExceeded { .. } => {
                Some("Release sessions/tables or reduce input size before retrying.")
            }
            Self::ScratchpadSqlRejected { .. } => {
                Some("Use read-only SELECT/WITH/EXPLAIN/DESCRIBE/SUMMARIZE queries only.")
            }
            Self::ScratchpadQueryTimeout { .. } => {
                Some("Reduce query complexity or increase scratchpad query timeout.")
            }
            Self::ScratchpadQueryCancelled => {
                Some("Retry the query if cancellation was not intentional.")
            }
            Self::ScratchpadSessionNotFound { .. } => {
                Some("Open the scratchpad session before querying or listing tables.")
            }
            Self::PolicyDenied { .. } => Some(
                "Switch capability profile to scratchpad for scratchpad tools, or use GA read tools under read_only.",
            ),
            _ => None,
        }
    }

    pub fn position(&self) -> Option<&str> {
        let _ = self;
        None
    }

    pub fn invalid(field: &'static str, message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            field,
            message: message.into(),
        }
    }

    pub fn invalid_cursor(message: impl Into<String>) -> Self {
        Self::InvalidCursor(message.into())
    }

    pub fn scratchpad_limit(field: &'static str, message: impl Into<String>) -> Self {
        Self::ScratchpadLimitExceeded {
            field,
            message: message.into(),
        }
    }

    pub fn scratchpad_sql_rejected(
        policy_code: ScratchpadSqlPolicyCode,
        message: impl Into<String>,
    ) -> Self {
        Self::ScratchpadSqlRejected {
            policy_code,
            message: message.into(),
        }
    }

    pub fn scratchpad_query_timeout(timeout: std::time::Duration) -> Self {
        let timeout_ms = timeout.as_millis();
        let timeout_ms = u64::try_from(timeout_ms).unwrap_or(u64::MAX);
        Self::ScratchpadQueryTimeout { timeout_ms }
    }

    pub fn scratchpad_query_cancelled() -> Self {
        Self::ScratchpadQueryCancelled
    }

    pub fn scratchpad_session_not_found(session_id: impl Into<String>) -> Self {
        Self::ScratchpadSessionNotFound {
            session_id: session_id.into(),
        }
    }

    pub fn policy_denied(profile: impl Into<String>, tool: impl Into<String>) -> Self {
        Self::PolicyDenied {
            profile: profile.into(),
            tool: tool.into(),
        }
    }

    pub fn missing_request_access_token(header: impl Into<String>) -> Self {
        Self::MissingRequestAccessToken {
            header: header.into(),
        }
    }

    pub fn malformed_request_access_token(
        header: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::MalformedRequestAccessToken {
            header: header.into(),
            message: message.into(),
        }
    }
}
