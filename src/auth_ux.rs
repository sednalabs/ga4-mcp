//! Human-facing authentication helpers for the CLI and setup tools.

use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::{env, fs};

use anyhow::{Context, Result, anyhow};
use mcp_toolkit_auth::provider_auth::{
    GOOGLE_CLOUD_PLATFORM_SCOPE, GoogleProviderAuthConfig, GoogleProviderAuthFailureKind,
    classify_google_provider_auth_error, format_provider_auth_command, google_adc_login_scopes,
    google_adc_quota_project_command,
};
use mcp_toolkit_auth::upstream_oauth::{
    BrowserLaunchMode, LoopbackOAuthOptions, google_oauth_client_from_file,
    save_google_authorized_user_adc, start_loopback_authorization,
};
use serde::Serialize;

use crate::config::{
    AuthDoctorArgs, AuthLoginArgs, AuthStatusCliArgs, AuthSubcommand, DEFAULT_ANALYTICS_SCOPE,
    Settings, UpstreamTokenSource, conventional_adc_credentials_path,
    conventional_cloudsdk_config_dir, server_adc_credentials_path, server_cloudsdk_config_dir,
};
use crate::contract::redact_secret_text;
use crate::ga_client::{AnalyticsApiClient, AuthSource};

const ANALYTICS_API_NAME: &str = "Google Analytics API";
const ANALYTICS_ADMIN_API_SERVICE: &str = "analyticsadmin.googleapis.com";
const ANALYTICS_DATA_API_SERVICE: &str = "analyticsdata.googleapis.com";

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
    quota_project: QuotaProjectStatus,
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
    oauth_client_secret_json: bool,
    oauth_refresh_token: bool,
    quota_project: bool,
    shared_adc: bool,
    cloudsdk_config: bool,
}

#[derive(Debug, Clone, Serialize)]
struct QuotaProjectStatus {
    configured: bool,
    value: Option<String>,
    source: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum VerificationReport {
    NotChecked,
    Ok,
    RequestHeaderRequired { header: String },
    Failed { error: String, hint: Option<String> },
    ConfigError { error: String },
}

/// Runs the requested auth UX command.
pub async fn run_auth_command(settings: &Settings, command: &AuthSubcommand) -> Result<()> {
    match command {
        AuthSubcommand::Login(args) => run_login(settings, args).await,
        AuthSubcommand::Command(args) => print_login_command(settings, args),
        AuthSubcommand::Status(args) => print_status(settings, args).await,
        AuthSubcommand::Doctor(args) => print_doctor(settings, args).await,
    }
}

pub fn login_command_for_scope(
    scope: &str,
    headless: bool,
    client_id_file: Option<&Path>,
) -> String {
    login_command_for_scope_with_cloudsdk(
        scope,
        headless,
        client_id_file,
        server_cloudsdk_config_dir().as_deref(),
    )
}

pub fn login_command_for_scope_with_cloudsdk(
    scope: &str,
    headless: bool,
    client_id_file: Option<&Path>,
    cloudsdk_config: Option<&Path>,
) -> String {
    shell_join_with_cloudsdk_config(
        &gcloud_login_args(scope, headless, client_id_file, None),
        cloudsdk_config,
    )
}

pub fn quota_project_command_with_cloudsdk(
    project: &str,
    cloudsdk_config: Option<&Path>,
) -> String {
    shell_join_with_cloudsdk_config(&gcloud_set_quota_project_command(project), cloudsdk_config)
}

pub fn auth_login_cli_command(
    scope: &str,
    headless: bool,
    client_id_file: Option<&Path>,
    quota_project: Option<&str>,
    shared_adc: bool,
    account: Option<&str>,
    callback_port: Option<u16>,
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
    if let Some(account) = account.filter(|account| !account.trim().is_empty()) {
        args.push("--account".to_string());
        args.push(account.trim().to_string());
    }
    if let Some(port) = callback_port {
        args.push("--callback-port".to_string());
        args.push(port.to_string());
    }
    if let Some(project) = quota_project.filter(|project| !project.trim().is_empty()) {
        args.push("--quota-project".to_string());
        args.push(project.to_string());
    }
    if shared_adc {
        args.push("--shared-adc".to_string());
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
    let command_args = gcloud_login_args(
        &scope,
        args.headless,
        args.client_id_file.as_deref(),
        args.account.as_deref(),
    );
    let cloudsdk_config = require_login_cloudsdk_config(args.shared_adc)?;
    let rendered = shell_join_with_cloudsdk_config(&command_args, cloudsdk_config.as_deref());
    let quota_project = args
        .quota_project
        .clone()
        .or_else(|| settings.quota_project.clone());

    if use_direct_browser_oauth(args) {
        return run_direct_browser_oauth_login(settings, args, &scope, quota_project).await;
    }

    if args.dry_run {
        println!("{rendered}");
        if let Some(project) = quota_project.as_deref() {
            println!(
                "{}",
                shell_join_with_cloudsdk_config(
                    &gcloud_set_quota_project_command(project),
                    cloudsdk_config.as_deref(),
                )
            );
        }
        return Ok(());
    }

    let detection = detect_credentials(args.shared_adc);
    if !detection.gcloud_available {
        return Err(anyhow!(
            "gcloud was not found on PATH. Install the Google Cloud SDK, then run:\n  {rendered}\n\nUnattended deployments can use GOOGLE_APPLICATION_CREDENTIALS or GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON plus GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN instead."
        ));
    }

    println!("Starting Google Analytics login using Application Default Credentials.");
    println!("Scope: {scope}");
    println!(
        "Credential file: {}",
        adc_login_target_description(args.shared_adc)
    );
    println!("Command: {rendered}");
    println!(
        "Tip: ADC login includes the required cloud-platform scope because gcloud requires it for local ADC user credentials."
    );
    println!(
        "Tip: use --quota-project PROJECT_ID so verification can send x-goog-user-project when Google requires a quota project for local ADC."
    );
    if !args.shared_adc {
        println!(
            "Tip: this login uses a GA4-specific ADC file so other Google MCPs keep their own tokens and scopes."
        );
    }
    if args.client_id_file.is_none() {
        println!(
            "Tip: if Google blocks the bundled gcloud OAuth app for Analytics scopes, create a Desktop OAuth client and rerun with `--client-id-file /path/to/client_id.json`."
        );
    }
    if args.headless {
        println!(
            "Headless mode requested; follow the URL and paste the browser result if gcloud asks."
        );
    }

    if let Some(dir) = cloudsdk_config.as_deref() {
        fs::create_dir_all(dir).context("failed to create server-specific gcloud config dir")?;
    }

    let mut login = gcloud_command();
    login.args(&command_args[1..]);
    if let Some(dir) = cloudsdk_config.as_deref() {
        login.env("CLOUDSDK_CONFIG", dir);
    }
    let status = login.status().context("failed to run gcloud")?;
    if !status.success() {
        let mut message = format!("gcloud login failed with status {status}");
        if args.client_id_file.is_none() {
            message.push_str(
                ". If Google blocked the bundled gcloud OAuth app for Analytics scopes, rerun with `--client-id-file /path/to/client_id.json` from a Desktop OAuth client.",
            );
        }
        return Err(anyhow!(message));
    }

    if let Some(project) = quota_project.as_deref() {
        let quota_project_command = gcloud_set_quota_project_command(project);
        println!(
            "Setting ADC quota project: {}",
            shell_join_with_cloudsdk_config(&quota_project_command, cloudsdk_config.as_deref())
        );
        let mut quota = gcloud_command();
        quota.args(&quota_project_command[1..]);
        if let Some(dir) = cloudsdk_config.as_deref() {
            quota.env("CLOUDSDK_CONFIG", dir);
        }
        let status = quota
            .status()
            .context("failed to run gcloud ADC quota-project command")?;
        if !status.success() {
            return Err(anyhow!(
                "gcloud set-quota-project failed with status {status}"
            ));
        }
    }

    println!("Google login completed.");
    if args.no_verify {
        if settings.upstream_token_source == UpstreamTokenSource::RequestHeader {
            println!(
                "Verification skipped. Call ga4_auth_status with verify_token=true while sending {} when ready.",
                settings.upstream_token_header
            );
        } else {
            println!("Verification skipped. Run `ga4-mcp auth status --verify-token` when ready.");
        }
        for step in post_login_runtime_steps(settings, &scope) {
            println!("{step}");
        }
        return Ok(());
    }

    let mut verify_settings = settings.clone();
    verify_settings.analytics_scope = scope.clone();
    verify_settings.quota_project = quota_project;
    let mut report = build_auth_report(&verify_settings, true).await;
    add_post_login_runtime_steps(settings, &scope, &mut report);
    print_human_report(&report, true);
    if verification_blocks_login_completion(&report) {
        Err(anyhow!(
            "login completed, but Google Analytics verification did not pass"
        ))
    } else {
        Ok(())
    }
}

fn print_login_command(settings: &Settings, args: &crate::config::AuthCommandArgs) -> Result<()> {
    let scope = login_scope(settings);
    let cloudsdk_config = require_login_cloudsdk_config(args.shared_adc)?;
    if args.client_id_file.is_some() && !args.shared_adc {
        println!(
            "{}",
            auth_login_cli_command(
                scope,
                args.headless,
                args.client_id_file.as_deref(),
                args.quota_project
                    .as_deref()
                    .or(settings.quota_project.as_deref()),
                args.shared_adc,
                args.account.as_deref(),
                args.callback_port,
            )
        );
        return Ok(());
    }
    let command = gcloud_login_args(
        scope,
        args.headless,
        args.client_id_file.as_deref(),
        args.account.as_deref(),
    );
    println!(
        "{}",
        shell_join_with_cloudsdk_config(&command, cloudsdk_config.as_deref())
    );
    if let Some(project) = args
        .quota_project
        .as_deref()
        .or(settings.quota_project.as_deref())
    {
        println!(
            "{}",
            shell_join_with_cloudsdk_config(
                &gcloud_set_quota_project_command(project),
                cloudsdk_config.as_deref(),
            )
        );
    }
    Ok(())
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
    let detection = detect_credentials(settings.shared_adc);
    let client = AnalyticsApiClient::from_settings(settings).await;
    let mut detected_auth_source = None;
    let mut detected_quota_project = settings.quota_project.clone();
    let verification = match client {
        Ok(client) => {
            detected_auth_source = Some(client.auth_source());
            detected_quota_project = client.quota_project().map(str::to_string);
            if verify_token {
                let result = if settings.upstream_token_source == UpstreamTokenSource::RequestHeader
                {
                    Ok(VerificationReport::RequestHeaderRequired {
                        header: settings.upstream_token_header.clone(),
                    })
                } else {
                    client.verify_token().await.map(|()| VerificationReport::Ok)
                };
                match result {
                    Ok(report) => report,
                    Err(err) => VerificationReport::Failed {
                        error: redact_secret_text(&err.to_string()),
                        hint: err.hint().map(str::to_string),
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
    let quota_project = effective_quota_project(settings, detected_quota_project.as_deref());
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
        quota_project,
        detected: detection,
        verification,
        ready,
        next_steps,
    }
}

fn effective_quota_project(
    settings: &Settings,
    detected_quota_project: Option<&str>,
) -> QuotaProjectStatus {
    if let Some(project) = settings.quota_project.as_deref() {
        return QuotaProjectStatus {
            configured: true,
            value: Some(project.to_string()),
            source: Some("GOOGLE_ANALYTICS_MCP_QUOTA_PROJECT_or_cli".to_string()),
        };
    }
    if let Some(project) = detected_quota_project {
        return QuotaProjectStatus {
            configured: true,
            value: Some(project.to_string()),
            source: Some("adc_credentials_file".to_string()),
        };
    }
    QuotaProjectStatus {
        configured: false,
        value: None,
        source: None,
    }
}

fn print_human_report(report: &AuthReport, doctor: bool) {
    println!("Google Analytics MCP auth");
    println!("Profile: {}", report.capability_profile);
    println!("Scope: {}", report.requested_scope);
    if doctor {
        println!("Upstream token source: {}", report.upstream_token_source);
        println!("Request token header: {}", report.upstream_token_header);
    }
    match (
        report.auth_source.as_deref(),
        report.auth_source_candidate.as_deref(),
    ) {
        (Some(source), _) => println!("Credential source: {source}"),
        (None, Some(candidate)) => {
            println!("Credential source: not verified (candidate: {candidate})")
        }
        (None, None) => println!("Credential source: not configured"),
    }
    println!("Config valid: {}", yes_no(report.config_valid));
    println!(
        "Credential material detected: {}",
        yes_no(report.credential_material_detected)
    );
    println!(
        "Quota project: {}",
        match (&report.quota_project.value, &report.quota_project.source) {
            (Some(project), Some(source)) => format!("{project} ({source})"),
            _ => "not configured".to_string(),
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
        "Env credentials: GOOGLE_APPLICATION_CREDENTIALS={}, oauth-client={}, oauth-refresh-token={}, quota-project={}, shared-adc={}",
        yes_no(report.detected.env.google_application_credentials),
        yes_no(report.detected.env.oauth_client_secret_json),
        yes_no(report.detected.env.oauth_refresh_token),
        yes_no(report.detected.env.quota_project),
        yes_no(report.detected.env.shared_adc),
    );
    match &report.verification {
        VerificationReport::NotChecked => println!("Verification: not checked"),
        VerificationReport::Ok => println!("Verification: ok"),
        VerificationReport::RequestHeaderRequired { header } => {
            println!("Verification: requires request header");
            println!(
                "Hint: standalone CLI cannot verify request_header mode; call ga4_auth_status with verify_token=true while sending {header}, or switch local loopback use to request_header_or_config and rerun `ga4-mcp auth status --verify-token`."
            );
        }
        VerificationReport::Failed { error, hint } => {
            println!("Verification: failed");
            println!("Error: {error}");
            if let Some(hint) = hint {
                println!("Hint: {hint}");
            }
        }
        VerificationReport::ConfigError { error } => {
            println!("Configuration: invalid");
            println!("Error: {error}");
        }
    }
    println!(
        "Ready: {}",
        if matches!(
            &report.verification,
            VerificationReport::NotChecked | VerificationReport::RequestHeaderRequired { .. }
        ) {
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

fn detect_credentials(shared_adc: bool) -> CredentialDetection {
    let gcloud_version = gcloud_version_summary();
    let adc_path = if shared_adc {
        conventional_adc_credentials_path()
    } else {
        server_adc_credentials_path()
    };
    let adc_present = adc_path.as_deref().is_some_and(adc_file_present);
    CredentialDetection {
        gcloud_available: gcloud_version.is_some(),
        gcloud_version,
        adc_file: FilePresence {
            present: adc_present,
            path: adc_path.map(|path| path.display().to_string()),
        },
        env: EnvPresence {
            google_application_credentials: env_present("GOOGLE_APPLICATION_CREDENTIALS"),
            oauth_client_secret_json: env_present("GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON"),
            oauth_refresh_token: env_present("GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN"),
            quota_project: env_present("GOOGLE_ANALYTICS_MCP_QUOTA_PROJECT"),
            shared_adc,
            cloudsdk_config: env_present("CLOUDSDK_CONFIG"),
        },
    }
}

pub fn local_credential_material_detected() -> bool {
    credential_material_detected(&detect_credentials(env_bool_true(
        "GOOGLE_ANALYTICS_MCP_SHARED_ADC",
    )))
}

fn credential_material_detected(detection: &CredentialDetection) -> bool {
    detection.adc_file.present
        || detection.env.google_application_credentials
        || (detection.env.oauth_client_secret_json && detection.env.oauth_refresh_token)
}

fn settings_credential_material_detected(settings: &Settings) -> bool {
    settings.oauth_client_secret_json.is_some() && settings.oauth_refresh_token.is_some()
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
    detection.env.google_application_credentials
        || (detection.env.oauth_client_secret_json && detection.env.oauth_refresh_token)
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

fn verification_blocks_login_completion(report: &AuthReport) -> bool {
    matches!(
        &report.verification,
        VerificationReport::Failed { .. } | VerificationReport::ConfigError { .. }
    )
}

fn use_direct_browser_oauth(args: &AuthLoginArgs) -> bool {
    args.client_id_file.is_some() && !args.shared_adc
}

async fn run_direct_browser_oauth_login(
    settings: &Settings,
    args: &AuthLoginArgs,
    scope: &str,
    quota_project: Option<String>,
) -> Result<()> {
    let client_id_file = args
        .client_id_file
        .as_deref()
        .expect("direct browser OAuth requires client_id_file");
    let adc_path = server_adc_credentials_path().ok_or_else(|| {
        anyhow!(
            "failed to determine the GA4-specific ADC path; set HOME/XDG_CONFIG_HOME on Unix or APPDATA on Windows, or pass --shared-adc to intentionally use conventional shared ADC"
        )
    })?;
    let rendered = auth_login_cli_command(
        scope,
        args.headless,
        Some(client_id_file),
        quota_project.as_deref(),
        false,
        args.account.as_deref(),
        args.callback_port,
    );
    if args.dry_run {
        println!("{rendered}");
        return Ok(());
    }

    let client = google_oauth_client_from_file(client_id_file).with_context(|| {
        format!(
            "failed to load Google OAuth client file at {}",
            client_id_file.display()
        )
    })?;
    let mut options = LoopbackOAuthOptions::google_reauth();
    options.bind_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    options.port = args.callback_port;
    options.browser = if args.headless {
        BrowserLaunchMode::Disabled
    } else {
        BrowserLaunchMode::BestEffortSystem
    };
    if let Some(account) = args
        .account
        .as_deref()
        .map(str::trim)
        .filter(|account| !account.is_empty())
    {
        options
            .extra_authorization_params
            .push(("login_hint".to_string(), account.to_string()));
    }

    println!("Starting Google Analytics browser OAuth login.");
    println!("Scope: {scope}");
    println!(
        "Credential file: server-specific ADC ({})",
        adc_path.display()
    );
    println!("OAuth client file: {}", client_id_file.display());
    if let Some(project) = quota_project.as_deref() {
        println!("Quota project: {project}");
    } else {
        println!("Quota project: not configured");
    }
    println!(
        "Tip: this flow uses your OAuth client directly, so it avoids the bundled gcloud OAuth app that Google can block for Analytics scopes."
    );
    println!(
        "Tip: this login uses a GA4-specific ADC file so other Google MCPs keep their own tokens and scopes."
    );

    let pending = start_loopback_authorization(client.clone(), split_scopes(scope), options)
        .await
        .context("failed to start Google browser OAuth")?;
    println!("Authorization URL:");
    println!("{}", pending.authorization_url());
    println!("Redirect URI: {}", pending.redirect_uri());

    let token_set = if args.headless {
        println!(
            "Headless mode requested. Open the URL on a trusted machine. When Google redirects to the loopback callback, copy the full browser address-bar URL and paste it below."
        );
        let callback_url = read_secret_line("Paste redirected callback URL, then press Enter: ")?;
        pending
            .finish_with_callback_url(callback_url.trim())
            .await
            .context("failed to finish pasted Google OAuth callback")?
    } else {
        if !pending
            .launch_browser()
            .context("failed to launch browser for Google OAuth")?
        {
            println!("Open the Authorization URL in your browser.");
        }
        pending
            .finish()
            .await
            .context("failed to finish Google OAuth callback")?
    };

    save_google_authorized_user_adc(&adc_path, &client, token_set, quota_project.as_deref())
        .with_context(|| {
            format!(
                "failed to write GA4-specific ADC file at {}",
                adc_path.display()
            )
        })?;
    println!("Google login completed.");

    if args.no_verify {
        if settings.upstream_token_source == UpstreamTokenSource::RequestHeader {
            println!(
                "Verification skipped. Call ga4_auth_status with verify_token=true while sending {} when ready.",
                settings.upstream_token_header
            );
        } else {
            println!("Verification skipped. Run `ga4-mcp auth status --verify-token` when ready.");
        }
        for step in post_login_runtime_steps(settings, scope) {
            println!("{step}");
        }
        return Ok(());
    }

    let mut verify_settings = settings.clone();
    verify_settings.analytics_scope = scope.to_string();
    verify_settings.quota_project = quota_project;
    let mut report = build_auth_report(&verify_settings, true).await;
    add_post_login_runtime_steps(settings, scope, &mut report);
    print_human_report(&report, true);
    if verification_blocks_login_completion(&report) {
        Err(anyhow!(
            "login completed, but Google Analytics verification did not pass"
        ))
    } else {
        Ok(())
    }
}

fn read_secret_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush().context("failed to flush prompt")?;
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .context("failed to read OAuth callback URL from stdin")?;
    if value.trim().is_empty() {
        return Err(anyhow!("OAuth callback URL was empty"));
    }
    Ok(value)
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
        let fallback = local_fallback_step();
        if !steps.iter().any(|step| step == &fallback) {
            steps.push(fallback);
        }
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
    "For the lowest-friction local/loopback service, set GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header_or_config and GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=x-google-access-token; keep request_header for hosted per-user services where every client supplies a Google token.".to_string()
}

fn request_header_verification_step(header: &str) -> String {
    format!(
        "Call ga4_auth_status with verify_token=true while sending {header}. For local or loopback fallback, switch GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header_or_config and rerun `ga4-mcp auth status --verify-token`."
    )
}

fn next_steps(
    settings: &Settings,
    detection: &CredentialDetection,
    verification: &VerificationReport,
    verify_token: bool,
) -> Vec<String> {
    let auth_config = google_provider_auth_config(DEFAULT_ANALYTICS_SCOPE);
    let setup_plan = auth_config.adc_setup_plan();
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
                    steps.push(request_header_verification_step(
                        &settings.upstream_token_header,
                    ));
                } else {
                    steps.push(
                        "Run `ga4-mcp auth status --verify-token` to prove Google API access."
                            .to_string(),
                    );
                }
                steps.push(
                    "Then restart the MCP client and call get_account_summaries.".to_string(),
                );
                steps
            } else {
                let mut steps = vec![
                    format!("Run `{login_command}` for browser login."),
                    "Restart the MCP client and call get_account_summaries.".to_string(),
                ];
                if explicit_credential_config_detected(settings, detection) {
                    steps.insert(0, "Fix or clear explicit credential configuration before browser login; it takes precedence over Application Default Credentials.".to_string());
                }
                if missing_analytics_scope {
                    steps.insert(0, read_scope_step);
                }
                if settings.upstream_token_source == UpstreamTokenSource::RequestHeader {
                    steps.insert(
                        1,
                        request_header_verification_step(&settings.upstream_token_header),
                    );
                } else {
                    steps.insert(
                        1,
                        "Then run `ga4-mcp auth status --verify-token` to prove Google API access."
                            .to_string(),
                    );
                }
                steps
            }
        }
        VerificationReport::RequestHeaderRequired { header } => {
            let mut steps = Vec::new();
            if missing_analytics_scope {
                steps.push(read_scope_step);
            }
            steps.push(request_header_verification_step(header));
            steps.push(
                "Restart MCP clients that keep long-lived stdio or HTTP server processes."
                    .to_string(),
            );
            steps.push(
                "Call get_account_summaries after runtime auth is verified to discover accessible GA4 accounts and properties."
                    .to_string(),
            );
            steps
        }
        VerificationReport::Failed { error, .. } => {
            let mut steps = Vec::new();
            let diagnostic = classify_google_provider_auth_error(403, error, &auth_config);
            if missing_analytics_scope {
                steps.push(read_scope_step);
            }
            if explicit_credential_config_detected(settings, detection) {
                steps.push("Fix or clear explicit credential configuration before browser login; it takes precedence over Application Default Credentials.".to_string());
            }
            if matches!(
                diagnostic.kind,
                GoogleProviderAuthFailureKind::MissingQuotaProject
                    | GoogleProviderAuthFailureKind::ApiDisabled
            ) {
                steps.push(format!(
                    "Set an ADC quota project with `{}`.",
                    setup_plan.quota_project.shell
                ));
                if let Some(api_enable) = setup_plan.api_enable.as_ref() {
                    steps.push(format!(
                        "Enable the required Analytics APIs with `{}`.",
                        api_enable.shell
                    ));
                }
                steps.push("Then rerun `ga4-mcp auth status --verify-token`.".to_string());
            } else if diagnostic.kind == GoogleProviderAuthFailureKind::OAuthAppBlocked {
                steps.push(format!(
                    "Run `{login_command}` again with `--client-id-file /path/to/client_id.json` from a Desktop OAuth client."
                ));
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
            if settings.upstream_token_source == UpstreamTokenSource::RequestHeader {
                vec![request_header_verification_step(
                    &settings.upstream_token_header,
                )]
            } else {
                vec![
                    "Run `ga4-mcp auth status --verify-token` to prove Google API access."
                        .to_string(),
                ]
            }
        }
    }
}

fn scope_allows_analytics_read(scope: &str) -> bool {
    scope.split([',', ' ', '\n', '\t']).any(|item| {
        item == DEFAULT_ANALYTICS_SCOPE || item == "https://www.googleapis.com/auth/analytics"
    })
}

pub const GCLOUD_ADC_REQUIRED_SCOPE: &str = GOOGLE_CLOUD_PLATFORM_SCOPE;

pub fn adc_login_scopes(scope: &str) -> String {
    google_adc_login_scopes(
        scope
            .split([',', ' ', '\n', '\t'])
            .map(str::trim)
            .filter(|item| !item.is_empty()),
    )
    .join(",")
}

fn gcloud_login_args(
    scope: &str,
    headless: bool,
    client_id_file: Option<&Path>,
    account: Option<&str>,
) -> Vec<String> {
    let config = google_provider_auth_config(scope);
    let args = if let Some(path) = client_id_file {
        config.adc_login_command_with_client_id_file(headless, &path.display().to_string())
    } else {
        config.adc_login_command(headless)
    };
    with_gcloud_account_arg(args, account)
}

fn with_gcloud_account_arg(mut args: Vec<String>, account: Option<&str>) -> Vec<String> {
    let Some(account) = account.map(str::trim).filter(|account| !account.is_empty()) else {
        return args;
    };
    let insert_at = args
        .iter()
        .position(|part| part == "login")
        .map(|index| index + 1)
        .unwrap_or(args.len());
    args.insert(insert_at, account.to_string());
    args
}

fn shell_command(args: &[String]) -> String {
    format_provider_auth_command(args)
}

fn shell_join_with_cloudsdk_config(parts: &[String], cloudsdk_config: Option<&Path>) -> String {
    if let Some(dir) = cloudsdk_config {
        let dir_str = shell_command(&[dir.display().to_string()]);
        let command = shell_command(parts);
        if command.is_empty() {
            #[cfg(windows)]
            {
                format!("$env:CLOUDSDK_CONFIG={dir_str}")
            }
            #[cfg(not(windows))]
            {
                format!("CLOUDSDK_CONFIG={dir_str}")
            }
        } else {
            #[cfg(windows)]
            {
                format!("$env:CLOUDSDK_CONFIG={dir_str}; {command}")
            }
            #[cfg(not(windows))]
            {
                format!("CLOUDSDK_CONFIG={dir_str} {command}")
            }
        }
    } else {
        shell_command(parts)
    }
}

fn gcloud_set_quota_project_command(project: &str) -> Vec<String> {
    google_adc_quota_project_command(project)
}

fn login_cloudsdk_config_dir(shared_adc: bool) -> Option<PathBuf> {
    if shared_adc {
        None
    } else {
        server_cloudsdk_config_dir()
    }
}

fn require_login_cloudsdk_config(shared_adc: bool) -> Result<Option<PathBuf>> {
    let cloudsdk_config = login_cloudsdk_config_dir(shared_adc);
    if !shared_adc && cloudsdk_config.is_none() {
        return Err(anyhow!(
            "failed to determine the server-specific gcloud config directory; set HOME/XDG_CONFIG_HOME on Unix or APPDATA on Windows, or pass --shared-adc to intentionally use conventional shared ADC"
        ));
    }
    Ok(cloudsdk_config)
}

fn adc_login_target_description(shared_adc: bool) -> String {
    if shared_adc {
        return conventional_adc_credentials_path()
            .map(|path| format!("shared gcloud ADC ({})", path.display()))
            .unwrap_or_else(|| "shared gcloud ADC".to_string());
    }
    server_adc_credentials_path()
        .map(|path| format!("server-specific ADC ({})", path.display()))
        .unwrap_or_else(|| "server-specific ADC".to_string())
}

pub(crate) fn google_provider_auth_config(scope: &str) -> GoogleProviderAuthConfig {
    GoogleProviderAuthConfig::new(ANALYTICS_API_NAME, split_scopes(scope))
        .with_api_service_names([ANALYTICS_ADMIN_API_SERVICE, ANALYTICS_DATA_API_SERVICE])
}

fn split_scopes(scope: &str) -> Vec<String> {
    scope
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn gcloud_version_summary() -> Option<String> {
    let output = gcloud_command().arg("--version").output().ok()?;
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

fn gcloud_command() -> ProcessCommand {
    if cfg!(windows) {
        let mut cmd = ProcessCommand::new("cmd");
        cmd.arg("/C").arg("gcloud");
        cmd
    } else {
        ProcessCommand::new("gcloud")
    }
}

fn adc_file_present(path: &Path) -> bool {
    let root = server_adc_credentials_path()
        .filter(|candidate| candidate == path)
        .and_then(|_| server_cloudsdk_config_dir())
        .or_else(|| {
            conventional_adc_credentials_path()
                .filter(|candidate| candidate == path)
                .and_then(|_| conventional_cloudsdk_config_dir())
        });
    root.as_deref()
        .is_some_and(|root| file_is_under_root(path, root))
}

fn file_is_under_root(path: &Path, root: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    path.starts_with(root) && path.is_file()
}

fn env_present(name: &str) -> bool {
    env::var_os(name)
        .map(|value| !value.is_empty())
        .unwrap_or(false)
}

fn env_bool_true(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
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
            login_command_for_scope_with_cloudsdk(login_scope(&settings), false, None, None),
            "gcloud auth application-default login --scopes=https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/analytics.readonly"
        );
    }

    #[test]
    fn login_command_prefixes_server_specific_cloudsdk_config() {
        let command = login_command_for_scope_with_cloudsdk(
            DEFAULT_ANALYTICS_SCOPE,
            true,
            None,
            Some(Path::new("/tmp/ga4 adc")),
        );

        #[cfg(windows)]
        assert!(command.starts_with("$env:CLOUDSDK_CONFIG="));

        #[cfg(not(windows))]
        assert!(command.starts_with("CLOUDSDK_CONFIG='/tmp/ga4 adc' gcloud auth"));
        assert!(command.contains("analytics.readonly"));
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
            "https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/analytics.readonly"
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
            None,
            false,
            None,
            None,
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
            None,
            false,
            None,
            None,
        );

        assert_eq!(
            command,
            "ga4-mcp --analytics-scope 'https://www.googleapis.com/auth/analytics.readonly extra' auth login"
        );
    }

    #[test]
    fn auth_login_cli_command_includes_quota_project() {
        let command = auth_login_cli_command(
            DEFAULT_ANALYTICS_SCOPE,
            true,
            None,
            Some("itwire-project"),
            false,
            None,
            None,
        );

        assert_eq!(
            command,
            "ga4-mcp auth login --headless --quota-project itwire-project"
        );
    }

    #[test]
    fn auth_login_cli_command_includes_account_and_callback_port() {
        let command = auth_login_cli_command(
            DEFAULT_ANALYTICS_SCOPE,
            true,
            Some(Path::new("/tmp/client.json")),
            Some("itwire-project"),
            false,
            Some("user@example.com"),
            Some(8091),
        );

        assert_eq!(
            command,
            "ga4-mcp auth login --headless --client-id-file /tmp/client.json --account user@example.com --callback-port 8091 --quota-project itwire-project"
        );
    }

    #[test]
    fn gcloud_login_args_can_include_account_hint() {
        let command = shell_command(&gcloud_login_args(
            DEFAULT_ANALYTICS_SCOPE,
            true,
            None,
            Some("user@example.com"),
        ));

        assert!(command.starts_with("gcloud auth application-default login user@example.com "));
        assert!(command.contains("--no-launch-browser"));
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
                oauth_client_secret_json: false,
                oauth_refresh_token: false,
                quota_project: false,
                shared_adc: false,
                cloudsdk_config: false,
            },
        };

        let steps = next_steps(
            &test_settings(DEFAULT_ANALYTICS_SCOPE),
            &detection,
            &VerificationReport::Failed {
                error: "PERMISSION_DENIED: local Application Default Credentials requires a quota project; SERVICE_DISABLED".to_string(),
                hint: None,
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
    fn request_header_verification_steps_do_not_require_cli_token_check() {
        let mut settings = test_settings(DEFAULT_ANALYTICS_SCOPE);
        settings.upstream_token_source = UpstreamTokenSource::RequestHeader;

        let steps = next_steps(
            &settings,
            &CredentialDetection {
                gcloud_available: true,
                gcloud_version: Some("Google Cloud SDK 999.0.0".to_string()),
                adc_file: FilePresence {
                    path: Some("/tmp/adc.json".to_string()),
                    present: true,
                },
                env: EnvPresence {
                    google_application_credentials: false,
                    oauth_client_secret_json: false,
                    oauth_refresh_token: false,
                    quota_project: false,
                    shared_adc: false,
                    cloudsdk_config: false,
                },
            },
            &VerificationReport::RequestHeaderRequired {
                header: "x-google-access-token".to_string(),
            },
            true,
        );

        assert!(steps.iter().any(|step| step.contains("ga4_auth_status")));
        assert!(
            steps
                .iter()
                .all(|step| !step.contains("Run `ga4-mcp auth status --verify-token`"))
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
            upstream_token_header: "x-google-access-token".to_string(),
            quota_project: None,
            shared_adc: false,
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
