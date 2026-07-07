//! # HTTP Transport Configuration
//!
//! Environment-driven settings for GA4 streamable HTTP runtime.

use std::collections::HashSet;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use ipnet::IpNet;
use mcp_toolkit_auth::{AuthConfig, AuthMode, AuthSecurityProfile, ClientAuthMethod};

use crate::config::UpstreamTokenSource;

const DEFAULT_BIND_ADDR: &str = "127.0.0.1:9420";
const DEFAULT_ALLOWED_HOSTS: &str = "localhost,127.0.0.1,::1";
const DEFAULT_ALLOWED_CIDRS: &str = "127.0.0.1/32,::1/128";

#[derive(Debug, Clone)]
pub struct HttpSettings {
    pub bind_addr: SocketAddr,
    pub allow_non_loopback: bool,
    pub allowed_hosts: HashSet<String>,
    pub allowed_cidrs: Vec<IpNet>,
    pub tls_cert_path: Option<PathBuf>,
    pub tls_key_path: Option<PathBuf>,
    pub auth: HttpAuthSettings,
}

#[derive(Debug, Clone)]
pub struct HttpAuthSettings {
    pub enabled: bool,
    pub realm: String,
    pub resource_url: Option<String>,
    pub issuer: Option<String>,
    pub required_scopes: Vec<String>,
    pub scopes_supported: Vec<String>,
    pub allowed_client_ids: Vec<String>,
    pub auth_config: AuthConfig,
}

impl HttpAuthSettings {
    fn disabled() -> Self {
        let mut auth_config = AuthConfig::with_profile(AuthSecurityProfile::L2Strong);
        auth_config.required_scopes.clear();
        Self {
            enabled: false,
            realm: "ga4-mcp".to_string(),
            resource_url: None,
            issuer: None,
            required_scopes: Vec::new(),
            scopes_supported: Vec::new(),
            allowed_client_ids: Vec::new(),
            auth_config,
        }
    }

    fn from_env() -> Result<Self> {
        let enabled = env_flag("GA4_MCP_AUTH_ENABLED", false)?;
        if !enabled {
            return Ok(Self::disabled());
        }

        let mut auth_config = AuthConfig::with_profile(AuthSecurityProfile::L2Strong);
        auth_config.mode = parse_auth_mode(&env_setting("GA4_MCP_AUTH_MODE", "jwks"))?;
        auth_config.strict_oauth = env_flag("GA4_MCP_AUTH_STRICT_OAUTH", auth_config.strict_oauth)?;
        auth_config.jwks_url = env_optional_string("GA4_MCP_AUTH_JWKS_URL");
        auth_config.issuer = env_optional_string("GA4_MCP_AUTH_ISSUER");
        auth_config.audience = env_optional_string("GA4_MCP_AUTH_AUDIENCE");
        auth_config.required_scopes =
            parse_csv(&env_setting("GA4_MCP_AUTH_REQUIRED_SCOPES", "ga4:read"));
        auth_config.actor_claim = env_setting("GA4_MCP_AUTH_ACTOR_CLAIM", "sub");
        auth_config.introspection_url = env_optional_string("GA4_MCP_AUTH_INTROSPECTION_URL");
        auth_config.introspection_client_id =
            env_optional_string("GA4_MCP_AUTH_INTROSPECTION_CLIENT_ID");
        auth_config.introspection_client_secret =
            env_optional_string("GA4_MCP_AUTH_INTROSPECTION_CLIENT_SECRET");
        auth_config.introspection_auth_method = parse_auth_method(&env_setting(
            "GA4_MCP_AUTH_INTROSPECTION_AUTH_METHOD",
            "client_secret_basic",
        ))?;
        auth_config.introspection_cache_ttl_s = env_f64(
            "GA4_MCP_AUTH_INTROSPECTION_CACHE_TTL_S",
            auth_config.introspection_cache_ttl_s,
        )?;
        auth_config.introspection_force = env_flag(
            "GA4_MCP_AUTH_INTROSPECTION_FORCE",
            auth_config.introspection_force,
        )?;
        auth_config.delegation_secret = env_optional_string("GA4_MCP_AUTH_DELEGATION_SECRET");
        auth_config.delegation_issuer = env_setting("GA4_MCP_AUTH_DELEGATION_ISSUER", "ga4-mcp");
        auth_config.delegation_audience =
            env_setting("GA4_MCP_AUTH_DELEGATION_AUDIENCE", "ga4-mcp");
        auth_config.jti_ttl_s = env_f64("GA4_MCP_AUTH_JTI_TTL_S", auth_config.jti_ttl_s)?;
        auth_config.jti_cache_size =
            env_i64("GA4_MCP_AUTH_JTI_CACHE_SIZE", auth_config.jti_cache_size)?;
        auth_config.jti_enforce_bearer = env_flag(
            "GA4_MCP_AUTH_JTI_ENFORCE_BEARER",
            auth_config.jti_enforce_bearer,
        )?;
        auth_config.clock_skew_s = env_f64("GA4_MCP_AUTH_CLOCK_SKEW_S", auth_config.clock_skew_s)?;
        validate_auth_config_for_mode(&auth_config)?;

        let required_scopes = auth_config.required_scopes.clone();
        let scopes_supported = {
            let configured = parse_csv(&env_setting("GA4_MCP_AUTH_SCOPES_SUPPORTED", ""));
            if configured.is_empty() {
                required_scopes.clone()
            } else {
                configured
            }
        };
        let allowed_client_ids = parse_csv(&env_setting("GA4_MCP_AUTH_ALLOWED_CLIENT_IDS", ""));
        let realm = env_setting("GA4_MCP_AUTH_REALM", "ga4-mcp");
        let resource_url = env_optional_string("GA4_MCP_AUTH_RESOURCE_URL");
        let issuer = auth_config.issuer.clone();

        Ok(Self {
            enabled,
            realm,
            resource_url,
            issuer,
            required_scopes,
            scopes_supported,
            allowed_client_ids,
            auth_config,
        })
    }
}

impl HttpSettings {
    pub fn from_env() -> Result<Self> {
        let bind_addr = env_setting("GA4_MCP_BIND_ADDR", DEFAULT_BIND_ADDR)
            .parse::<SocketAddr>()
            .map_err(|err| anyhow!("invalid GA4_MCP_BIND_ADDR: {err}"))?;
        let allow_non_loopback = env_flag("GA4_MCP_ALLOW_NON_LOOPBACK", false)?;
        if !allow_non_loopback && !bind_addr.ip().is_loopback() {
            return Err(anyhow!(
                "non-loopback bind denied; set GA4_MCP_ALLOW_NON_LOOPBACK=1 to override"
            ));
        }

        let allowed_hosts =
            parse_allowed_hosts(&env_setting("GA4_MCP_ALLOWED_HOSTS", DEFAULT_ALLOWED_HOSTS));
        if allowed_hosts.is_empty() {
            return Err(anyhow!("GA4_MCP_ALLOWED_HOSTS must not be empty"));
        }

        let allowed_cidrs = parse_allowed_cidrs(
            &env_setting("GA4_MCP_ALLOWED_CIDRS", DEFAULT_ALLOWED_CIDRS),
            "GA4_MCP_ALLOWED_CIDRS",
        )?;
        if allowed_cidrs.is_empty() {
            return Err(anyhow!("GA4_MCP_ALLOWED_CIDRS must not be empty"));
        }

        let tls_cert_path = env_optional_path("GA4_MCP_TLS_CERT_PATH");
        let tls_key_path = env_optional_path("GA4_MCP_TLS_KEY_PATH");
        match (tls_cert_path.as_ref(), tls_key_path.as_ref()) {
            (Some(_), Some(_)) => {}
            (None, None) => {}
            _ => {
                return Err(anyhow!(
                    "GA4_MCP_TLS_CERT_PATH and GA4_MCP_TLS_KEY_PATH must both be set together"
                ));
            }
        }
        if allow_non_loopback && (tls_cert_path.is_none() || tls_key_path.is_none()) {
            return Err(anyhow!(
                "non-loopback HTTP exposure requires native TLS; set GA4_MCP_TLS_CERT_PATH and GA4_MCP_TLS_KEY_PATH"
            ));
        }
        let auth = HttpAuthSettings::from_env()?;
        validate_public_exposure_auth_posture(allow_non_loopback, &auth)?;

        Ok(Self {
            bind_addr,
            allow_non_loopback,
            allowed_hosts,
            allowed_cidrs,
            tls_cert_path,
            tls_key_path,
            auth,
        })
    }

    pub fn client_ip_allowed(&self, ip: std::net::IpAddr) -> bool {
        self.allowed_cidrs.iter().any(|cidr| cidr.contains(&ip))
    }

    pub fn tls_files(&self) -> Option<(&std::path::Path, &std::path::Path)> {
        match (self.tls_cert_path.as_deref(), self.tls_key_path.as_deref()) {
            (Some(cert), Some(key)) => Some((cert, key)),
            _ => None,
        }
    }
}

fn env_setting(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_optional_string(name: &str) -> Option<String> {
    env::var(name).ok().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn env_flag(name: &str, default: bool) -> Result<bool> {
    let raw = match env::var(name) {
        Ok(value) => value,
        Err(_) => return Ok(default),
    };
    parse_flag(&raw)
        .ok_or_else(|| anyhow!("invalid {name} value {raw:?}; expected true/false or 1/0"))
}

fn parse_flag(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_f64(name: &str, default: f64) -> Result<f64> {
    let raw = match env::var(name) {
        Ok(value) => value,
        Err(_) => return Ok(default),
    };
    raw.trim()
        .parse::<f64>()
        .map_err(|err| anyhow!("invalid {name} value {raw:?}: {err}"))
}

fn env_i64(name: &str, default: i64) -> Result<i64> {
    let raw = match env::var(name) {
        Ok(value) => value,
        Err(_) => return Ok(default),
    };
    raw.trim()
        .parse::<i64>()
        .map_err(|err| anyhow!("invalid {name} value {raw:?}: {err}"))
}

fn parse_auth_mode(raw: &str) -> Result<AuthMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "delegation" => Ok(AuthMode::Delegation),
        "jwks" => Ok(AuthMode::Jwks),
        "introspection" => Ok(AuthMode::Introspection),
        _ => Err(anyhow!(
            "invalid GA4_MCP_AUTH_MODE value {raw:?}; expected delegation/jwks/introspection"
        )),
    }
}

fn parse_auth_method(raw: &str) -> Result<ClientAuthMethod> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "client_secret_basic" => Ok(ClientAuthMethod::ClientSecretBasic),
        "client_secret_post" => Ok(ClientAuthMethod::ClientSecretPost),
        _ => Err(anyhow!(
            "invalid GA4_MCP_AUTH_INTROSPECTION_AUTH_METHOD value {raw:?}; expected client_secret_basic/client_secret_post"
        )),
    }
}

fn validate_auth_config_for_mode(auth_config: &AuthConfig) -> Result<()> {
    if auth_config.required_scopes.is_empty() {
        return Err(anyhow!(
            "GA4_MCP_AUTH_REQUIRED_SCOPES must include at least one scope when auth is enabled"
        ));
    }
    if auth_config.actor_claim.trim().is_empty() {
        return Err(anyhow!(
            "GA4_MCP_AUTH_ACTOR_CLAIM must not be empty when auth is enabled"
        ));
    }

    let has_issuer = auth_config
        .issuer
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    let has_audience = auth_config
        .audience
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    let has_jwks_url = auth_config
        .jwks_url
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    let has_introspection_url = auth_config
        .introspection_url
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    let has_introspection_client_id = auth_config
        .introspection_client_id
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    let has_introspection_client_secret = auth_config
        .introspection_client_secret
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    let has_delegation_secret = auth_config
        .delegation_secret
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    let has_delegation_issuer = !auth_config.delegation_issuer.trim().is_empty();
    let has_delegation_audience = !auth_config.delegation_audience.trim().is_empty();

    match auth_config.mode {
        AuthMode::Jwks => {
            if !has_jwks_url || !has_issuer || !has_audience {
                return Err(anyhow!(
                    "GA4_MCP_AUTH_MODE=jwks requires GA4_MCP_AUTH_JWKS_URL, GA4_MCP_AUTH_ISSUER, and GA4_MCP_AUTH_AUDIENCE"
                ));
            }
            if has_introspection_url
                && (!has_introspection_client_id || !has_introspection_client_secret)
            {
                return Err(anyhow!(
                    "JWKS mode with GA4_MCP_AUTH_INTROSPECTION_URL also requires GA4_MCP_AUTH_INTROSPECTION_CLIENT_ID and GA4_MCP_AUTH_INTROSPECTION_CLIENT_SECRET"
                ));
            }
        }
        AuthMode::Introspection => {
            if !has_introspection_url {
                return Err(anyhow!(
                    "GA4_MCP_AUTH_MODE=introspection requires GA4_MCP_AUTH_INTROSPECTION_URL"
                ));
            }
            if !has_introspection_client_id || !has_introspection_client_secret {
                return Err(anyhow!(
                    "GA4_MCP_AUTH_MODE=introspection requires GA4_MCP_AUTH_INTROSPECTION_CLIENT_ID and GA4_MCP_AUTH_INTROSPECTION_CLIENT_SECRET"
                ));
            }
            if !has_issuer || !has_audience {
                return Err(anyhow!(
                    "GA4_MCP_AUTH_MODE=introspection requires GA4_MCP_AUTH_ISSUER and GA4_MCP_AUTH_AUDIENCE"
                ));
            }
        }
        AuthMode::Delegation => {
            if !has_delegation_secret {
                return Err(anyhow!(
                    "GA4_MCP_AUTH_MODE=delegation requires GA4_MCP_AUTH_DELEGATION_SECRET"
                ));
            }
            if !has_delegation_issuer || !has_delegation_audience {
                return Err(anyhow!(
                    "GA4_MCP_AUTH_MODE=delegation requires non-empty GA4_MCP_AUTH_DELEGATION_ISSUER and GA4_MCP_AUTH_DELEGATION_AUDIENCE"
                ));
            }
            if has_introspection_url {
                return Err(anyhow!(
                    "GA4_MCP_AUTH_INTROSPECTION_URL is not supported with GA4_MCP_AUTH_MODE=delegation"
                ));
            }
        }
    }

    Ok(())
}

fn validate_public_exposure_auth_posture(
    allow_non_loopback: bool,
    auth: &HttpAuthSettings,
) -> Result<()> {
    if !allow_non_loopback || !auth.enabled {
        return Ok(());
    }
    if !auth.auth_config.strict_oauth {
        return Err(anyhow!(
            "public exposure requires strict OAuth parsing; set GA4_MCP_AUTH_STRICT_OAUTH=1"
        ));
    }

    ensure_https_url_if_set("GA4_MCP_AUTH_RESOURCE_URL", auth.resource_url.as_deref())?;
    ensure_https_url_if_set("GA4_MCP_AUTH_ISSUER", auth.issuer.as_deref())?;
    ensure_https_url_if_set(
        "GA4_MCP_AUTH_JWKS_URL",
        auth.auth_config.jwks_url.as_deref(),
    )?;
    ensure_https_url_if_set(
        "GA4_MCP_AUTH_INTROSPECTION_URL",
        auth.auth_config.introspection_url.as_deref(),
    )?;
    Ok(())
}

fn ensure_https_url_if_set(var_name: &str, value: Option<&str>) -> Result<()> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let lowercase = value.to_ascii_lowercase();
    if lowercase.starts_with("http://") {
        return Err(anyhow!(
            "{var_name} must use https:// for non-loopback/public exposure"
        ));
    }
    if !lowercase.starts_with("https://") {
        return Err(anyhow!(
            "{var_name} must be an absolute https URL for non-loopback/public exposure"
        ));
    }
    Ok(())
}

pub fn validate_http_runtime_credential_posture(
    settings: &HttpSettings,
    upstream_token_source: UpstreamTokenSource,
    upstream_token_header: &str,
) -> Result<()> {
    if settings.auth.enabled
        && upstream_token_source.uses_request_header()
        && upstream_token_header.eq_ignore_ascii_case("authorization")
    {
        return Err(anyhow!(
            "GA4_MCP_AUTH_ENABLED=1 cannot share the authorization header with upstream Google tokens; set GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=x-google-access-token"
        ));
    }
    if settings.allow_non_loopback && upstream_token_source != UpstreamTokenSource::RequestHeader {
        return Err(anyhow!(
            "non-loopback HTTP exposure must use GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header so each client supplies its own Google token; request_header_or_config and config can fall back to server-held credentials"
        ));
    }
    Ok(())
}

fn parse_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_allowed_hosts(raw: &str) -> HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.to_ascii_lowercase())
        .collect()
}

fn parse_allowed_cidrs(raw: &str, var_name: &str) -> Result<Vec<IpNet>> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.parse::<IpNet>()
                .map_err(|err| anyhow::Error::msg(invalid_cidr_message(var_name, item, err)))
        })
        .collect()
}

fn invalid_cidr_message(var_name: &str, item: &str, err: impl std::fmt::Display) -> String {
    let mut message = String::from("invalid ");
    message.push_str(var_name);
    message.push_str(" CIDR ");
    message.push_str(item);
    message.push_str(": ");
    message.push_str(&err.to_string());
    message
}

fn env_optional_path(name: &str) -> Option<PathBuf> {
    env::var(name).ok().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_toolkit_auth::AuthSecurityProfile;

    fn base_auth_config() -> AuthConfig {
        let mut config = AuthConfig::with_profile(AuthSecurityProfile::L2Strong);
        config.required_scopes = vec!["ga4:read".to_string()];
        config.actor_claim = "sub".to_string();
        config
    }

    #[test]
    fn parse_flag_supports_common_boolean_forms() {
        assert_eq!(parse_flag("true"), Some(true));
        assert_eq!(parse_flag("1"), Some(true));
        assert_eq!(parse_flag("on"), Some(true));
        assert_eq!(parse_flag("false"), Some(false));
        assert_eq!(parse_flag("0"), Some(false));
        assert_eq!(parse_flag("off"), Some(false));
        assert_eq!(parse_flag("bogus"), None);
    }

    #[test]
    fn parse_allowed_cidrs_rejects_invalid_entries() {
        let err = parse_allowed_cidrs("192.168.1.0/24,not-a-cidr", "GA4_MCP_ALLOWED_CIDRS")
            .expect_err("invalid CIDR must fail");
        assert!(
            err.to_string()
                .contains("invalid GA4_MCP_ALLOWED_CIDRS CIDR")
        );
    }

    #[test]
    fn parse_allowed_hosts_normalizes_values() {
        let hosts = parse_allowed_hosts(" localhost,Example.COM,,127.0.0.1 ");
        assert!(hosts.contains("localhost"));
        assert!(hosts.contains("example.com"));
        assert!(hosts.contains("127.0.0.1"));
        assert_eq!(hosts.len(), 3);
    }

    #[test]
    fn parse_auth_mode_rejects_unknown_value() {
        let err = parse_auth_mode("unsupported").expect_err("unknown mode should fail");
        assert!(err.to_string().contains("invalid GA4_MCP_AUTH_MODE"));
    }

    #[test]
    fn parse_auth_method_rejects_unknown_value() {
        let err = parse_auth_method("unsupported").expect_err("unknown method should fail");
        assert!(
            err.to_string()
                .contains("invalid GA4_MCP_AUTH_INTROSPECTION_AUTH_METHOD")
        );
    }

    #[test]
    fn disabled_auth_settings_do_not_enable_resource_server_mode() {
        let auth = HttpAuthSettings::disabled();
        assert!(!auth.enabled);
        assert!(auth.required_scopes.is_empty());
        assert!(auth.scopes_supported.is_empty());
    }

    #[test]
    fn validate_auth_config_jwks_requires_core_fields() {
        let mut config = base_auth_config();
        config.mode = AuthMode::Jwks;
        config.jwks_url = Some("https://issuer.example/jwks".to_string());
        config.issuer = Some("https://issuer.example".to_string());
        config.audience = Some("ga4-mcp".to_string());
        validate_auth_config_for_mode(&config).expect("jwks config should pass");

        config.audience = None;
        let err = validate_auth_config_for_mode(&config).expect_err("missing audience must fail");
        assert!(err.to_string().contains("GA4_MCP_AUTH_MODE=jwks requires"));
    }

    #[test]
    fn validate_auth_config_introspection_requires_credentials() {
        let mut config = base_auth_config();
        config.mode = AuthMode::Introspection;
        config.issuer = Some("https://issuer.example".to_string());
        config.audience = Some("ga4-mcp".to_string());
        config.introspection_url = Some("https://issuer.example/introspect".to_string());
        config.introspection_client_id = Some("client-id".to_string());
        config.introspection_client_secret = Some("client-secret".to_string());
        validate_auth_config_for_mode(&config).expect("introspection config should pass");

        config.introspection_client_secret = None;
        let err =
            validate_auth_config_for_mode(&config).expect_err("missing client secret must fail");
        assert!(
            err.to_string()
                .contains("GA4_MCP_AUTH_MODE=introspection requires")
        );
    }

    #[test]
    fn validate_auth_config_delegation_requires_secret() {
        let mut config = base_auth_config();
        config.mode = AuthMode::Delegation;
        config.delegation_issuer = "ga4-mcp".to_string();
        config.delegation_audience = "ga4-mcp".to_string();
        config.delegation_secret = Some("secret".to_string());
        validate_auth_config_for_mode(&config).expect("delegation config should pass");

        config.delegation_secret = None;
        let err =
            validate_auth_config_for_mode(&config).expect_err("missing delegation secret fails");
        assert!(
            err.to_string()
                .contains("GA4_MCP_AUTH_MODE=delegation requires")
        );
    }

    #[test]
    fn validate_auth_config_rejects_introspection_url_in_delegation_mode() {
        let mut config = base_auth_config();
        config.mode = AuthMode::Delegation;
        config.delegation_secret = Some("secret".to_string());
        config.introspection_url = Some("https://issuer.example/introspect".to_string());
        config.delegation_issuer = "ga4-mcp".to_string();
        config.delegation_audience = "ga4-mcp".to_string();
        let err = validate_auth_config_for_mode(&config)
            .expect_err("delegation mode cannot include introspection URL");
        assert!(
            err.to_string()
                .contains("not supported with GA4_MCP_AUTH_MODE=delegation")
        );
    }

    #[test]
    fn validate_public_exposure_requires_https_endpoints() {
        let mut auth = HttpAuthSettings::disabled();
        auth.enabled = true;
        auth.auth_config.strict_oauth = true;
        auth.resource_url = Some("http://mcp.example.com/mcp".to_string());
        auth.issuer = Some("https://issuer.example".to_string());
        auth.auth_config.jwks_url = Some("https://issuer.example/jwks".to_string());
        let err = validate_public_exposure_auth_posture(true, &auth)
            .expect_err("http resource URL must fail");
        assert!(
            err.to_string()
                .contains("GA4_MCP_AUTH_RESOURCE_URL must use https://")
        );
    }

    #[test]
    fn validate_public_exposure_accepts_loopback_profiles() {
        let mut auth = HttpAuthSettings::disabled();
        auth.enabled = true;
        auth.auth_config.strict_oauth = false;
        auth.resource_url = Some("http://127.0.0.1:9420/mcp".to_string());
        validate_public_exposure_auth_posture(false, &auth)
            .expect("loopback deployment should not enforce public posture");
    }

    fn base_http_settings() -> HttpSettings {
        HttpSettings {
            bind_addr: DEFAULT_BIND_ADDR.parse().expect("bind addr"),
            allow_non_loopback: false,
            allowed_hosts: parse_allowed_hosts(DEFAULT_ALLOWED_HOSTS),
            allowed_cidrs: parse_allowed_cidrs(DEFAULT_ALLOWED_CIDRS, "GA4_MCP_ALLOWED_CIDRS")
                .expect("default CIDRs"),
            tls_cert_path: None,
            tls_key_path: None,
            auth: HttpAuthSettings::disabled(),
        }
    }

    #[test]
    fn runtime_posture_rejects_public_server_fallback_even_with_inbound_auth() {
        let mut settings = base_http_settings();
        settings.allow_non_loopback = true;
        settings.auth.enabled = true;
        let err = validate_http_runtime_credential_posture(
            &settings,
            UpstreamTokenSource::RequestHeaderOrConfig,
            "x-google-access-token",
        )
        .expect_err("public server fallback should fail");
        assert!(err.to_string().contains("request_header"));
    }

    #[test]
    fn runtime_posture_rejects_authorization_header_collision_when_auth_enabled() {
        let mut settings = base_http_settings();
        settings.auth.enabled = true;
        let err = validate_http_runtime_credential_posture(
            &settings,
            UpstreamTokenSource::RequestHeader,
            "authorization",
        )
        .expect_err("authorization header collision should fail");
        assert!(err.to_string().contains("x-google-access-token"));
    }
}
