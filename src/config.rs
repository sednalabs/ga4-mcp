//! # Configuration
//!
//! CLI and environment-backed runtime configuration for ga4-mcp.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::{Args, Parser, Subcommand, ValueEnum};

pub const DEFAULT_ANALYTICS_SCOPE: &str = "https://www.googleapis.com/auth/analytics.readonly";
const DEFAULT_ADMIN_BASE_URL: &str = "https://analyticsadmin.googleapis.com";
const DEFAULT_DATA_BASE_URL: &str = "https://analyticsdata.googleapis.com";
const DEFAULT_HTTP_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_MAX_PAGE_SIZE: u32 = 200;
const DEFAULT_MAX_PAGES: u32 = 20;
const DEFAULT_USER_AGENT: &str = "ga4-mcp/0.1.0";
const DEFAULT_SCRATCHPAD_SESSION_TTL_SECS: u64 = 900;
const DEFAULT_SCRATCHPAD_MAX_SESSIONS: usize = 64;
const DEFAULT_SCRATCHPAD_MAX_TABLES_PER_SESSION: usize = 32;
const DEFAULT_SCRATCHPAD_MAX_ROWS_PER_SESSION: usize = 1_000_000;
const DEFAULT_SCRATCHPAD_MAX_MEMORY_MB: usize = 256;
const DEFAULT_SCRATCHPAD_QUERY_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_SCRATCHPAD_MAX_SQL_BYTES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum CapabilityProfile {
    ReadOnly,
    Scratchpad,
}

impl CapabilityProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Scratchpad => "scratchpad",
        }
    }

    pub fn allows_tool(self, tool_name: &str) -> bool {
        match self {
            Self::ReadOnly => !tool_name.starts_with("scratchpad_"),
            Self::Scratchpad => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum UpstreamTokenSource {
    Config,
    RequestHeader,
    RequestHeaderOrConfig,
}

impl UpstreamTokenSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::RequestHeader => "request_header",
            Self::RequestHeaderOrConfig => "request_header_or_config",
        }
    }

    pub fn uses_request_header(self) -> bool {
        matches!(self, Self::RequestHeader | Self::RequestHeaderOrConfig)
    }
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "ga4-mcp",
    version,
    about = "Rust stdio MCP server for Google Analytics (GA4)"
)]
pub struct Cli {
    /// OAuth scope used for token acquisition.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_SCOPE",
        global = true,
        default_value = DEFAULT_ANALYTICS_SCOPE
    )]
    pub analytics_scope: String,

    /// Base URL for Google Analytics Admin API.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_ADMIN_BASE_URL",
        global = true,
        default_value = DEFAULT_ADMIN_BASE_URL
    )]
    pub admin_base_url: String,

    /// Base URL for Google Analytics Data API.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_DATA_BASE_URL",
        global = true,
        default_value = DEFAULT_DATA_BASE_URL
    )]
    pub data_base_url: String,

    /// HTTP timeout budget in milliseconds.
    #[arg(long, env = "GOOGLE_ANALYTICS_MCP_HTTP_TIMEOUT_MS", default_value_t = DEFAULT_HTTP_TIMEOUT_MS)]
    pub http_timeout_ms: u64,

    /// Maximum per-page size for paginated read tools.
    #[arg(long, env = "GOOGLE_ANALYTICS_MCP_MAX_PAGE_SIZE", default_value_t = DEFAULT_MAX_PAGE_SIZE)]
    pub max_page_size: u32,

    /// Default max pages for auto-pagination tools.
    #[arg(long, env = "GOOGLE_ANALYTICS_MCP_MAX_PAGES", default_value_t = DEFAULT_MAX_PAGES)]
    pub max_pages: u32,

    /// User-Agent string applied to outbound Google API requests.
    #[arg(long, env = "GOOGLE_ANALYTICS_MCP_USER_AGENT", default_value = DEFAULT_USER_AGENT)]
    pub user_agent: String,

    /// Optional path to OAuth client-secret JSON (`installed` or `web`) for refresh-token auth.
    ///
    /// When this is set, `GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN` must also be set.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON",
        global = true
    )]
    pub oauth_client_secret_json: Option<String>,

    /// Optional OAuth refresh token used with `GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON`.
    #[arg(long, env = "GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN", global = true)]
    pub oauth_refresh_token: Option<String>,

    /// Token source for upstream Google API calls.
    ///
    /// - `config`: server-side credentials (ADC or OAuth refresh token).
    /// - `request_header`: token must be supplied per request in `upstream_token_header`.
    /// - `request_header_or_config`: request header first, then server-side fallback.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE",
        global = true,
        value_enum,
        default_value_t = UpstreamTokenSource::Config
    )]
    pub upstream_token_source: UpstreamTokenSource,

    /// Request header name used when `upstream_token_source` reads request tokens.
    ///
    /// Use `authorization` for standard `Authorization: Bearer <token>` flows.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER",
        global = true,
        default_value = "authorization"
    )]
    pub upstream_token_header: String,

    /// Optional quota/billing project for Google APIs (`x-goog-user-project`).
    #[arg(long, env = "GOOGLE_ANALYTICS_MCP_QUOTA_PROJECT", global = true)]
    pub quota_project: Option<String>,

    /// Scratchpad session TTL in seconds.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_SCRATCHPAD_SESSION_TTL_SECS",
        default_value_t = DEFAULT_SCRATCHPAD_SESSION_TTL_SECS
    )]
    pub scratchpad_session_ttl_secs: u64,

    /// Maximum number of active scratchpad sessions.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_SCRATCHPAD_MAX_SESSIONS",
        default_value_t = DEFAULT_SCRATCHPAD_MAX_SESSIONS
    )]
    pub scratchpad_max_sessions: usize,

    /// Maximum number of tables tracked per scratchpad session.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_SCRATCHPAD_MAX_TABLES_PER_SESSION",
        default_value_t = DEFAULT_SCRATCHPAD_MAX_TABLES_PER_SESSION
    )]
    pub scratchpad_max_tables_per_session: usize,

    /// Maximum number of rows tracked per scratchpad session.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_SCRATCHPAD_MAX_ROWS_PER_SESSION",
        default_value_t = DEFAULT_SCRATCHPAD_MAX_ROWS_PER_SESSION
    )]
    pub scratchpad_max_rows_per_session: usize,

    /// Maximum DuckDB memory limit (MB) applied per scratchpad session connection.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_SCRATCHPAD_MAX_MEMORY_MB",
        default_value_t = DEFAULT_SCRATCHPAD_MAX_MEMORY_MB
    )]
    pub scratchpad_max_memory_mb: usize,

    /// Scratchpad query timeout in milliseconds.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_SCRATCHPAD_QUERY_TIMEOUT_MS",
        default_value_t = DEFAULT_SCRATCHPAD_QUERY_TIMEOUT_MS
    )]
    pub scratchpad_query_timeout_ms: u64,

    /// Maximum SQL payload size accepted by scratchpad query guardrails.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_SCRATCHPAD_MAX_SQL_BYTES",
        default_value_t = DEFAULT_SCRATCHPAD_MAX_SQL_BYTES
    )]
    pub scratchpad_max_sql_bytes: usize,

    /// Capability profile used for dispatch-time tool authorization.
    #[arg(
        long,
        env = "GOOGLE_ANALYTICS_MCP_CAPABILITY_PROFILE",
        global = true,
        value_enum,
        default_value_t = CapabilityProfile::ReadOnly
    )]
    pub capability_profile: CapabilityProfile,

    /// Print registered tool names and exit.
    #[arg(long)]
    pub print_tools: bool,

    /// Print full tool schema snapshot JSON and exit.
    #[arg(long)]
    pub print_tool_schema: bool,

    /// Optional command. Omit to run the stdio MCP server.
    #[command(subcommand)]
    pub command: Option<CliCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum CliCommand {
    /// Run the stdio MCP server. This is also the default when no command is supplied.
    Serve,
    /// Login, verify, and diagnose Google Analytics credentials.
    Auth(AuthCli),
}

#[derive(Debug, Clone, Args)]
pub struct AuthCli {
    #[command(subcommand)]
    pub command: AuthSubcommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum AuthSubcommand {
    /// Run the browser-based gcloud Application Default Credentials login flow.
    Login(AuthLoginArgs),
    /// Print the exact gcloud login command without running it.
    Command(AuthCommandArgs),
    /// Show the configured credential source and optional Google API verification result.
    Status(AuthStatusCliArgs),
    /// Check the local auth environment and suggest the next action.
    Doctor(AuthDoctorArgs),
}

#[derive(Debug, Clone, Args)]
pub struct AuthLoginArgs {
    /// Print a browser URL instead of launching a browser where supported by gcloud.
    #[arg(long)]
    pub headless: bool,

    /// Optional Google OAuth client id file for gcloud ADC login.
    #[arg(long)]
    pub client_id_file: Option<PathBuf>,

    /// Print the command that would run, without invoking gcloud.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip post-login Google API verification.
    #[arg(long)]
    pub no_verify: bool,
}

#[derive(Debug, Clone, Args)]
pub struct AuthCommandArgs {
    /// Include the headless browser flag in the printed gcloud command.
    #[arg(long)]
    pub headless: bool,

    /// Optional Google OAuth client id file for gcloud ADC login.
    #[arg(long)]
    pub client_id_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct AuthStatusCliArgs {
    /// Acquire a Google access token and call GA account summaries. The token is never printed.
    #[arg(long)]
    pub verify_token: bool,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct AuthDoctorArgs {
    /// Acquire a Google access token and call GA account summaries. The token is never printed.
    #[arg(long)]
    pub verify_token: bool,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub analytics_scope: String,
    pub admin_base_url: String,
    pub data_base_url: String,
    pub http_timeout: Duration,
    pub max_page_size: u32,
    pub max_pages: u32,
    pub user_agent: String,
    pub oauth_client_secret_json: Option<String>,
    pub oauth_refresh_token: Option<String>,
    pub upstream_token_source: UpstreamTokenSource,
    pub upstream_token_header: String,
    pub quota_project: Option<String>,
    pub scratchpad_session_ttl: Duration,
    pub scratchpad_max_sessions: usize,
    pub scratchpad_max_tables_per_session: usize,
    pub scratchpad_max_rows_per_session: usize,
    pub scratchpad_max_memory_mb: usize,
    pub scratchpad_query_timeout: Duration,
    pub scratchpad_max_sql_bytes: usize,
    pub capability_profile: CapabilityProfile,
    pub print_tools: bool,
    pub print_tool_schema: bool,
    pub command: Option<CliCommand>,
}

impl Settings {
    pub fn from_cli(cli: Cli) -> Result<Self> {
        let analytics_scope = cli.analytics_scope.trim();
        if analytics_scope.is_empty() {
            return Err(anyhow!("analytics scope must not be empty"));
        }

        if cli.http_timeout_ms == 0 {
            return Err(anyhow!("http timeout must be greater than zero"));
        }
        if cli.max_page_size == 0 {
            return Err(anyhow!("max page size must be greater than zero"));
        }
        if cli.max_pages == 0 {
            return Err(anyhow!("max pages must be greater than zero"));
        }
        if cli.scratchpad_session_ttl_secs == 0 {
            return Err(anyhow!("scratchpad session ttl must be greater than zero"));
        }
        if cli.scratchpad_max_sessions == 0 {
            return Err(anyhow!("scratchpad max sessions must be greater than zero"));
        }
        if cli.scratchpad_max_tables_per_session == 0 {
            return Err(anyhow!(
                "scratchpad max tables per session must be greater than zero"
            ));
        }
        if cli.scratchpad_max_rows_per_session == 0 {
            return Err(anyhow!(
                "scratchpad max rows per session must be greater than zero"
            ));
        }
        if cli.scratchpad_max_memory_mb == 0 {
            return Err(anyhow!(
                "scratchpad max memory mb must be greater than zero"
            ));
        }
        if cli.scratchpad_query_timeout_ms == 0 {
            return Err(anyhow!(
                "scratchpad query timeout must be greater than zero"
            ));
        }
        if cli.scratchpad_max_sql_bytes == 0 {
            return Err(anyhow!(
                "scratchpad max sql bytes must be greater than zero"
            ));
        }

        let oauth_client_secret_json = trim_optional(cli.oauth_client_secret_json);
        let oauth_refresh_token = trim_optional(cli.oauth_refresh_token);
        if oauth_client_secret_json.is_some() != oauth_refresh_token.is_some() {
            return Err(anyhow!(
                "GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON and GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN must be set together"
            ));
        }
        let upstream_token_header = sanitize_header_name(&cli.upstream_token_header)?;
        if cli.upstream_token_source.uses_request_header() && upstream_token_header.is_empty() {
            return Err(anyhow!(
                "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER must be set when GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE uses request headers"
            ));
        }

        Ok(Self {
            analytics_scope: analytics_scope.to_string(),
            admin_base_url: sanitize_base_url(&cli.admin_base_url)?,
            data_base_url: sanitize_base_url(&cli.data_base_url)?,
            http_timeout: Duration::from_millis(cli.http_timeout_ms),
            max_page_size: cli.max_page_size,
            max_pages: cli.max_pages,
            user_agent: sanitize_user_agent(&cli.user_agent)?,
            oauth_client_secret_json,
            oauth_refresh_token,
            upstream_token_source: cli.upstream_token_source,
            upstream_token_header,
            quota_project: trim_optional(cli.quota_project),
            scratchpad_session_ttl: Duration::from_secs(cli.scratchpad_session_ttl_secs),
            scratchpad_max_sessions: cli.scratchpad_max_sessions,
            scratchpad_max_tables_per_session: cli.scratchpad_max_tables_per_session,
            scratchpad_max_rows_per_session: cli.scratchpad_max_rows_per_session,
            scratchpad_max_memory_mb: cli.scratchpad_max_memory_mb,
            scratchpad_query_timeout: Duration::from_millis(cli.scratchpad_query_timeout_ms),
            scratchpad_max_sql_bytes: cli.scratchpad_max_sql_bytes,
            capability_profile: cli.capability_profile,
            print_tools: cli.print_tools,
            print_tool_schema: cli.print_tool_schema,
            command: cli.command,
        })
    }
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn sanitize_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("base URL must not be empty"));
    }
    if !trimmed.starts_with("https://") {
        return Err(anyhow!("base URL must start with https://"));
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

fn sanitize_user_agent(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("user agent must not be empty"));
    }
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(anyhow!("user agent must not contain newlines"));
    }
    Ok(trimmed.to_string())
}

fn sanitize_header_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("upstream token header must not be empty"));
    }
    if !trimmed
        .bytes()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
    {
        return Err(anyhow!(
            "upstream token header must contain only ASCII letters, digits, and '-'"
        ));
    }
    Ok(trimmed.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cli() -> Cli {
        Cli {
            analytics_scope: DEFAULT_ANALYTICS_SCOPE.to_string(),
            admin_base_url: DEFAULT_ADMIN_BASE_URL.to_string(),
            data_base_url: DEFAULT_DATA_BASE_URL.to_string(),
            http_timeout_ms: DEFAULT_HTTP_TIMEOUT_MS,
            max_page_size: DEFAULT_MAX_PAGE_SIZE,
            max_pages: DEFAULT_MAX_PAGES,
            user_agent: DEFAULT_USER_AGENT.to_string(),
            oauth_client_secret_json: None,
            oauth_refresh_token: None,
            upstream_token_source: UpstreamTokenSource::Config,
            upstream_token_header: "authorization".to_string(),
            quota_project: None,
            scratchpad_session_ttl_secs: DEFAULT_SCRATCHPAD_SESSION_TTL_SECS,
            scratchpad_max_sessions: DEFAULT_SCRATCHPAD_MAX_SESSIONS,
            scratchpad_max_tables_per_session: DEFAULT_SCRATCHPAD_MAX_TABLES_PER_SESSION,
            scratchpad_max_rows_per_session: DEFAULT_SCRATCHPAD_MAX_ROWS_PER_SESSION,
            scratchpad_max_memory_mb: DEFAULT_SCRATCHPAD_MAX_MEMORY_MB,
            scratchpad_query_timeout_ms: DEFAULT_SCRATCHPAD_QUERY_TIMEOUT_MS,
            scratchpad_max_sql_bytes: DEFAULT_SCRATCHPAD_MAX_SQL_BYTES,
            capability_profile: CapabilityProfile::ReadOnly,
            print_tools: false,
            print_tool_schema: false,
            command: None,
        }
    }

    #[test]
    fn sanitize_base_url_trims_trailing_slash() {
        let url = sanitize_base_url("https://analyticsdata.googleapis.com/")
            .expect("url should be valid");
        assert_eq!(url, "https://analyticsdata.googleapis.com");
    }

    #[test]
    fn sanitize_base_url_rejects_missing_scheme() {
        let err = sanitize_base_url("analyticsdata.googleapis.com")
            .expect_err("missing scheme should fail");
        assert!(err.to_string().contains("must start with"));
    }

    #[test]
    fn sanitize_base_url_rejects_cleartext_http() {
        let err = sanitize_base_url("http://analyticsdata.googleapis.com")
            .expect_err("cleartext upstream URL should fail");
        assert!(err.to_string().contains("https://"));
    }

    #[test]
    fn from_cli_rejects_zero_scratchpad_sessions() {
        let mut cli = sample_cli();
        cli.scratchpad_max_sessions = 0;
        let err = Settings::from_cli(cli).expect_err("zero scratchpad sessions should fail");
        assert!(
            err.to_string()
                .contains("scratchpad max sessions must be greater than zero")
        );
    }

    #[test]
    fn from_cli_populates_scratchpad_settings() {
        let settings = Settings::from_cli(sample_cli()).expect("sample settings should parse");
        assert_eq!(settings.scratchpad_session_ttl, Duration::from_secs(900));
        assert_eq!(settings.scratchpad_max_sessions, 64);
        assert_eq!(settings.scratchpad_max_tables_per_session, 32);
        assert_eq!(settings.scratchpad_max_rows_per_session, 1_000_000);
        assert_eq!(settings.scratchpad_max_memory_mb, 256);
        assert_eq!(
            settings.scratchpad_query_timeout,
            Duration::from_millis(15_000)
        );
        assert_eq!(settings.scratchpad_max_sql_bytes, 65_536);
        assert_eq!(settings.capability_profile, CapabilityProfile::ReadOnly);
    }

    #[test]
    fn capability_profile_read_only_blocks_scratchpad_tools() {
        assert!(CapabilityProfile::ReadOnly.allows_tool("run_report"));
        assert!(!CapabilityProfile::ReadOnly.allows_tool("scratchpad_query"));
        assert!(CapabilityProfile::Scratchpad.allows_tool("scratchpad_query"));
    }

    #[test]
    fn from_cli_rejects_partial_oauth_refresh_configuration() {
        let mut cli = sample_cli();
        cli.oauth_client_secret_json = Some("/tmp/client_secret.json".to_string());
        cli.oauth_refresh_token = None;
        let err = Settings::from_cli(cli).expect_err("partial oauth refresh config should fail");
        assert!(err.to_string().contains("must be set together"));
    }

    #[test]
    fn sanitize_header_name_lowercases_and_rejects_invalid_values() {
        let sanitized = sanitize_header_name("Authorization").expect("header name should sanitize");
        assert_eq!(sanitized, "authorization");

        let err = sanitize_header_name("authorization value")
            .expect_err("spaces should be rejected in header names");
        assert!(err.to_string().contains("ASCII letters, digits, and '-'"));
    }

    #[test]
    fn from_cli_keeps_request_header_upstream_token_source() {
        let mut cli = sample_cli();
        cli.upstream_token_source = UpstreamTokenSource::RequestHeader;
        cli.upstream_token_header = "Authorization".to_string();
        let settings = Settings::from_cli(cli).expect("settings should parse");
        assert_eq!(
            settings.upstream_token_source,
            UpstreamTokenSource::RequestHeader
        );
        assert_eq!(settings.upstream_token_header, "authorization");
    }

    #[test]
    fn from_cli_keeps_request_header_or_config_upstream_token_source() {
        let mut cli = sample_cli();
        cli.upstream_token_source = UpstreamTokenSource::RequestHeaderOrConfig;
        cli.upstream_token_header = "Authorization".to_string();
        let settings = Settings::from_cli(cli).expect("settings should parse");
        assert_eq!(
            settings.upstream_token_source,
            UpstreamTokenSource::RequestHeaderOrConfig
        );
        assert_eq!(settings.upstream_token_header, "authorization");
        assert!(settings.upstream_token_source.uses_request_header());
    }
}
