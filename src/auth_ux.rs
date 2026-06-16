//! Human-facing authentication helpers for the CLI and setup tools.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use crate::config::{
    AuthDoctorArgs, AuthLoginArgs, AuthStatusCliArgs, AuthSubcommand, DEFAULT_ANALYTICS_SCOPE,
    Settings, UpstreamTokenSource,
};
use crate::contract::redact_secret_text;
use crate::ga_client::{AnalyticsApiClient, AuthSource};

#[derive(Debug, Clone, Serialize)]
struct AuthReport {
    server: &'static str,
    capability_profile: String,
    requested_scope: String,
    upstream_token_source: String,
    upstream_token_header: String,
    auth_source: Option<String>,
    auth_source_candidate: Option<String>,
    config_valid: bool,
    credential_material_detected: bool,
    quota_project_configured: bool,
    detected: CredentialDetection,
    verification: VerificationReport,
    ready: bool,
    next_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CredentialDetection {
    gcloud_available: bool,
    gcloud_version: Option<String>,
    adc_file: FilePresence,
    env: EnvPresence,
}

#[derive(Debug, Clone, Serialize)]
struct FilePresence {
    path: Option<String>,
    present: bool,
}

#[derive(Debug, Clone, Serialize)]
struct EnvPresence {
    google_application_credentials: bool,
    google_application_credentials_file_present: bool,
    oauth_client_secret_json: bool,
    oauth_client_secret_json_file_present: bool,
    oauth_refresh_token: bool,
    cloudsdk_config: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum VerificationReport {
    NotChecked,
    Ok,
    Failed { error: String },
    ConfigError { error: String },
}

/// Runs the requested auth UX command.
pub async fn run_auth_command(settings: &Settings, command: &AuthSubcommand) -> Result<()> {
    match command {
        AuthSubcommand::Login(args) => run_login(settings, args).await,
        AuthSubcommand::Command(args) => {
            println!(
                "{}",
                shell_command(&gcloud_login_args(
                    login_scope(settings),
                    args.headless,
                    args.client_id_file.as_deref(),
                ))
            );
            Ok(())
        }
        AuthSubcommand::Status(args) => print_status(settings, args).await,
        AuthSubcommand::Doctor(args) => print_doctor(settings, args).await,
    }
}

pub fn login_command_for_scope(
    scope: &str,
    headless: bool,
    client_id_file: Option<&Path>,
) -> String {
    shell_command(&gcloud_login_args(scope, headless, client_id_file))
}

pub fn auth_login_cli_command(
    scope: &str,
    headless: bool,
    client_id_file: Option<&Path>,
) -> String {
    let mut args = vec!["ga4-mcp".to_string()];
    if scope != DEFAULT_ANALYTICS_SCOPE {
        args.push("--analytics-scope".to_string());
        args.push(scope.to_string());
    }
    args.push("auth".to_string());
    args.push("login".to_string());
    if headless {
        args.push("--headless".to_string());
    }
    if let Some(path) = client_id_file {
        args.push("--client-id-file".to_string());
        args.push(path.display().to_string());
    }
    shell_command(&args)
}

fn login_scope(settings: &Settings) -> &str {
    let ambient_scope = env::var("GOOGLE_ANALYTICS_MCP_SCOPE").ok();
    login_scope_from_env_hint(
        settings,
        ambient_scope.as_deref(),
        analytics_scope_arg_present(),
    )
}

fn login_scope_from_env_hint<'a>(
    settings: &'a Settings,
    ambient_scope: Option<&str>,
    explicit_scope_arg: bool,
) -> &'a str {
    if !explicit_scope_arg
        && ambient_scope.is_some_and(|scope| scope == settings.analytics_scope)
        && settings.analytics_scope != DEFAULT_ANALYTICS_SCOPE
        && !scope_allows_analytics_read(&settings.analytics_scope)
    {
        DEFAULT_ANALYTICS_SCOPE
    } else {
        settings.analytics_scope.as_str()
    }
}

fn analytics_scope_arg_present() -> bool {
    env::args_os().any(|arg| {
        arg == "--analytics-scope"
            || arg
                .to_str()
                .is_some_and(|value| value.starts_with("--analytics-scope="))
    })
}

async fn run_login(settings: &Settings, args: &AuthLoginArgs) -> Result<()> {
    let scope = login_scope(settings).to_string();
    let command_args = gcloud_login_args(&scope, args.headless, args.client_id_file.as_deref());
    let rendered = shell_command(&command_args);

    if args.dry_run {
        println!("{rendered}");
        return Ok(());
    }

    let detection = detect_credentials();
    if !detection.gcloud_available {
        return Err(anyhow!(
            "gcloud was not found on PATH. Install the Google Cloud SDK, then run:\n  {rendered}\n\nUnattended deployments can use GOOGLE_APPLICATION_CREDENTIALS or GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON plus GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN instead."
        ));
    }

    println!("Starting Google Analytics login using Application Default Credentials.");
    println!("Scope: {scope}");
    println!("Command: {rendered}");
    if args.client_id_file.is_none() {
        println!(
            "Tip: if Google rejects the Analytics scope, create a Desktop OAuth client and rerun with `--client-id-file /path/to/client_id.json`."
        );
    }
    if args.headless {
        println!(
            "Headless mode requested; follow the URL and paste the browser result if gcloud asks."
        );
    }

    let status = ProcessCommand::new("gcloud")
        .args(&command_args[1..])
        .status()
        .context("failed to run gcloud")?;
    if !status.success() {
        return Err(anyhow!("gcloud login failed with status {status}"));
    }

    println!("Google login completed.");
    if args.no_verify {
        println!("Verification skipped. Run `ga4-mcp auth status --verify-token` when ready.");
        for step in post_login_runtime_steps(settings, &scope) {
            println!("{step}");
        }
        return Ok(());
    }

    let mut verify_settings = settings.clone();
    verify_settings.analytics_scope = scope.clone();
    let mut report = build_auth_report(&verify_settings, true).await;
    add_post_login_runtime_steps(settings, &scope, &mut report);
    print_human_report(&report, true);
    if verification_ok(&report) {
        Ok(())
    } else {
        Err(anyhow!(
            "login completed, but Google Analytics verification did not pass"
        ))
    }
}

async fn print_status(settings: &Settings, args: &AuthStatusCliArgs) -> Result<()> {
    let report = build_auth_report(settings, args.verify_token).await;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human_report(&report, false);
    }
    Ok(())
}

async fn print_doctor(settings: &Settings, args: &AuthDoctorArgs) -> Result<()> {
    let report = build_auth_report(settings, args.verify_token).await;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human_report(&report, true);
    }
    Ok(())
}

async fn build_auth_report(settings: &Settings, verify_token: bool) -> AuthReport {
    let detection = detect_credentials();
    let client = AnalyticsApiClient::from_settings(settings).await;
    let mut detected_auth_source = None;
    let mut quota_project_configured = settings.quota_project.is_some();
    let verification = match client {
        Ok(client) => {
            detected_auth_source = Some(client.auth_source());
            quota_project_configured = client.quota_project_configured();
            if verify_token {
                let result = if settings.upstream_token_source == UpstreamTokenSource::RequestHeader
                {
                    client.verify_config_token().await
                } else {
                    client.verify_token().await
                };
                match result {
                    Ok(()) => VerificationReport::Ok,
                    Err(err) => VerificationReport::Failed {
                        error: redact_secret_text(&err.to_string()),
                    },
                }
            } else {
                VerificationReport::NotChecked
            }
        }
        Err(err) => VerificationReport::ConfigError {
            error: redact_secret_text(&err.to_string()),
        },
    };
    let credential_material_detected =
        credential_material_detected(&detection) || settings_credential_material_detected(settings);
    let explicit_credential_needs_repair =
        explicit_credential_config_needs_repair(settings, &detection);
    let auth_source = visible_auth_source(
        detected_auth_source,
        credential_material_detected,
        &verification,
    );
    let auth_source_candidate = detected_auth_source.map(|source| source.as_str().to_string());
    let config_valid = auth_source.is_some()
        && !explicit_credential_needs_repair
        && !matches!(verification, VerificationReport::ConfigError { .. });
    let ready = report_ready(settings, &verification, config_valid);
    let next_steps = next_steps(settings, &detection, &verification, verify_token);

    AuthReport {
        server: "ga4-mcp",
        capability_profile: settings.capability_profile.as_str().to_string(),
        requested_scope: settings.analytics_scope.clone(),
        upstream_token_source: settings.upstream_token_source.as_str().to_string(),
        upstream_token_header: settings.upstream_token_header.clone(),
        auth_source,
        auth_source_candidate,
        config_valid,
        credential_material_detected,
        quota_project_configured,
        detected: detection,
        verification,
        ready,
        next_steps,
    }
}

fn print_human_report(report: &AuthReport, doctor: bool) {
    println!("Google Analytics MCP auth");
    println!("Capability profile: {}", report.capability_profile);
    println!("Scope: {}", report.requested_scope);
    println!("Upstream token source: {}", report.upstream_token_source);
    println!("Request token header: {}", report.upstream_token_header);
    match (
        report.auth_source.as_deref(),
        report.auth_source_candidate.as_deref(),
    ) {
        (Some(source), _) => println!("Server credential source: {source}"),
        (None, Some(candidate)) => {
            println!("Server credential source: not verified (candidate: {candidate})")
        }
        (None, None) => println!("Server credential source: not configured"),
    }
    println!("Config valid: {}", yes_no(report.config_valid));
    println!(
        "Credential material detected: {}",
        yes_no(report.credential_material_detected)
    );
    println!(
        "Quota project: {}",
        if report.quota_project_configured {
            "configured"
        } else {
            "not configured"
        }
    );
    println!(
        "gcloud: {}",
        report
            .detected
            .gcloud_version
            .as_deref()
            .unwrap_or(if report.detected.gcloud_available {
                "available"
            } else {
                "not found"
            })
    );
    match &report.detected.adc_file.path {
        Some(path) => println!(
            "ADC file: {} ({})",
            if report.detected.adc_file.present {
                "present"
            } else {
                "missing"
            },
            path
        ),
        None => println!("ADC file: unknown"),
    }
    println!(
        "Env credentials: GOOGLE_APPLICATION_CREDENTIALS={}, oauth-client={}, oauth-refresh-token={}",
        yes_no(report.detected.env.google_application_credentials),
        yes_no(report.detected.env.oauth_client_secret_json),
        yes_no(report.detected.env.oauth_refresh_token),
    );
    match &report.verification {
        VerificationReport::NotChecked => println!("Verification: not checked"),
        VerificationReport::Ok => println!("Verification: ok"),
        VerificationReport::Failed { error } => {
            println!("Verification: failed");
            println!("Error: {error}");
        }
        VerificationReport::ConfigError { error } => {
            println!("Configuration: invalid");
            println!("Error: {error}");
        }
    }
    println!(
        "Ready: {}",
        if matches!(report.verification, VerificationReport::NotChecked) {
            "not verified"
        } else {
            yes_no(report.ready)
        }
    );
    if doctor || !report.ready {
        println!("Next steps:");
        for step in &report.next_steps {
            println!("- {step}");
        }
    }
}

fn detect_credentials() -> CredentialDetection {
    let gcloud_version = gcloud_version_summary();
    let adc_path = adc_credentials_path();
    CredentialDetection {
        gcloud_available: gcloud_version.is_some(),
        gcloud_version,
        adc_file: FilePresence {
            present: adc_path.as_ref().map(|path| path.exists()).unwrap_or(false),
            path: adc_path.map(|path| path.display().to_string()),
        },
        env: EnvPresence {
            google_application_credentials: env_present("GOOGLE_APPLICATION_CREDENTIALS"),
            google_application_credentials_file_present: path_env_file_present(
                "GOOGLE_APPLICATION_CREDENTIALS",
            ),
            oauth_client_secret_json: env_present("GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON"),
            oauth_client_secret_json_file_present: path_env_file_present(
                "GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON",
            ),
            oauth_refresh_token: env_present("GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN"),
            cloudsdk_config: env_present("CLOUDSDK_CONFIG"),
        },
    }
}

pub fn local_credential_material_detected() -> bool {
    credential_material_detected(&detect_credentials())
}

fn credential_material_detected(detection: &CredentialDetection) -> bool {
    detection.adc_file.present
        || detection.env.google_application_credentials_file_present
        || (detection.env.oauth_client_secret_json_file_present
            && detection.env.oauth_refresh_token)
}

fn settings_credential_material_detected(settings: &Settings) -> bool {
    settings
        .oauth_client_secret_json
        .as_deref()
        .is_some_and(|path| Path::new(path).is_file())
        && settings
            .oauth_refresh_token
            .as_deref()
            .is_some_and(|token| !token.is_empty())
}

fn explicit_credential_env_detected(detection: &CredentialDetection) -> bool {
    detection.env.google_application_credentials
        || detection.env.oauth_client_secret_json
        || detection.env.oauth_refresh_token
}

fn explicit_credential_config_detected(
    settings: &Settings,
    detection: &CredentialDetection,
) -> bool {
    explicit_credential_env_detected(detection)
        || settings.oauth_client_secret_json.is_some()
        || settings.oauth_refresh_token.is_some()
}

fn explicit_credential_material_detected(
    settings: &Settings,
    detection: &CredentialDetection,
) -> bool {
    detection.env.google_application_credentials_file_present
        || (detection.env.oauth_client_secret_json_file_present
            && detection.env.oauth_refresh_token)
        || settings_credential_material_detected(settings)
}

fn explicit_credential_config_needs_repair(
    settings: &Settings,
    detection: &CredentialDetection,
) -> bool {
    explicit_credential_config_detected(settings, detection)
        && !explicit_credential_material_detected(settings, detection)
}

fn visible_auth_source(
    detected_auth_source: Option<AuthSource>,
    credential_material_detected: bool,
    verification: &VerificationReport,
) -> Option<String> {
    match detected_auth_source {
        Some(AuthSource::GoogleDefaultProviderChain)
            if !credential_material_detected && !matches!(verification, VerificationReport::Ok) =>
        {
            None
        }
        Some(source) => Some(source.as_str().to_string()),
        None => None,
    }
}

fn report_ready(
    settings: &Settings,
    verification: &VerificationReport,
    config_valid: bool,
) -> bool {
    config_valid
        && matches!(verification, VerificationReport::Ok)
        && scope_allows_analytics_read(&settings.analytics_scope)
}

fn verification_ok(report: &AuthReport) -> bool {
    matches!(report.verification, VerificationReport::Ok)
}

fn add_post_login_runtime_steps(
    original_settings: &Settings,
    login_scope: &str,
    report: &mut AuthReport,
) {
    let ambient_scope = env::var("GOOGLE_ANALYTICS_MCP_SCOPE").ok();
    let runtime_steps =
        post_login_runtime_steps_with_env(original_settings, login_scope, ambient_scope.as_deref());
    for step in runtime_steps.into_iter().rev() {
        report.next_steps.insert(0, step);
    }
    if runtime_scope_needs_repair(login_scope, ambient_scope.as_deref())
        || original_settings.upstream_token_source == UpstreamTokenSource::RequestHeader
    {
        report.ready = false;
    }
}

fn post_login_runtime_steps(original_settings: &Settings, login_scope: &str) -> Vec<String> {
    let ambient_scope = env::var("GOOGLE_ANALYTICS_MCP_SCOPE").ok();
    post_login_runtime_steps_with_env(original_settings, login_scope, ambient_scope.as_deref())
}

fn post_login_runtime_steps_with_env(
    original_settings: &Settings,
    login_scope: &str,
    ambient_scope: Option<&str>,
) -> Vec<String> {
    let mut steps = Vec::new();
    if runtime_scope_needs_repair(login_scope, ambient_scope) {
        steps.push(runtime_scope_step(login_scope));
    }
    if original_settings.upstream_token_source == UpstreamTokenSource::RequestHeader {
        steps.push(local_fallback_step());
    }
    steps
}

fn runtime_scope_needs_repair(login_scope: &str, ambient_scope: Option<&str>) -> bool {
    ambient_scope
        .filter(|scope| !scope.is_empty())
        .is_some_and(|scope| scope != login_scope)
}

fn runtime_scope_step(scope: &str) -> String {
    format!(
        "Unset GOOGLE_ANALYTICS_MCP_SCOPE, set GOOGLE_ANALYTICS_MCP_SCOPE={scope}, or update any MCP launcher `--analytics-scope` argument before starting the MCP server; stale scope configuration overrides the login scope."
    )
}

fn local_fallback_step() -> String {
    "For the lowest-friction local/loopback service, set GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header_or_config and GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=authorization; keep request_header for hosted per-user services where every client supplies a Google token.".to_string()
}

fn next_steps(
    settings: &Settings,
    detection: &CredentialDetection,
    verification: &VerificationReport,
    verify_token: bool,
) -> Vec<String> {
    let missing_analytics_scope = !scope_allows_analytics_read(&settings.analytics_scope);
    let read_scope_step = format!(
        "Set GOOGLE_ANALYTICS_MCP_SCOPE={DEFAULT_ANALYTICS_SCOPE} or start the MCP server with `--analytics-scope {DEFAULT_ANALYTICS_SCOPE}`."
    );
    let login_command = if missing_analytics_scope {
        format!("ga4-mcp --analytics-scope {DEFAULT_ANALYTICS_SCOPE} auth login")
    } else {
        "ga4-mcp auth login".to_string()
    };

    match verification {
        VerificationReport::Ok => {
            let mut steps = Vec::new();
            if explicit_credential_config_needs_repair(settings, detection) {
                steps.push("Fix or clear explicit credential configuration before browser login; it takes precedence over Application Default Credentials.".to_string());
            }
            if missing_analytics_scope {
                steps.push(read_scope_step);
            }
            if settings.upstream_token_source == UpstreamTokenSource::RequestHeader {
                steps.push(local_fallback_step());
            }
            steps.push(
                "Restart MCP clients that keep long-lived stdio or HTTP server processes."
                    .to_string(),
            );
            steps.push(
                "Call get_account_summaries to list accessible GA4 accounts and properties."
                    .to_string(),
            );
            steps
        }
        VerificationReport::NotChecked if !verify_token => {
            if credential_material_detected(detection)
                || settings_credential_material_detected(settings)
            {
                let mut steps = Vec::new();
                if explicit_credential_config_needs_repair(settings, detection) {
                    steps.push("Fix or clear explicit credential configuration before browser login; it takes precedence over Application Default Credentials.".to_string());
                }
                if missing_analytics_scope {
                    steps.push(read_scope_step);
                }
                if settings.upstream_token_source == UpstreamTokenSource::RequestHeader {
                    steps.push(local_fallback_step());
                }
                steps.extend([
                    "Run `ga4-mcp auth status --verify-token` to prove Google API access."
                        .to_string(),
                    "Then restart the MCP client and call get_account_summaries.".to_string(),
                ]);
                steps
            } else {
                let mut steps = vec![
                    format!("Run `{login_command}` for browser login."),
                    "Then run `ga4-mcp auth status --verify-token` to prove Google API access."
                        .to_string(),
                    "Restart the MCP client and call get_account_summaries.".to_string(),
                ];
                if explicit_credential_config_detected(settings, detection) {
                    steps.insert(0, "Fix or clear explicit credential configuration before browser login; it takes precedence over Application Default Credentials.".to_string());
                }
                if missing_analytics_scope {
                    steps.insert(0, read_scope_step);
                }
                if settings.upstream_token_source == UpstreamTokenSource::RequestHeader {
                    steps.push(local_fallback_step());
                }
                steps
            }
        }
        VerificationReport::Failed { error } => {
            let mut steps = Vec::new();
            if missing_analytics_scope {
                steps.push(read_scope_step);
            }
            if explicit_credential_config_detected(settings, detection) {
                steps.push("Fix or clear explicit credential configuration before browser login; it takes precedence over Application Default Credentials.".to_string());
            }
            if verification_needs_quota_project(error) {
                steps.push("Set an ADC quota project with `gcloud auth application-default set-quota-project YOUR_PROJECT`; the project must have analyticsadmin.googleapis.com and analyticsdata.googleapis.com enabled and your account must be allowed to use it for quota.".to_string());
                steps.push("Then rerun `ga4-mcp auth status --verify-token`.".to_string());
            } else if !detection.gcloud_available {
                steps.push(
                    "Install the Google Cloud SDK, or configure GOOGLE_APPLICATION_CREDENTIALS."
                        .to_string(),
                );
            } else {
                steps.push(format!("Run `{login_command}` for browser login."));
                steps.push("If Google rejects the Analytics scope, rerun login with `--client-id-file /path/to/client_id.json` from a Desktop OAuth client.".to_string());
            }
            if settings.upstream_token_source == UpstreamTokenSource::RequestHeader {
                steps.push(local_fallback_step());
            }
            steps.push("For unattended deployments, prefer GOOGLE_APPLICATION_CREDENTIALS or OAuth refresh-token env configuration.".to_string());
            steps
        }
        VerificationReport::ConfigError { .. } => {
            let mut steps = Vec::new();
            if missing_analytics_scope {
                steps.push(read_scope_step);
            }
            if explicit_credential_config_detected(settings, detection) {
                steps.push("Fix or clear malformed explicit credential configuration before browser login; it takes precedence over Application Default Credentials.".to_string());
            }
            if !detection.gcloud_available {
                steps.push("Install the Google Cloud SDK for browser login, or configure a valid GOOGLE_APPLICATION_CREDENTIALS file.".to_string());
            } else {
                steps.push(format!("Run `{login_command}` after explicit credential configuration is fixed or cleared."));
            }
            if settings.upstream_token_source == UpstreamTokenSource::RequestHeader {
                steps.push(local_fallback_step());
            }
            steps.push("For unattended deployments, prefer GOOGLE_APPLICATION_CREDENTIALS or OAuth refresh-token env configuration.".to_string());
            steps
        }
        VerificationReport::NotChecked => {
            vec!["Run `ga4-mcp auth status --verify-token` to prove Google API access.".to_string()]
        }
    }
}

fn verification_needs_quota_project(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("quota project")
        || lower.contains("quota_project")
        || lower.contains("x-goog-user-project")
        || lower.contains("service_disabled")
}

fn scope_allows_analytics_read(scope: &str) -> bool {
    scope.split([',', ' ', '\n', '\t']).any(|item| {
        item == DEFAULT_ANALYTICS_SCOPE || item == "https://www.googleapis.com/auth/analytics"
    })
}

pub const GCLOUD_ADC_REQUIRED_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

pub fn adc_login_scopes(scope: &str) -> String {
    let mut scopes = Vec::new();
    let mut has_required_scope = false;

    for item in scope
        .split([',', ' ', '\n', '\t'])
        .filter(|item| !item.is_empty())
    {
        if item == GCLOUD_ADC_REQUIRED_SCOPE {
            has_required_scope = true;
        }
        if !scopes.iter().any(|existing| existing == &item) {
            scopes.push(item);
        }
    }

    if !has_required_scope {
        scopes.insert(0, GCLOUD_ADC_REQUIRED_SCOPE);
    }

    scopes.join(",")
}

fn gcloud_login_args(scope: &str, headless: bool, client_id_file: Option<&Path>) -> Vec<String> {
    let login_scopes = adc_login_scopes(scope);
    let mut args = vec![
        "gcloud".to_string(),
        "auth".to_string(),
        "application-default".to_string(),
        "login".to_string(),
        format!("--scopes={login_scopes}"),
    ];
    if headless {
        args.push("--no-launch-browser".to_string());
    }
    if let Some(path) = client_id_file {
        args.push("--client-id-file".to_string());
        args.push(path.display().to_string());
    }
    args
}

fn shell_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_word(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_word(arg: &str) -> String {
    if arg
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '='))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r#"'\''"#))
    }
}

fn gcloud_version_summary() -> Option<String> {
    let output = ProcessCommand::new("gcloud")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
}

fn adc_credentials_path() -> Option<PathBuf> {
    if let Some(config) = env::var_os("CLOUDSDK_CONFIG").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(config).join("application_default_credentials.json"));
    }
    if cfg!(windows) {
        return env::var_os("APPDATA")
            .filter(|value| !value.is_empty())
            .map(|appdata| {
                PathBuf::from(appdata)
                    .join("gcloud")
                    .join("application_default_credentials.json")
            });
    }
    home_dir().map(|home| {
        home.join(".config")
            .join("gcloud")
            .join("application_default_credentials.json")
    })
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
}

fn env_present(name: &str) -> bool {
    env::var_os(name)
        .map(|value| !value.is_empty())
        .unwrap_or(false)
}

fn path_env_file_present(name: &str) -> bool {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(|value| PathBuf::from(value).is_file())
        .unwrap_or(false)
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CapabilityProfile, CliCommand};

    #[test]
    fn login_command_defaults_to_read_only_scope() {
        let settings = test_settings(DEFAULT_ANALYTICS_SCOPE);

        assert_eq!(
            login_command_for_scope(login_scope(&settings), false, None),
            "gcloud auth application-default login '--scopes=https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/analytics.readonly'"
        );
    }

    #[test]
    fn adc_login_scopes_include_gcloud_required_scope_once() {
        assert_eq!(
            adc_login_scopes(DEFAULT_ANALYTICS_SCOPE),
            "https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/analytics.readonly"
        );
        assert_eq!(
            adc_login_scopes(
                "https://www.googleapis.com/auth/analytics.readonly,https://www.googleapis.com/auth/cloud-platform"
            ),
            "https://www.googleapis.com/auth/analytics.readonly,https://www.googleapis.com/auth/cloud-platform"
        );
    }

    #[test]
    fn login_scope_repairs_ambient_bad_scope() {
        let settings = test_settings("https://www.googleapis.com/auth/drive");

        assert_eq!(
            login_scope_from_env_hint(
                &settings,
                Some("https://www.googleapis.com/auth/drive"),
                false,
            ),
            DEFAULT_ANALYTICS_SCOPE
        );
    }

    #[test]
    fn login_scope_preserves_explicit_custom_scope() {
        let scope = "https://www.googleapis.com/auth/analytics.readonly https://www.googleapis.com/auth/userinfo.email";
        let settings = test_settings(scope);

        assert_eq!(
            login_scope_from_env_hint(&settings, Some(scope), true),
            scope
        );
    }

    #[test]
    fn auth_login_cli_command_includes_copyable_flags() {
        let command = auth_login_cli_command(
            DEFAULT_ANALYTICS_SCOPE,
            true,
            Some(Path::new("/tmp/client id.json")),
        );

        assert_eq!(
            command,
            "ga4-mcp auth login --headless --client-id-file '/tmp/client id.json'"
        );
    }

    #[test]
    fn auth_login_cli_command_includes_custom_scope_before_subcommand() {
        let command = auth_login_cli_command(
            "https://www.googleapis.com/auth/analytics.readonly extra",
            false,
            None,
        );

        assert_eq!(
            command,
            "ga4-mcp --analytics-scope 'https://www.googleapis.com/auth/analytics.readonly extra' auth login"
        );
    }

    #[test]
    fn next_steps_call_out_missing_quota_project() {
        let detection = CredentialDetection {
            gcloud_available: true,
            gcloud_version: Some("Google Cloud SDK 999.0.0".to_string()),
            adc_file: FilePresence {
                path: Some("/tmp/adc.json".to_string()),
                present: true,
            },
            env: EnvPresence {
                google_application_credentials: false,
                google_application_credentials_file_present: false,
                oauth_client_secret_json: false,
                oauth_client_secret_json_file_present: false,
                oauth_refresh_token: false,
                cloudsdk_config: false,
            },
        };

        let steps = next_steps(
            &test_settings(DEFAULT_ANALYTICS_SCOPE),
            &detection,
            &VerificationReport::Failed {
                error: "PERMISSION_DENIED: local Application Default Credentials requires a quota project; SERVICE_DISABLED".to_string(),
            },
            true,
        );

        assert!(
            steps
                .iter()
                .any(|step| step.contains("set-quota-project YOUR_PROJECT"))
        );
        assert!(!steps.iter().any(|step| step.contains("auth login")));
    }

    #[test]
    fn request_header_mode_suggests_local_fallback_after_login() {
        let mut settings = test_settings(DEFAULT_ANALYTICS_SCOPE);
        settings.upstream_token_source = UpstreamTokenSource::RequestHeader;

        let steps = post_login_runtime_steps(&settings, DEFAULT_ANALYTICS_SCOPE);

        assert!(
            steps
                .iter()
                .any(|step| step.contains("request_header_or_config"))
        );
    }

    #[test]
    fn adc_without_material_is_not_reported_as_configured_before_verification() {
        assert_eq!(
            visible_auth_source(
                Some(AuthSource::GoogleDefaultProviderChain),
                false,
                &VerificationReport::NotChecked,
            ),
            None
        );
    }

    fn test_settings(scope: &str) -> Settings {
        Settings {
            analytics_scope: scope.to_string(),
            admin_base_url: "https://analyticsadmin.googleapis.com".to_string(),
            data_base_url: "https://analyticsdata.googleapis.com".to_string(),
            http_timeout: std::time::Duration::from_secs(1),
            max_page_size: 200,
            max_pages: 20,
            user_agent: "test".to_string(),
            oauth_client_secret_json: None,
            oauth_refresh_token: None,
            upstream_token_source: UpstreamTokenSource::Config,
            upstream_token_header: "authorization".to_string(),
            quota_project: None,
            scratchpad_session_ttl: std::time::Duration::from_secs(900),
            scratchpad_max_sessions: 64,
            scratchpad_max_tables_per_session: 32,
            scratchpad_max_rows_per_session: 1_000_000,
            scratchpad_max_memory_mb: 256,
            scratchpad_query_timeout: std::time::Duration::from_secs(15),
            scratchpad_max_sql_bytes: 65_536,
            capability_profile: CapabilityProfile::ReadOnly,
            print_tools: false,
            print_tool_schema: false,
            command: Some(CliCommand::Serve),
        }
    }
}
