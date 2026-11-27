//! Request-bound hosted scratchpad isolation using the shared toolkit manager.
//! This feature is opt-in; no ambient credential or legacy MCP-session authority.
use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use axum::{
    extract::{Request, State},
    http::{Method, StatusCode, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use mcp_toolkit_auth::{
    AuthMode, Authenticator, surface::consume_verified_auth_surface_request_from_request,
};
use reqwest::{Client, RequestBuilder};
use rmcp::{RoleServer, model::CallToolRequestParams, service::RequestContext};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    config::{CapabilityProfile, UpstreamTokenSource},
    error::AnalyticsError,
    ga_client::{PropertyId, parse_request_token_header_value},
    http_config::HttpSettings,
    scratchpad::{ScratchpadScope, ScratchpadSessionManager},
    server::AnalyticsMcp,
};

const USERINFO: &str = "https://openidconnect.googleapis.com/v1/userinfo";
const ACCOUNT_SUMMARIES: &str = "https://analyticsadmin.googleapis.com/v1beta/accountSummaries";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_PROPERTIES: usize = 4096;
const MAX_PAGES: usize = 10;
const REQUEST_WITNESS_TTL: Duration = Duration::from_secs(30);

fn denied() -> AnalyticsError {
    AnalyticsError::policy_denied("hosted_scratchpad_identity_unverified", "scratchpad")
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
            out
        })
}

fn canonical_token(raw: Option<&str>, header: &str) -> Result<String, AnalyticsError> {
    let token = parse_request_token_header_value(raw, header)?.ok_or_else(denied)?;
    if token.len() > 8192
        || token.is_empty()
        || token
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(denied());
    }
    Ok(token)
}

/// Internal one-use witness created only after consuming the auth surface proof.
/// No raw token is stored here, and this type has no Debug or Serialize impl.
#[derive(Clone)]
struct HostedPrincipal {
    principal: Arc<str>,
    upstream_digest: Arc<str>,
    issued: Instant,
    used: Arc<AtomicBool>,
}

impl HostedPrincipal {
    fn claim(&self, token: &str) -> Result<&str, AnalyticsError> {
        if self.issued.elapsed() >= REQUEST_WITNESS_TTL
            || self.upstream_digest.as_ref() != digest(token.as_bytes())
            || self.used.swap(true, Ordering::AcqRel)
        {
            return Err(denied());
        }
        Ok(&self.principal)
    }
}

#[derive(Clone)]
pub(crate) struct HostedAuthGuard {
    authenticator: Arc<Authenticator>,
    google_header: String,
}

impl HostedAuthGuard {
    pub(crate) fn new(authenticator: Arc<Authenticator>, google_header: String) -> Self {
        Self {
            authenticator,
            google_header,
        }
    }
}

pub(crate) async fn verify_hosted_mcp_request(
    State(state): State<HostedAuthGuard>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if path != "/mcp" && !path.starts_with("/mcp/") {
        return next.run(request).await;
    }
    if request.method() != Method::POST || request.headers().contains_key("mcp-session-id") {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            "hosted scratchpad requires headerless stateless POST",
        )
            .into_response();
    }
    // Consume the current method/URI/header-bound witness before rmcp routing.
    let Some(verified) =
        consume_verified_auth_surface_request_from_request(&request, &state.authenticator)
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let (Some(auth), Some(surface)) = (verified.auth(), verified.surface()) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(subject) = auth.subject.as_deref().filter(|sub| {
        !sub.trim().is_empty() && sub.len() <= 512 && !sub.chars().any(char::is_control)
    }) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if auth.claims.get("iss").and_then(Value::as_str) != Some(surface.issuer.as_str())
        || auth.claims.get("sub").and_then(Value::as_str) != Some(subject)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut values = request
        .headers()
        .get_all(state.google_header.as_str())
        .iter();
    let raw = values.next().and_then(|header| header.to_str().ok());
    if values.next().is_some() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(token) = canonical_token(raw, &state.google_header) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(principal) = serde_json::to_string(&(surface.issuer.as_str(), subject)) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    request.extensions_mut().insert(HostedPrincipal {
        principal: principal.into(),
        upstream_digest: digest(token.as_bytes()).into(),
        issued: Instant::now(),
        used: Arc::new(AtomicBool::new(false)),
    });
    next.run(request).await
}

pub(crate) struct HostedScratchpadVerifier {
    client: Client,
    max_sessions_per_principal: usize,
    userinfo: String,
    account_summaries: String,
}

impl HostedScratchpadVerifier {
    pub(crate) fn from_env(settings: &HttpSettings, server: &AnalyticsMcp) -> Result<Option<Self>> {
        let enabled = match std::env::var("GA4_MCP_ENABLE_HOSTED_SCRATCHPAD").as_deref() {
            Ok("1" | "true") => true,
            Ok("0" | "false") | Err(std::env::VarError::NotPresent) => false,
            _ => {
                return Err(anyhow!(
                    "GA4_MCP_ENABLE_HOSTED_SCRATCHPAD must be true or false"
                ));
            }
        };
        if !enabled {
            return Ok(None);
        }
        let auth = &settings.auth.auth_config;
        let root = server.scratchpad_sessions.config();
        if server.capability_profile() != CapabilityProfile::Scratchpad
            || server.upstream_token_source() != UpstreamTokenSource::RequestHeader
            || server
                .upstream_token_header()
                .eq_ignore_ascii_case("authorization")
            || server
                .upstream_token_header()
                .eq_ignore_ascii_case("proxy-authorization")
            || !server.client.uses_official_google_origins()
            || settings.allow_local_single_user_scratchpad_http
            || settings.tls_files().is_none()
            || !settings.auth.enabled
            || auth.mode != AuthMode::Jwks
            || !auth.strict_oauth
            || auth
                .issuer
                .as_deref()
                .is_none_or(|s| !s.starts_with("https://"))
            || auth.audience.as_deref().is_none_or(|s| s.trim().is_empty())
            || auth.required_scopes.is_empty()
            || settings
                .auth
                .resource_url
                .as_deref()
                .is_none_or(|s| !s.starts_with("https://") || !s.ends_with("/mcp"))
            || root.session_ttl > Duration::from_secs(900)
        {
            return Err(anyhow!(
                "hosted scratchpad requires strict authenticated JWKS with issuer, audience and scopes, native TLS, HTTPS MCP resource, separate request-only Google credentials, official Google origins, no local-single-user opt-in and a session TTL at most 900 seconds"
            ));
        }
        let limit = match std::env::var("GA4_MCP_HOSTED_MAX_SESSIONS_PER_PRINCIPAL") {
            Ok(s) => s
                .parse::<usize>()
                .map_err(|_| anyhow!("invalid hosted principal session cap"))?,
            Err(std::env::VarError::NotPresent) => root.max_sessions.min(4),
            Err(_) => return Err(anyhow!("invalid hosted principal session cap")),
        };
        if limit == 0 || limit > root.max_sessions {
            return Err(anyhow!(
                "hosted principal session cap must be positive and at most global capacity"
            ));
        }
        let client = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(Some(Self {
            client,
            max_sessions_per_principal: limit,
            userinfo: USERINFO.to_owned(),
            account_summaries: ACCOUNT_SUMMARIES.to_owned(),
        }))
    }

    pub(crate) async fn manager_for_request(
        &self,
        root: &ScratchpadSessionManager,
        context: &RequestContext<RoleServer>,
        request: &CallToolRequestParams,
        raw_token: Option<&str>,
        token_header: &str,
    ) -> Result<ScratchpadSessionManager, AnalyticsError> {
        let token = canonical_token(raw_token, token_header)?;
        let witness = context
            .extensions
            .get::<Parts>()
            .and_then(|parts| parts.extensions.get::<HostedPrincipal>())
            .ok_or_else(denied)?;
        let principal = witness.claim(&token)?;
        if context.ct.is_cancelled() {
            return Err(denied());
        }
        let (subject, properties) =
            tokio::time::timeout(Duration::from_secs(20), self.verify_google(&token))
                .await
                .map_err(|_| denied())??;
        if context.ct.is_cancelled() {
            return Err(denied());
        }
        // New ingest tools must explicitly participate in the same property check.
        if request.name.starts_with("scratchpad_ingest_") {
            let property: PropertyId = serde_json::from_value(
                request
                    .arguments
                    .as_ref()
                    .and_then(|args| args.get("property_id"))
                    .cloned()
                    .ok_or_else(denied)?,
            )
            .map_err(|_| denied())?;
            if !properties.contains(&property.to_resource_name()?) {
                return Err(denied());
            }
        }
        let scope = bound_scope(principal, &subject, &token, &properties)?;
        root.for_scope(scope, self.max_sessions_per_principal)
            .map_err(AnalyticsError::from)
    }

    async fn verify_google(
        &self,
        token: &str,
    ) -> Result<(String, BTreeSet<String>), AnalyticsError> {
        let info = bounded_json(self.client.get(&self.userinfo).bearer_auth(token)).await?;
        let subject = info
            .get("sub")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty() && s.len() <= 255 && !s.chars().any(char::is_control))
            .ok_or_else(denied)?
            .to_string();
        let mut properties = BTreeSet::new();
        let mut seen = HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut req = self
                .client
                .get(&self.account_summaries)
                .bearer_auth(token)
                .query(&[
                    ("pageSize", "100"),
                    (
                        "fields",
                        "accountSummaries(propertySummaries(property)),nextPageToken",
                    ),
                ]);
            if let Some(page) = &cursor {
                req = req.query(&[("pageToken", page)]);
            }
            let page = bounded_json(req).await?;
            collect_properties(&page, &mut properties)?;
            cursor = match page.get("nextPageToken") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if s.is_empty() => None,
                Some(Value::String(s)) if s.len() <= 4096 && seen.insert(s.clone()) => {
                    Some(s.clone())
                }
                _ => return Err(denied()),
            };
            if cursor.is_none() {
                return Ok((subject, properties));
            }
        }
        // Partial authority inventories must never select a persisted namespace.
        Err(denied())
    }
}

fn bound_scope(
    principal: &str,
    subject: &str,
    token: &str,
    properties: &BTreeSet<String>,
) -> Result<ScratchpadScope, AnalyticsError> {
    let serialized = serde_json::to_vec(properties).map_err(|_| denied())?;
    // Include the verified Google account and credential epoch plus current
    // accessible-property snapshot. Rotation or permission changes invalidate
    // existing namespaces rather than silently inheriting old cached data.
    let upstream = serde_json::to_string(&(subject, digest(token.as_bytes()), digest(&serialized)))
        .map_err(|_| denied())?;
    ScratchpadScope::new(principal, upstream).map_err(AnalyticsError::from)
}

fn collect_properties(
    page: &Value,
    properties: &mut BTreeSet<String>,
) -> Result<(), AnalyticsError> {
    if !page.is_object() {
        return Err(denied());
    }
    let Some(accounts) = page.get("accountSummaries") else {
        return Ok(());
    };
    for account in accounts.as_array().ok_or_else(denied)? {
        if !account.is_object() {
            return Err(denied());
        }
        let Some(summaries) = account.get("propertySummaries") else {
            continue;
        };
        for summary in summaries.as_array().ok_or_else(denied)? {
            let name = summary
                .get("property")
                .and_then(Value::as_str)
                .ok_or_else(denied)?;
            let canonical = PropertyId::Text(name.to_string()).to_resource_name()?;
            if canonical != name || name == "properties/0" {
                return Err(denied());
            }
            properties.insert(canonical);
            if properties.len() > MAX_PROPERTIES {
                return Err(denied());
            }
        }
    }
    Ok(())
}

async fn bounded_json(request: RequestBuilder) -> Result<Value, AnalyticsError> {
    let mut response = request.send().await.map_err(|_| denied())?;
    if !response.status().is_success() {
        return Err(denied());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| denied())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(denied());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| denied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratchpad::{DuckDbEngine, ScratchpadSessionConfig, SharedScratchpadEngine};
    use serde_json::json;

    #[test]
    fn witness_is_single_use_exact_and_expires() {
        let token = "fixture-google-token";
        let witness = HostedPrincipal {
            principal: "verified-principal".into(),
            upstream_digest: digest(token.as_bytes()).into(),
            issued: Instant::now(),
            used: Arc::new(AtomicBool::new(false)),
        };
        assert!(witness.claim("different").is_err());
        assert!(witness.claim(token).is_ok());
        assert!(witness.clone().claim(token).is_err());
        let expired = HostedPrincipal {
            issued: Instant::now() - REQUEST_WITNESS_TTL,
            used: Arc::new(AtomicBool::new(false)),
            ..witness
        };
        assert!(expired.claim(token).is_err());
        assert!(canonical_token(Some("not valid token"), "x-google-token").is_err());
    }

    #[test]
    fn malformed_authority_inventory_is_never_treated_as_empty() {
        for page in [
            json!({"accountSummaries": null}),
            json!({"accountSummaries": {}}),
            json!({"accountSummaries": [{"propertySummaries": null}]}),
            json!({"accountSummaries": [{"propertySummaries": {}}]}),
            json!({"accountSummaries": [{"propertySummaries": [{}]}]}),
        ] {
            assert!(collect_properties(&page, &mut BTreeSet::new()).is_err());
        }
        assert!(collect_properties(&json!({}), &mut BTreeSet::new()).is_ok());
    }

    #[tokio::test]
    async fn provider_identity_requires_complete_valid_bounded_authority() {
        use axum::{Json, Router, extract::Query, http::HeaderMap, routing::get};
        use std::collections::HashMap;

        async fn userinfo(headers: HeaderMap) -> (StatusCode, Json<Value>) {
            let subject = match headers.get("authorization").and_then(|h| h.to_str().ok()) {
                Some("Bearer fixture-a") => "google-a",
                Some("Bearer fixture-b") => "google-b",
                Some("Bearer fixture-repeat") => "google-repeat",
                Some("Bearer fixture-malformed") => "google-malformed",
                _ => return (StatusCode::UNAUTHORIZED, Json(json!({}))),
            };
            (StatusCode::OK, Json(json!({"sub": subject})))
        }

        async fn summaries(
            headers: HeaderMap,
            Query(query): Query<HashMap<String, String>>,
        ) -> (StatusCode, Json<Value>) {
            let result = match headers.get("authorization").and_then(|h| h.to_str().ok()) {
                Some("Bearer fixture-a") if !query.contains_key("pageToken") => {
                    json!({"accountSummaries":[{"propertySummaries":[{"property":"properties/42"}]}],"nextPageToken":"next"})
                }
                Some("Bearer fixture-a")
                    if query.get("pageToken").map(String::as_str) == Some("next") =>
                {
                    json!({"accountSummaries":[{"propertySummaries":[{"property":"properties/43"}]}]})
                }
                Some("Bearer fixture-b") => {
                    json!({"accountSummaries":[{"propertySummaries":[{"property":"properties/99"}]}]})
                }
                Some("Bearer fixture-repeat") => json!({"nextPageToken":"same"}),
                Some("Bearer fixture-malformed") => json!({"accountSummaries": null}),
                _ => return (StatusCode::FORBIDDEN, Json(json!({}))),
            };
            (StatusCode::OK, Json(result))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/userinfo", get(userinfo))
            .route("/summaries", get(summaries));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // Private test construction only; production uses fixed HTTPS URLs.
        let verifier = HostedScratchpadVerifier {
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            max_sessions_per_principal: 4,
            userinfo: format!("{origin}/userinfo"),
            account_summaries: format!("{origin}/summaries"),
        };
        let (subject, props) = verifier.verify_google("fixture-a").await.unwrap();
        assert_eq!(subject, "google-a");
        assert_eq!(
            props,
            BTreeSet::from(["properties/42".to_owned(), "properties/43".to_owned()])
        );
        let (subject, props) = verifier.verify_google("fixture-b").await.unwrap();
        assert_eq!(subject, "google-b");
        assert_eq!(props, BTreeSet::from(["properties/99".to_owned()]));
        for token in ["fixture-invalid", "fixture-repeat", "fixture-malformed"] {
            assert!(verifier.verify_google(token).await.is_err());
        }
        server.abort();
    }

    #[test]
    fn account_rotation_permission_changes_and_other_principals_never_share_data() {
        let mut props = BTreeSet::new();
        collect_properties(
            &json!({"accountSummaries":[{"propertySummaries":[{"property":"properties/42"}]}]}),
            &mut props,
        )
        .unwrap();
        assert!(collect_properties(&json!({"accountSummaries":[{"propertySummaries":[{"property":"properties/../../"}]}]}), &mut BTreeSet::new()).is_err());
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().unwrap());
        let root = ScratchpadSessionManager::new(
            engine,
            ScratchpadSessionConfig::new(Duration::from_secs(60), 8, 4, 100, 128),
        )
        .unwrap();
        let a = root
            .for_scope(
                bound_scope("issuer/a", "google-a", "token-a", &props).unwrap(),
                4,
            )
            .unwrap();
        a.open_connection("same")
            .unwrap()
            .execute_batch("CREATE TABLE private_data AS SELECT 11 AS value")
            .unwrap();
        for scope in [
            bound_scope("issuer/b", "google-a", "token-a", &props).unwrap(),
            bound_scope("issuer/a", "google-b", "token-a", &props).unwrap(),
            bound_scope("issuer/a", "google-a", "token-rotated", &props).unwrap(),
            bound_scope("issuer/a", "google-a", "token-a", &BTreeSet::new()).unwrap(),
        ] {
            let b = root.for_scope(scope, 4).unwrap();
            assert!(b.session_info("same").is_err());
            assert!(b.list_sessions(10).unwrap().is_empty());
            assert!(b.set_max_sessions_limit(99).is_err());
        }
        let same = root
            .for_scope(
                bound_scope("issuer/a", "google-a", "token-a", &props).unwrap(),
                4,
            )
            .unwrap();
        assert_eq!(same.list_tables("same", 10).unwrap().len(), 1);
    }
}
