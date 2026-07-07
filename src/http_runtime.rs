//! # HTTP Runtime
//!
//! Streamable HTTP transport wiring for GA4 MCP with host/IP guardrails.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use axum::extract::{State, connect_info::ConnectInfo};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, HOST, PROXY_AUTHORIZATION, USER_AGENT};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use axum_server::tls_rustls::RustlsConfig;
use mcp_toolkit_auth::surface::{AuthSurfaceConfig, AuthSurfaceLayer, IssuerEntry};
use mcp_toolkit_auth::{Authenticator, discover_oidc_metadata};
use mcp_toolkit_http::host::{
    HostValidationError, parse_host_header, validate_origin_header, validate_request_authority,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::json;

use crate::http_config::HttpSettings;
use crate::server::AnalyticsMcp;

#[derive(Clone)]
struct GuardState {
    settings: HttpSettings,
    allowed_request_hosts: Vec<String>,
    accepts_upstream_request_tokens: bool,
    upstream_token_header: String,
}

/// Serve the GA4 MCP server on streamable HTTP transport.
///
/// # Errors
/// Returns an error when binding or HTTP serving fails.
pub async fn run_http_server(server: AnalyticsMcp, settings: HttpSettings) -> Result<()> {
    let server_factory = server.clone();
    let mcp_service: StreamableHttpService<AnalyticsMcp, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server_factory.clone()),
            Default::default(),
            StreamableHttpServerConfig::default(),
        );

    let guard_state = GuardState {
        settings: settings.clone(),
        allowed_request_hosts: settings.allowed_hosts.iter().cloned().collect(),
        accepts_upstream_request_tokens: server.accepts_upstream_request_tokens(),
        upstream_token_header: server.client.upstream_token_header().to_string(),
    };
    let mut router = Router::new()
        .route("/health", get(health))
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            guard_state.clone(),
            inbound_auth_guard,
        ))
        .layer(middleware::from_fn_with_state(
            guard_state.clone(),
            host_guard,
        ))
        .layer(middleware::from_fn_with_state(
            guard_state.clone(),
            client_ip_guard,
        ))
        .with_state(guard_state.clone());
    if let Some(auth_layer) = build_auth_surface_layer(&settings).await? {
        router = router.layer(auth_layer);
    }
    router = router.layer(middleware::from_fn_with_state(
        guard_state.clone(),
        request_audit,
    ));

    mcp_toolkit_observability::emit_event(
        mcp_toolkit_observability::Level::INFO,
        "ga4_mcp.startup",
        &mcp_toolkit_observability::EventContext::new(),
        &[
            mcp_toolkit_observability::safe_text("transport", "streamable_http"),
            mcp_toolkit_observability::safe_text("bind_addr", &settings.bind_addr.to_string()),
            mcp_toolkit_observability::safe_text(
                "allow_non_loopback",
                if settings.allow_non_loopback {
                    "true"
                } else {
                    "false"
                },
            ),
            mcp_toolkit_observability::safe_text(
                "tls_enabled",
                if settings.tls_files().is_some() {
                    "true"
                } else {
                    "false"
                },
            ),
            mcp_toolkit_observability::safe_text(
                "auth_enabled",
                if settings.auth.enabled {
                    "true"
                } else {
                    "false"
                },
            ),
        ],
    );

    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        shutdown_handle.graceful_shutdown(None);
    });

    let service = router.into_make_service_with_connect_info::<SocketAddr>();
    if let Some((cert_path, key_path)) = settings.tls_files() {
        let tls = RustlsConfig::from_pem_file(cert_path, key_path).await?;
        axum_server::bind_rustls(settings.bind_addr, tls)
            .handle(handle)
            .serve(service)
            .await?;
    } else {
        axum_server::bind(settings.bind_addr)
            .handle(handle)
            .serve(service)
            .await?;
    }
    Ok(())
}

async fn health(State(state): State<GuardState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "transport": "streamable_http",
        "bind_addr": state.settings.bind_addr.to_string(),
        "cidr_allowlist_entries": state.settings.allowed_cidrs.len(),
        "host_allowlist_entries": state.settings.allowed_hosts.len(),
        "auth_enabled": state.settings.auth.enabled,
    }))
}

async fn request_audit(
    State(state): State<GuardState>,
    req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let host = request_host_for_log(req.headers(), req.uri());
    let cf_ray = header_text(req.headers(), "cf-ray");
    let cf_connecting_ip = header_text(req.headers(), "cf-connecting-ip");
    let true_client_ip = header_text(req.headers(), "true-client-ip");
    let user_agent = req
        .headers()
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let remote_addr = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect_info| connect_info.0.to_string());

    let response = next.run(req).await;
    emit_request_audit_event(
        &method,
        &path,
        response.status(),
        state.settings.auth.enabled,
        host.as_deref(),
        cf_ray.as_deref(),
        cf_connecting_ip.as_deref(),
        true_client_ip.as_deref(),
        user_agent.as_deref(),
        remote_addr.as_deref(),
    );
    response
}

async fn inbound_auth_guard(
    State(state): State<GuardState>,
    req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    if state.settings.auth.enabled {
        return next.run(req).await;
    }

    if request_targets_mcp(req.uri().path())
        && request_has_inbound_auth(req.headers())
        && (!state.accepts_upstream_request_tokens
            || request_has_disallowed_inbound_auth(req.headers(), &state.upstream_token_header))
    {
        mcp_toolkit_observability::emit_event(
            mcp_toolkit_observability::Level::WARN,
            "ga4_mcp.inbound_auth_header_rejected",
            &mcp_toolkit_observability::EventContext::new(),
            &[mcp_toolkit_observability::safe_text(
                "path",
                req.uri().path(),
            )],
        );
        return (
            StatusCode::BAD_REQUEST,
            "Authorization headers are not accepted on /mcp unless request-header upstream token mode explicitly uses that header",
        )
            .into_response();
    }
    next.run(req).await
}

fn emit_request_audit_event(
    method: &str,
    path: &str,
    status: StatusCode,
    auth_enabled: bool,
    host: Option<&str>,
    cf_ray: Option<&str>,
    cf_connecting_ip: Option<&str>,
    true_client_ip: Option<&str>,
    user_agent: Option<&str>,
    remote_addr: Option<&str>,
) {
    let level = match status.as_u16() {
        500..=599 => mcp_toolkit_observability::Level::ERROR,
        400..=499 => mcp_toolkit_observability::Level::WARN,
        _ => mcp_toolkit_observability::Level::INFO,
    };
    let mut fields = vec![
        mcp_toolkit_observability::safe_text("method", method),
        mcp_toolkit_observability::safe_text("path", path),
        mcp_toolkit_observability::safe_text("status", status.as_u16().to_string()),
        mcp_toolkit_observability::safe_text(
            "auth_enabled",
            if auth_enabled { "true" } else { "false" },
        ),
    ];
    if let Some(value) = host {
        fields.push(mcp_toolkit_observability::safe_text("host", value));
    }
    if let Some(value) = cf_ray {
        fields.push(mcp_toolkit_observability::safe_text("cf_ray", value));
    }
    if let Some(value) = cf_connecting_ip {
        fields.push(mcp_toolkit_observability::safe_text(
            "cf_connecting_ip",
            value,
        ));
    }
    if let Some(value) = true_client_ip {
        fields.push(mcp_toolkit_observability::safe_text(
            "true_client_ip",
            value,
        ));
    }
    if let Some(value) = user_agent {
        fields.push(mcp_toolkit_observability::safe_text("user_agent", value));
    }
    if let Some(value) = remote_addr {
        fields.push(mcp_toolkit_observability::safe_text("remote_addr", value));
    }

    mcp_toolkit_observability::emit_event(
        level,
        "ga4_mcp.http.request",
        &mcp_toolkit_observability::EventContext::new(),
        &fields,
    );
}

fn request_host_for_log(headers: &axum::http::HeaderMap, uri: &axum::http::Uri) -> Option<String> {
    headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_host_header)
        .map(|parsed| parsed.host)
        .or_else(|| {
            uri.authority()
                .and_then(|value| parse_host_header(value.as_str()))
                .map(|parsed| parsed.host)
        })
}

fn header_text(headers: &axum::http::HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

async fn host_guard(
    State(state): State<GuardState>,
    req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    if let Err(err) = validate_request_host(&req, &state.allowed_request_hosts) {
        let status = err.status_code();
        let message = err.message();
        return (status, message).into_response();
    }
    if let Err(err) = validate_request_origin(&req, &state.allowed_request_hosts) {
        let status = err.status_code();
        let message = err.message();
        return (status, message).into_response();
    }
    next.run(req).await
}

async fn client_ip_guard(
    State(state): State<GuardState>,
    req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    let client_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect_info| connect_info.0.ip());
    let Some(client_ip) = client_ip else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Missing client address metadata",
        )
            .into_response();
    };

    if !state.settings.client_ip_allowed(client_ip) {
        return (
            StatusCode::FORBIDDEN,
            "Client IP is not in the allowed CIDR set",
        )
            .into_response();
    }

    next.run(req).await
}

fn validate_request_host(
    req: &axum::extract::Request,
    allowed_hosts: &[String],
) -> Result<(), HostValidationError> {
    validate_request_authority(Some(req.uri()), req.headers(), allowed_hosts).map(|_| ())
}

fn validate_request_origin(
    req: &axum::extract::Request,
    allowed_hosts: &[String],
) -> Result<(), HostValidationError> {
    validate_origin_header(req.headers(), allowed_hosts)
}

fn public_base_url(settings: &HttpSettings) -> String {
    let scheme = if settings.tls_files().is_some() {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://{}", settings.bind_addr)
}

fn public_base_url_from_resource_url(resource_url: &str) -> Option<String> {
    let trimmed = resource_url.trim().trim_end_matches('/');
    let base = trimmed.strip_suffix("/mcp")?;
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

fn fallback_oauth_endpoints(issuer: &str) -> (String, String) {
    let trimmed = issuer.trim_end_matches('/');
    if trimmed.contains("/realms/") {
        return (
            format!("{trimmed}/protocol/openid-connect/auth"),
            format!("{trimmed}/protocol/openid-connect/token"),
        );
    }
    (
        format!("{trimmed}/oauth/authorize"),
        format!("{trimmed}/oauth/token"),
    )
}

fn url_uses_insecure_http(value: &str) -> bool {
    value.trim().starts_with("http://")
}

fn auth_surface_allow_insecure_http(config: &AuthSurfaceConfig) -> bool {
    if url_uses_insecure_http(&config.public_base_url) {
        return true;
    }
    config.entries.iter().any(|entry| {
        url_uses_insecure_http(&entry.issuer)
            || url_uses_insecure_http(&entry.authorization_endpoint)
            || url_uses_insecure_http(&entry.token_endpoint)
            || entry
                .jwks_uri
                .as_deref()
                .map(url_uses_insecure_http)
                .unwrap_or(false)
            || entry
                .introspection_endpoint
                .as_deref()
                .map(url_uses_insecure_http)
                .unwrap_or(false)
            || entry
                .resource_url_override
                .as_deref()
                .map(url_uses_insecure_http)
                .unwrap_or(false)
    })
}

async fn build_auth_surface_layer(settings: &HttpSettings) -> Result<Option<AuthSurfaceLayer>> {
    if !settings.auth.enabled {
        return Ok(None);
    }

    let auth = Arc::new(
        Authenticator::new(settings.auth.auth_config.clone())
            .map_err(|err| anyhow!("invalid auth config: {err}"))?,
    );

    let mut base_url = public_base_url(settings);
    if let Some(resource_url) = settings.auth.resource_url.as_deref() {
        match public_base_url_from_resource_url(resource_url) {
            Some(derived) => base_url = derived,
            None => tracing::warn!(
                resource_url,
                "GA4_MCP_AUTH_RESOURCE_URL does not end with /mcp; using bind address for auth surface base URL"
            ),
        }
    }

    let issuer = settings
        .auth
        .issuer
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| base_url.clone());
    let (default_authz, default_token) = fallback_oauth_endpoints(&issuer);
    let (
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        discovered_jwks_uri,
        discovered_introspection,
        discovered_device_authorization,
        discovered_grant_types,
        discovered_client_id_metadata_document_supported,
        discovered_token_endpoint_auth_methods,
        discovered_code_challenge_methods,
    ) = if settings.auth.issuer.is_some() {
        match discover_oidc_metadata(&issuer, None).await {
            Ok(metadata) => (
                metadata
                    .authorization_endpoint
                    .unwrap_or_else(|| default_authz.clone()),
                metadata
                    .token_endpoint
                    .unwrap_or_else(|| default_token.clone()),
                metadata.registration_endpoint,
                Some(metadata.jwks_uri),
                metadata.introspection_endpoint,
                metadata.device_authorization_endpoint,
                metadata.grant_types_supported,
                metadata.client_id_metadata_document_supported,
                metadata.token_endpoint_auth_methods_supported,
                metadata.code_challenge_methods_supported,
            ),
            Err(err) => {
                tracing::warn!(
                    issuer = %issuer,
                    err = %err,
                    "failed OIDC discovery for auth surface; using fallback OAuth endpoint URLs"
                );
                (
                    default_authz,
                    default_token,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
            }
        }
    } else {
        (
            default_authz,
            default_token,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    };

    let mcp_entry = IssuerEntry {
        resource_path: "/mcp".to_string(),
        issuer,
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        jwks_uri: settings
            .auth
            .auth_config
            .jwks_url
            .clone()
            .or(discovered_jwks_uri),
        introspection_endpoint: settings
            .auth
            .auth_config
            .introspection_url
            .clone()
            .or(discovered_introspection),
        device_authorization_endpoint: discovered_device_authorization,
        grant_types_supported: discovered_grant_types,
        client_id_metadata_document_supported: discovered_client_id_metadata_document_supported,
        token_endpoint_auth_methods_supported: discovered_token_endpoint_auth_methods,
        code_challenge_methods_supported: discovered_code_challenge_methods,
        realm: settings.auth.realm.clone(),
        scopes_supported: settings.auth.scopes_supported.clone(),
        allowed_client_ids: settings.auth.allowed_client_ids.iter().cloned().collect(),
        authenticator: auth,
        resource_url_override: settings.auth.resource_url.clone(),
    };

    let mut surface = AuthSurfaceConfig::single_issuer(base_url, mcp_entry);
    surface.allow_insecure_http = auth_surface_allow_insecure_http(&surface);
    let layer = AuthSurfaceLayer::from_config(surface)
        .map_err(|err| anyhow!("invalid auth surface config: {err}"))?;
    Ok(Some(layer))
}

fn request_targets_mcp(path: &str) -> bool {
    path == "/mcp" || path.starts_with("/mcp/")
}

fn request_has_inbound_auth(headers: &axum::http::HeaderMap) -> bool {
    headers.contains_key(AUTHORIZATION) || headers.contains_key(PROXY_AUTHORIZATION)
}

fn request_has_disallowed_inbound_auth(
    headers: &axum::http::HeaderMap,
    upstream_token_header: &str,
) -> bool {
    let allows_authorization = upstream_token_header.eq_ignore_ascii_case(AUTHORIZATION.as_str());
    let allows_proxy_authorization =
        upstream_token_header.eq_ignore_ascii_case(PROXY_AUTHORIZATION.as_str());
    (headers.contains_key(AUTHORIZATION) && !allows_authorization)
        || (headers.contains_key(PROXY_AUTHORIZATION) && !allows_proxy_authorization)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::HeaderMap;
    use axum::http::StatusCode;
    use axum::http::Uri;
    use axum::http::header::{AUTHORIZATION, HOST, PROXY_AUTHORIZATION};

    use super::{
        header_text, public_base_url_from_resource_url, request_has_disallowed_inbound_auth,
        request_has_inbound_auth, request_host_for_log, request_targets_mcp, validate_request_host,
        validate_request_origin,
    };

    #[test]
    fn request_targets_mcp_matches_expected_paths() {
        assert!(request_targets_mcp("/mcp"));
        assert!(request_targets_mcp("/mcp/"));
        assert!(request_targets_mcp("/mcp/messages"));
        assert!(!request_targets_mcp("/"));
        assert!(!request_targets_mcp("/health"));
        assert!(!request_targets_mcp("/api/mcp"));
    }

    #[test]
    fn request_has_inbound_auth_detects_authorization_headers() {
        let mut headers = HeaderMap::new();
        assert!(!request_has_inbound_auth(&headers));

        headers.insert(AUTHORIZATION, "Bearer token".parse().expect("header"));
        assert!(request_has_inbound_auth(&headers));

        headers.remove(AUTHORIZATION);
        headers.insert(
            PROXY_AUTHORIZATION,
            "Basic YWxhZGRpbjpvcGVuc2VzYW1l".parse().expect("header"),
        );
        assert!(request_has_inbound_auth(&headers));
    }

    #[test]
    fn dedicated_upstream_header_still_rejects_authorization_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer token".parse().expect("header"));
        assert!(request_has_disallowed_inbound_auth(
            &headers,
            "x-google-access-token"
        ));

        headers.remove(AUTHORIZATION);
        headers.insert(
            PROXY_AUTHORIZATION,
            "Basic YWxhZGRpbjpvcGVuc2VzYW1l".parse().expect("header"),
        );
        assert!(request_has_disallowed_inbound_auth(
            &headers,
            "x-google-access-token"
        ));
    }

    #[test]
    fn matching_upstream_auth_header_is_allowed_but_other_auth_headers_are_not() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer token".parse().expect("header"));
        assert!(!request_has_disallowed_inbound_auth(
            &headers,
            "authorization"
        ));

        headers.insert(
            PROXY_AUTHORIZATION,
            "Basic YWxhZGRpbjpvcGVuc2VzYW1l".parse().expect("header"),
        );
        assert!(request_has_disallowed_inbound_auth(
            &headers,
            "authorization"
        ));
    }

    #[test]
    fn public_base_url_from_resource_url_extracts_expected_prefix() {
        let value = public_base_url_from_resource_url("https://mcp.example.com/mcp")
            .expect("expected base URL");
        assert_eq!(value, "https://mcp.example.com");
    }

    #[test]
    fn public_base_url_from_resource_url_rejects_non_mcp_paths() {
        assert!(public_base_url_from_resource_url("https://mcp.example.com").is_none());
    }

    #[test]
    fn request_host_for_log_prefers_host_header() {
        let mut headers = HeaderMap::new();
        headers.insert(HOST, "analytics.example.com".parse().expect("host"));
        let uri: Uri = "https://127.0.0.1:9420/mcp".parse().expect("uri");
        let host = request_host_for_log(&headers, &uri).expect("host");
        assert_eq!(host, "analytics.example.com");
    }

    #[test]
    fn request_host_for_log_falls_back_to_uri_authority() {
        let headers = HeaderMap::new();
        let uri: Uri = "https://api.example.com/mcp".parse().expect("uri");
        let host = request_host_for_log(&headers, &uri).expect("host");
        assert_eq!(host, "api.example.com");
    }

    #[test]
    fn header_text_trims_and_rejects_blank_values() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-ray", "  abc123-MEL  ".parse().expect("header"));
        headers.insert("x-empty", "   ".parse().expect("header"));

        assert_eq!(
            header_text(&headers, "cf-ray").as_deref(),
            Some("abc123-MEL")
        );
        assert!(header_text(&headers, "x-empty").is_none());
        assert!(header_text(&headers, "missing").is_none());
    }

    #[test]
    fn validate_request_origin_accepts_missing_origin() {
        let allowed_hosts = vec!["ga4-mcp.example".to_string()];
        let req = axum::http::Request::builder()
            .uri("https://ga4-mcp.example/mcp")
            .header(HOST, "ga4-mcp.example")
            .body(Body::empty())
            .expect("request");

        validate_request_origin(&req, &allowed_hosts).expect("missing origin should pass");
    }

    #[test]
    fn validate_request_origin_accepts_allowed_origin() {
        let allowed_hosts = vec!["ga4-mcp.example".to_string()];
        let req = axum::http::Request::builder()
            .uri("https://ga4-mcp.example/mcp")
            .header(HOST, "ga4-mcp.example")
            .header("origin", "https://ga4-mcp.example")
            .body(Body::empty())
            .expect("request");

        validate_request_origin(&req, &allowed_hosts).expect("allowed origin should pass");
    }

    #[test]
    fn validate_request_host_accepts_allowlisted_host_and_port() {
        let allowed_hosts = vec!["ga4-mcp.example:9443".to_string()];
        let req = axum::http::Request::builder()
            .uri("https://ga4-mcp.example:9443/mcp")
            .header(HOST, "ga4-mcp.example:9443")
            .body(Body::empty())
            .expect("request");

        validate_request_host(&req, &allowed_hosts).expect("allowlisted host:port should pass");
    }

    #[test]
    fn validate_request_origin_accepts_allowed_origin_with_port() {
        let allowed_hosts = vec!["ga4-mcp.example:9443".to_string()];
        let req = axum::http::Request::builder()
            .uri("https://ga4-mcp.example:9443/mcp")
            .header(HOST, "ga4-mcp.example:9443")
            .header("origin", "https://ga4-mcp.example:9443")
            .body(Body::empty())
            .expect("request");

        validate_request_origin(&req, &allowed_hosts)
            .expect("allowlisted origin with port should pass");
    }

    #[test]
    fn validate_request_origin_rejects_unexpected_origin() {
        let allowed_hosts = vec!["ga4-mcp.example".to_string()];
        let req = axum::http::Request::builder()
            .uri("https://ga4-mcp.example/mcp")
            .header(HOST, "ga4-mcp.example")
            .header("origin", "https://evil.example")
            .body(Body::empty())
            .expect("request");

        let err = validate_request_origin(&req, &allowed_hosts).expect_err("origin rejection");
        assert_eq!(err.status_code(), StatusCode::FORBIDDEN);
    }
}
