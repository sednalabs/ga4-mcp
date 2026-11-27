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
    quota_project: reqwest::header::HeaderValue,
    max_sessions_per_principal: usize,
    #[cfg(test)]
    dispatch_fixture: bool,
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
            || server
                .upstream_token_header()
                .eq_ignore_ascii_case("x-goog-user-project")
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
        let quota_project = server
            .client
            .quota_project()
            .filter(|project| !project.trim().is_empty())
            .ok_or_else(|| {
                anyhow!("hosted scratchpad requires an operator-configured quota project")
            })?;
        let quota_project = reqwest::header::HeaderValue::from_str(quota_project)
            .map_err(|_| anyhow!("invalid hosted scratchpad quota project"))?;
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
            quota_project,
            max_sessions_per_principal: limit,
            #[cfg(test)]
            dispatch_fixture: false,
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
        #[cfg(test)]
        if self.dispatch_fixture {
            return self
                .verify_google_with(token, tests::dispatch_provider_fixture)
                .await;
        }
        self.verify_google_with(token, bounded_json).await
    }

    async fn verify_google_with<F, Fut>(
        &self,
        token: &str,
        mut send: F,
    ) -> Result<(String, BTreeSet<String>), AnalyticsError>
    where
        F: FnMut(RequestBuilder) -> Fut,
        Fut: std::future::Future<Output = Result<Value, AnalyticsError>>,
    {
        // Provider destinations are fixed HTTPS origins even in fixture tests.
        // Only the response transport is replaceable, never the credential target.
        let info = send(self.client.get(USERINFO).bearer_auth(token)).await?;
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
                .get(ACCOUNT_SUMMARIES)
                .bearer_auth(token)
                .header("x-goog-user-project", self.quota_project.clone())
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
            let page = send(req).await?;
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
        use std::collections::HashMap;

        async fn fixture(request: RequestBuilder) -> Result<Value, AnalyticsError> {
            let request = request.build().map_err(|_| denied())?;
            assert_eq!(request.method(), reqwest::Method::GET);
            assert_eq!(request.url().scheme(), "https");
            let authorization = request
                .headers()
                .get("authorization")
                .and_then(|h| h.to_str().ok())
                .ok_or_else(denied)?;
            match request.url().host_str() {
                Some("openidconnect.googleapis.com") => {
                    assert_eq!(request.url().path(), "/v1/userinfo");
                    assert!(request.url().query().is_none());
                    assert!(!request.headers().contains_key("x-goog-user-project"));
                    let subject = match authorization {
                        "Bearer fixture-a" => "google-a",
                        "Bearer fixture-b" => "google-b",
                        "Bearer fixture-repeat" => "google-repeat",
                        "Bearer fixture-malformed" => "google-malformed",
                        _ => return Err(denied()),
                    };
                    Ok(json!({"sub": subject}))
                }
                Some("analyticsadmin.googleapis.com") => {
                    assert_eq!(request.url().path(), "/v1beta/accountSummaries");
                    assert_eq!(
                        request.headers().get("x-goog-user-project").unwrap(),
                        "operator-fixture"
                    );
                    let query: HashMap<_, _> = request.url().query_pairs().collect();
                    assert_eq!(query.get("pageSize").map(|s| s.as_ref()), Some("100"));
                    assert_eq!(
                        query.get("fields").map(|s| s.as_ref()),
                        Some("accountSummaries(propertySummaries(property)),nextPageToken")
                    );
                    let page = match authorization {
                        "Bearer fixture-a" if !query.contains_key("pageToken") => {
                            json!({"accountSummaries":[{"propertySummaries":[{"property":"properties/42"}]}],"nextPageToken":"next"})
                        }
                        "Bearer fixture-a"
                            if query.get("pageToken").map(|s| s.as_ref()) == Some("next") =>
                        {
                            json!({"accountSummaries":[{"propertySummaries":[{"property":"properties/43"}]}]})
                        }
                        "Bearer fixture-b" => {
                            json!({"accountSummaries":[{"propertySummaries":[{"property":"properties/99"}]}]})
                        }
                        "Bearer fixture-repeat" => json!({"nextPageToken":"same"}),
                        "Bearer fixture-malformed" => json!({"accountSummaries": null}),
                        _ => return Err(denied()),
                    };
                    Ok(page)
                }
                _ => panic!("unexpected fixture origin"),
            }
        }

        let verifier = HostedScratchpadVerifier {
            client: Client::builder()
                .https_only(true)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            quota_project: reqwest::header::HeaderValue::from_static("operator-fixture"),
            max_sessions_per_principal: 4,
            dispatch_fixture: false,
        };
        let (subject, props) = verifier
            .verify_google_with("fixture-a", fixture)
            .await
            .unwrap();
        assert_eq!(subject, "google-a");
        assert_eq!(
            props,
            BTreeSet::from(["properties/42".to_owned(), "properties/43".to_owned()])
        );
        let (subject, props) = verifier
            .verify_google_with("fixture-b", fixture)
            .await
            .unwrap();
        assert_eq!(subject, "google-b");
        assert_eq!(props, BTreeSet::from(["properties/99".to_owned()]));
        for token in ["fixture-invalid", "fixture-repeat", "fixture-malformed"] {
            assert!(verifier.verify_google_with(token, fixture).await.is_err());
        }
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

    // Only compiled in the test binary. The normal provider adapter has no
    // injectable destination or transport; the fixture inspects the real HTTPS
    // request without ever sending it.
    pub(super) async fn dispatch_provider_fixture(
        request: RequestBuilder,
    ) -> Result<Value, AnalyticsError> {
        let request = request.build().map_err(|_| denied())?;
        assert_eq!(request.url().scheme(), "https");
        let subject = match request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        {
            Some("Bearer fixture-google-a" | "Bearer fixture-google-a-rotated") => "google-a",
            Some("Bearer fixture-google-b") => "google-b",
            _ => return Err(denied()),
        };
        match request.url().host_str() {
            Some("openidconnect.googleapis.com") => {
                assert_eq!(request.url().path(), "/v1/userinfo");
                assert!(!request.headers().contains_key("x-goog-user-project"));
                Ok(json!({"sub": subject}))
            }
            Some("analyticsadmin.googleapis.com") => {
                assert_eq!(request.url().path(), "/v1beta/accountSummaries");
                assert_eq!(
                    request.headers().get("x-goog-user-project").unwrap(),
                    "operator-fixture"
                );
                Ok(
                    json!({"accountSummaries":[{"propertySummaries":[{"property":"properties/42"}]}]}),
                )
            }
            _ => panic!("unexpected fixture origin"),
        }
    }

    #[tokio::test]
    async fn actual_http_dispatch_isolates_verified_principals_and_rejects_limit_changes() {
        use axum::{Json, Router, middleware, routing::get};
        use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
        use mcp_toolkit_auth::{
            AuthConfig,
            surface::{AuthSurfaceConfig, AuthSurfaceLayer, IssuerEntry},
        };
        use mcp_toolkit_http::streamable::{
            LocalStreamableHttpServiceConfig, build_local_streamable_http_service,
        };
        use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
        use std::time::{SystemTime, UNIX_EPOCH};

        const ISSUER: &str = "https://issuer.fixture.test";
        const RESOURCE: &str = "https://resource.fixture.test/mcp";
        // Public, non-production ES256 test fixture from toolkit internal_tests.rs.
        // It is unrelated to any real service or credential.
        const FIXTURE_KEY: &str = concat!(
            "-----BEGIN PRIVATE KEY-----\n",
            "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgWTFfCGljY6aw3Hrt\n",
            "kHmPRiazukxPLb6ilpRAewjW8nihRANCAATDskChT+Altkm9X7MI69T3IUmrQU0L\n",
            "950IxEzvw/x5BMEINRMrXLBJhqzO9Bm+d6JbqA21YQmd1Kt4RzLJR1W+\n",
            "-----END PRIVATE KEY-----\n"
        );
        fn signed_token(subject: &str, scopes: &str) -> String {
            let exp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 300;
            let mut header = Header::new(Algorithm::ES256);
            header.kid = Some("fixture-ec-key".to_owned());
            encode(
                &header,
                &json!({"exp":exp,"iss":ISSUER,"aud":RESOURCE,"sub":subject,"scope":scopes}),
                &EncodingKey::from_ec_pem(FIXTURE_KEY.as_bytes()).unwrap(),
            )
            .unwrap()
        }

        let jwks = Router::new().route(
            "/jwks",
            get(|| async {
                Json(json!({"keys":[{
                    "kty":"EC","crv":"P-256",
                    "x":"w7JAoU_gJbZJvV-zCOvU9yFJq0FNC_edCMRM78P8eQQ",
                    "y":"wQg1EytcsEmGrM70Gb53oluoDbVhCZ3Uq3hHMslHVb4",
                    "kid":"fixture-ec-key","alg":"ES256","use":"sig"
                }]}))
            }),
        );
        let jwks_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let jwks_url = format!("http://{}/jwks", jwks_listener.local_addr().unwrap());
        let jwks_task =
            tokio::spawn(async move { axum::serve(jwks_listener, jwks).await.unwrap() });
        let authenticator = Arc::new(
            Authenticator::new(AuthConfig {
                mode: AuthMode::Jwks,
                strict_oauth: true,
                issuer: Some(ISSUER.to_owned()),
                audience: Some(RESOURCE.to_owned()),
                jwks_url: Some(jwks_url.clone()),
                required_scopes: vec!["analytics:read".to_owned()],
                jti_ttl_s: 0.0,
                jti_cache_size: 0,
                ..Default::default()
            })
            .unwrap(),
        );
        let mut surface = AuthSurfaceConfig::single_issuer(
            "https://resource.fixture.test".to_owned(),
            IssuerEntry {
                resource_path: "/mcp".to_owned(),
                issuer: ISSUER.to_owned(),
                authorization_endpoint: format!("{ISSUER}/authorize"),
                token_endpoint: format!("{ISSUER}/token"),
                registration_endpoint: None,
                jwks_uri: Some(jwks_url),
                introspection_endpoint: None,
                device_authorization_endpoint: None,
                grant_types_supported: None,
                client_id_metadata_document_supported: None,
                token_endpoint_auth_methods_supported: None,
                code_challenge_methods_supported: None,
                realm: "fixture".to_owned(),
                scopes_supported: vec!["analytics:read".to_owned()],
                allowed_client_ids: HashSet::new(),
                authenticator: authenticator.clone(),
                resource_url_override: Some(RESOURCE.to_owned()),
            },
        );
        // Loopback JWKS fixture only. Production startup still requires TLS.
        surface.allow_insecure_http = true;
        let auth_layer = AuthSurfaceLayer::from_config(surface).unwrap();

        let settings = crate::config::Settings {
            analytics_scope: crate::config::DEFAULT_ANALYTICS_SCOPE.to_owned(),
            admin_base_url: "https://analyticsadmin.googleapis.com".to_owned(),
            data_base_url: "https://analyticsdata.googleapis.com".to_owned(),
            http_timeout: Duration::from_secs(2),
            max_page_size: 100,
            max_pages: 2,
            user_agent: "ga4-hosted-dispatch-fixture".to_owned(),
            oauth_client_secret_json: None,
            oauth_refresh_token: None,
            upstream_token_source: UpstreamTokenSource::RequestHeader,
            upstream_token_header: "x-google-token".to_owned(),
            quota_project: Some("operator-fixture".to_owned()),
            shared_adc: true,
            scratchpad_session_ttl: Duration::from_secs(60),
            scratchpad_max_sessions: 2,
            scratchpad_max_tables_per_session: 4,
            scratchpad_max_rows_per_session: 100,
            scratchpad_max_memory_mb: 128,
            scratchpad_query_timeout: Duration::from_secs(2),
            scratchpad_max_sql_bytes: 4096,
            capability_profile: CapabilityProfile::Scratchpad,
            print_tools: false,
            print_tool_schema: false,
            command: None,
        };
        let client = Arc::new(
            crate::ga_client::AnalyticsApiClient::from_settings(&settings)
                .await
                .unwrap(),
        );
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().unwrap());
        let root = Arc::new(
            ScratchpadSessionManager::new(
                engine,
                ScratchpadSessionConfig::new(Duration::from_secs(60), 2, 4, 100, 128),
            )
            .unwrap(),
        );
        let mut server = AnalyticsMcp::new(
            client,
            root.clone(),
            CapabilityProfile::Scratchpad,
            UpstreamTokenSource::RequestHeader,
            "x-google-token".to_owned(),
        );
        server.hosted_scratchpad = Some(Arc::new(HostedScratchpadVerifier {
            client: Client::builder()
                .https_only(true)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            quota_project: reqwest::header::HeaderValue::from_static("operator-fixture"),
            max_sessions_per_principal: 1,
            dispatch_fixture: true,
        }));
        let runtime = build_local_streamable_http_service(
            move || Ok(server.clone()),
            LocalStreamableHttpServiceConfig {
                server_config: StreamableHttpServerConfig::default()
                    .with_allowed_hosts(["127.0.0.1".to_owned()])
                    .with_legacy_session_mode(false)
                    .with_json_response(true),
                ..Default::default()
            },
        );
        let app = Router::new()
            .nest_service("/mcp", runtime.service)
            .layer(middleware::from_fn_with_state(
                HostedAuthGuard::new(authenticator, "x-google-token".to_owned()),
                verify_hosted_mcp_request,
            ))
            .layer(auth_layer);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        let call = |token: String, google: &'static str, tool: &'static str, args: Value| {
            http.post(&endpoint)
                .bearer_auth(token)
                .header("x-google-token", google)
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", "2026-07-28")
                .header("mcp-method", "tools/call")
                .header("mcp-name", tool)
                .json(&json!({
                    "jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":tool,"arguments":args,"_meta":{
                        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                        "io.modelcontextprotocol/clientInfo":{"name":"fixture","version":"1"},
                        "io.modelcontextprotocol/clientCapabilities":{}
                    }}
                }))
        };
        async fn payload(request: RequestBuilder) -> Value {
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!response.headers().contains_key("mcp-session-id"));
            let envelope: Value = response.json().await.unwrap();
            assert!(envelope.get("error").is_none(), "{envelope}");
            let payload = &envelope["result"]["structuredContent"];
            assert!(payload["ok"].is_boolean(), "{envelope}");
            payload.clone()
        }

        let a = signed_token("mcp-a", "analytics:read");
        let b = signed_token("mcp-b", "analytics:read");
        let session = json!({"session_id":"same"});
        let (a_open, b_open) = tokio::join!(
            payload(call(
                a.clone(),
                "fixture-google-a",
                "scratchpad_open_session",
                session.clone()
            )),
            payload(call(
                b.clone(),
                "fixture-google-b",
                "scratchpad_open_session",
                session.clone()
            ))
        );
        assert_eq!(a_open["ok"], true);
        assert_eq!(b_open["ok"], true);

        let properties = BTreeSet::from(["properties/42".to_owned()]);
        for (mcp_subject, google_subject, google_token, sql) in [
            (
                "mcp-a",
                "google-a",
                "fixture-google-a",
                "CREATE TABLE private_data AS SELECT 'only-a' AS marker",
            ),
            (
                "mcp-b",
                "google-b",
                "fixture-google-b",
                "CREATE TABLE private_data AS SELECT 'only-b' AS marker",
            ),
        ] {
            let principal = serde_json::to_string(&(ISSUER, mcp_subject)).unwrap();
            let view = root
                .for_scope(
                    bound_scope(&principal, google_subject, google_token, &properties).unwrap(),
                    1,
                )
                .unwrap();
            view.open_connection("same")
                .unwrap()
                .execute_batch(sql)
                .unwrap();
        }
        let query = json!({"session_id":"same","sql":"SELECT marker FROM private_data"});
        let (a_query, b_query) = tokio::join!(
            payload(call(
                a.clone(),
                "fixture-google-a",
                "scratchpad_query",
                query.clone()
            )),
            payload(call(
                b.clone(),
                "fixture-google-b",
                "scratchpad_query",
                query.clone()
            ))
        );
        assert_eq!(a_query["ok"], true);
        assert_eq!(b_query["ok"], true);
        assert!(a_query["data"].to_string().contains("only-a"));
        assert!(!a_query["data"].to_string().contains("only-b"));
        assert!(b_query["data"].to_string().contains("only-b"));
        assert!(!b_query["data"].to_string().contains("only-a"));

        for (token, google) in [
            (a.clone(), "fixture-google-a-rotated"),
            (a.clone(), "fixture-google-b"),
            (b.clone(), "fixture-google-a"),
        ] {
            let listing = payload(call(token, google, "scratchpad_list_sessions", json!({}))).await;
            assert_eq!(listing["ok"], true);
            assert_eq!(listing["data"]["returned"], 0);
        }

        let limit = payload(call(
            a.clone(),
            "fixture-google-a",
            "scratchpad_set_runtime_limits",
            json!({"max_sessions":99}),
        ))
        .await;
        assert_eq!(limit["ok"], false);
        assert_eq!(root.max_sessions_limit(), 2);
        let extra = payload(call(
            a.clone(),
            "fixture-google-a",
            "scratchpad_open_session",
            json!({"session_id":"extra"}),
        ))
        .await;
        assert_eq!(extra["ok"], false);

        let dropped = payload(call(
            a.clone(),
            "fixture-google-a",
            "scratchpad_drop_table",
            json!({"session_id":"same","table_name":"private_data"}),
        ))
        .await;
        assert_eq!(dropped["ok"], true);
        let still_b = payload(call(
            b.clone(),
            "fixture-google-b",
            "scratchpad_query",
            query,
        ))
        .await;
        assert_eq!(still_b["ok"], true);
        assert!(still_b["data"].to_string().contains("only-b"));

        let invalid = call(
            signed_token("mcp-a", "wrong:scope"),
            "fixture-google-a",
            "scratchpad_list_sessions",
            json!({}),
        )
        .send()
        .await
        .unwrap();
        assert!(matches!(
            invalid.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ));
        let duplicate = call(
            a.clone(),
            "fixture-google-a",
            "scratchpad_list_sessions",
            json!({}),
        )
        .header("x-google-token", "fixture-google-b")
        .send()
        .await
        .unwrap();
        assert_eq!(duplicate.status(), StatusCode::UNAUTHORIZED);
        let invalid_google = payload(call(
            a.clone(),
            "fixture-google-invalid",
            "scratchpad_list_sessions",
            json!({}),
        ))
        .await;
        assert_eq!(invalid_google["ok"], false);
        let legacy = call(a, "fixture-google-a", "scratchpad_list_sessions", json!({}))
            .header("mcp-session-id", "not-authority")
            .send()
            .await
            .unwrap();
        assert_eq!(legacy.status(), StatusCode::METHOD_NOT_ALLOWED);

        task.abort();
        jwks_task.abort();
    }
}
