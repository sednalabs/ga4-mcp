//! # Google Analytics API Client
//!
//! Thin adapter around Google Analytics Admin/Data REST endpoints.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::future::Future;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gcp_auth::TokenProvider;
use mcp_toolkit_auth::upstream_oauth::{
    RefreshTokenProvider, UpstreamOAuthError, google_authorized_user_adc_from_file,
};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, Method, RequestBuilder, Url};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{OnceCell, RwLock};

use crate::config::{
    Settings, UpstreamTokenSource, conventional_adc_credentials_path,
    conventional_cloudsdk_config_dir, server_adc_credentials_path, server_cloudsdk_config_dir,
};
use crate::error::AnalyticsError;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PropertyId {
    Number(u64),
    Text(String),
}

impl PropertyId {
    pub fn to_resource_name(&self) -> Result<String, AnalyticsError> {
        parse_resource_name(
            self,
            "property_id",
            "properties",
            "expected a number or 'properties/<number>'",
        )
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AccountId {
    Number(u64),
    Text(String),
}

impl AccountId {
    pub fn to_resource_name(&self) -> Result<String, AnalyticsError> {
        parse_resource_name(
            self,
            "account_id",
            "accounts",
            "expected a number or 'accounts/<number>'",
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
struct OAuthClientSecretFile {
    #[serde(default)]
    installed: Option<OAuthClientConfig>,
    #[serde(default)]
    web: Option<OAuthClientConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct OAuthClientConfig {
    client_id: String,
    client_secret: String,
    #[serde(default)]
    token_uri: Option<String>,
}

#[derive(Debug, Clone)]
struct OAuthRefreshConfig {
    token_uri: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct OAuthRefreshResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Clone)]
struct CachedAccessToken {
    value: String,
    refresh_after: Instant,
}

#[derive(Debug, Clone)]
enum UpstreamAuthMode {
    RequestHeaderOnly,
    Adc,
    AuthorizedUserAdcFile(PathBuf),
    OAuthRefresh(Arc<OAuthRefreshConfig>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSource {
    RequestHeader,
    GoogleDefaultProviderChain,
    GoogleAuthorizedUserAdcFile,
    OAuthRefreshToken,
}

impl AuthSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RequestHeader => "request_header",
            Self::GoogleDefaultProviderChain => "google_default_provider_chain",
            Self::GoogleAuthorizedUserAdcFile => "google_authorized_user_adc_file",
            Self::OAuthRefreshToken => "oauth_refresh_token",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PaginationOptions {
    pub page_size: Option<u32>,
    pub max_pages: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct RunReportRequest {
    pub property_id: PropertyId,
    pub date_ranges: Vec<Value>,
    pub dimensions: Vec<String>,
    pub metrics: Vec<String>,
    pub dimension_filter: Option<Value>,
    pub metric_filter: Option<Value>,
    pub order_bys: Option<Vec<Value>>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub currency_code: Option<String>,
    pub return_property_quota: bool,
}

#[derive(Debug, Clone)]
pub struct RunRealtimeReportRequest {
    pub property_id: PropertyId,
    pub dimensions: Vec<String>,
    pub metrics: Vec<String>,
    pub dimension_filter: Option<Value>,
    pub metric_filter: Option<Value>,
    pub order_bys: Option<Vec<Value>>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub return_property_quota: bool,
}

#[derive(Debug, Clone)]
pub struct RunPivotReportRequest {
    pub property_id: PropertyId,
    pub date_ranges: Vec<Value>,
    pub dimensions: Vec<String>,
    pub metrics: Vec<String>,
    pub pivots: Vec<Value>,
    pub dimension_filter: Option<Value>,
    pub metric_filter: Option<Value>,
    pub order_bys: Option<Vec<Value>>,
    pub currency_code: Option<String>,
    pub keep_empty_rows: bool,
    pub return_property_quota: bool,
}

#[derive(Debug, Clone)]
pub struct BatchRunReportItemRequest {
    pub date_ranges: Vec<Value>,
    pub dimensions: Vec<String>,
    pub metrics: Vec<String>,
    pub dimension_filter: Option<Value>,
    pub metric_filter: Option<Value>,
    pub order_bys: Option<Vec<Value>>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub currency_code: Option<String>,
    pub return_property_quota: bool,
}

#[derive(Debug, Clone)]
pub struct BatchRunReportsRequest {
    pub property_id: PropertyId,
    pub requests: Vec<BatchRunReportItemRequest>,
}

#[derive(Debug, Clone)]
pub struct RunAccessReportRequest {
    pub date_ranges: Vec<Value>,
    pub dimensions: Vec<String>,
    pub metrics: Vec<String>,
    pub dimension_filter: Option<Value>,
    pub metric_filter: Option<Value>,
    pub order_bys: Option<Vec<Value>>,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
    pub time_zone: Option<String>,
}

#[derive(Clone)]
pub struct AnalyticsApiClient {
    http: Client,
    auth_mode: UpstreamAuthMode,
    token_source: UpstreamTokenSource,
    token_header: Arc<str>,
    token_provider: Arc<OnceCell<Arc<dyn TokenProvider>>>,
    oauth_token_provider: Arc<OnceCell<Arc<RefreshTokenProvider>>>,
    cached_oauth_token: Arc<RwLock<Option<CachedAccessToken>>>,
    analytics_scope: Arc<str>,
    admin_base_url: Arc<str>,
    data_base_url: Arc<str>,
    quota_project: Option<Arc<str>>,
    max_page_size: u32,
    default_max_pages: u32,
}

tokio::task_local! {
    static REQUEST_ACCESS_TOKEN_OVERRIDE: Option<String>;
}

pub async fn with_request_access_token_override<F, T>(token: Option<String>, future: F) -> T
where
    F: Future<Output = T>,
{
    REQUEST_ACCESS_TOKEN_OVERRIDE.scope(token, future).await
}

impl AnalyticsApiClient {
    pub async fn from_settings(settings: &Settings) -> Result<Self, AnalyticsError> {
        let mut headers = HeaderMap::new();
        let user_agent = HeaderValue::from_str(&settings.user_agent).map_err(|err| {
            AnalyticsError::AuthBootstrap(format!("invalid user-agent value: {err}"))
        })?;
        headers.insert(USER_AGENT, user_agent);

        let http = Client::builder()
            .timeout(settings.http_timeout)
            .default_headers(headers)
            .build()
            .map_err(AnalyticsError::Transport)?;

        let auth_mode = select_auth_mode(settings)?;

        let quota_project = settings
            .quota_project
            .clone()
            .or_else(|| adc_quota_project_id_for_auth_mode(&auth_mode, settings.shared_adc));

        Ok(Self {
            http,
            auth_mode,
            token_source: settings.upstream_token_source,
            token_header: settings.upstream_token_header.clone().into(),
            token_provider: Arc::new(OnceCell::new()),
            oauth_token_provider: Arc::new(OnceCell::new()),
            cached_oauth_token: Arc::new(RwLock::new(None)),
            analytics_scope: settings.analytics_scope.clone().into(),
            admin_base_url: settings.admin_base_url.clone().into(),
            data_base_url: settings.data_base_url.clone().into(),
            quota_project: quota_project.as_deref().map(Arc::<str>::from),
            max_page_size: settings.max_page_size,
            default_max_pages: settings.max_pages,
        })
    }

    pub fn auth_source(&self) -> AuthSource {
        match &self.auth_mode {
            UpstreamAuthMode::RequestHeaderOnly => AuthSource::RequestHeader,
            UpstreamAuthMode::Adc => AuthSource::GoogleDefaultProviderChain,
            UpstreamAuthMode::AuthorizedUserAdcFile(_) => AuthSource::GoogleAuthorizedUserAdcFile,
            UpstreamAuthMode::OAuthRefresh(_) => AuthSource::OAuthRefreshToken,
        }
    }

    pub fn analytics_scope(&self) -> &str {
        self.analytics_scope.as_ref()
    }

    pub fn upstream_token_source(&self) -> UpstreamTokenSource {
        self.token_source
    }

    pub fn upstream_token_header(&self) -> &str {
        self.token_header.as_ref()
    }

    pub fn quota_project_configured(&self) -> bool {
        self.quota_project.is_some()
    }

    pub fn quota_project(&self) -> Option<&str> {
        self.quota_project.as_deref()
    }

    pub async fn verify_token(&self) -> Result<(), AnalyticsError> {
        self.get_account_summaries(PaginationOptions {
            page_size: Some(1),
            max_pages: Some(1),
        })
        .await
        .map(|_| ())
    }

    pub async fn verify_config_token(&self) -> Result<(), AnalyticsError> {
        let token = self.configured_access_token().await?;
        let url =
            parse_https_upstream_url(&format!("{}/v1beta/accountSummaries", self.admin_base_url))?;
        let mut request = self
            .http
            .request(Method::GET, url)
            .bearer_auth(token)
            .query(&[("pageSize", "1")]);
        if let Some(quota_project) = &self.quota_project {
            request = request.header("x-goog-user-project", quota_project.as_ref());
        }
        self.send_json(request).await.map(|_| ())
    }

    pub async fn get_account_summaries(
        &self,
        pagination: PaginationOptions,
    ) -> Result<Value, AnalyticsError> {
        self.collect_paginated(
            &format!("{}/v1beta/accountSummaries", self.admin_base_url),
            "accountSummaries",
            pagination,
        )
        .await
        .map(|(items, meta)| {
            json!({
                "account_summaries": items,
                "meta": meta,
            })
        })
    }

    pub async fn get_property_details(
        &self,
        property_id: &PropertyId,
    ) -> Result<Value, AnalyticsError> {
        let property = property_id.to_resource_name()?;
        self.get_json(&format!("{}/v1beta/{property}", self.admin_base_url), &[])
            .await
    }

    pub async fn get_account_data_sharing_settings(
        &self,
        account_id: &AccountId,
    ) -> Result<Value, AnalyticsError> {
        let account = account_id.to_resource_name()?;
        self.get_json(
            &format!(
                "{}/v1beta/{account}/dataSharingSettings",
                self.admin_base_url
            ),
            &[],
        )
        .await
    }

    pub async fn get_property_data_retention_settings(
        &self,
        property_id: &PropertyId,
    ) -> Result<Value, AnalyticsError> {
        let property = property_id.to_resource_name()?;
        self.get_json(
            &format!(
                "{}/v1beta/{property}/dataRetentionSettings",
                self.admin_base_url
            ),
            &[],
        )
        .await
    }

    pub async fn list_google_ads_links(
        &self,
        property_id: &PropertyId,
        pagination: PaginationOptions,
    ) -> Result<Value, AnalyticsError> {
        let property = property_id.to_resource_name()?;
        self.collect_paginated(
            &format!("{}/v1beta/{property}/googleAdsLinks", self.admin_base_url),
            "googleAdsLinks",
            pagination,
        )
        .await
        .map(|(items, meta)| {
            json!({
                "google_ads_links": items,
                "meta": meta,
            })
        })
    }

    pub async fn list_property_annotations(
        &self,
        property_id: &PropertyId,
        pagination: PaginationOptions,
    ) -> Result<Value, AnalyticsError> {
        let property = property_id.to_resource_name()?;
        self.collect_paginated(
            &format!(
                "{}/v1alpha/{property}/reportingDataAnnotations",
                self.admin_base_url
            ),
            "reportingDataAnnotations",
            pagination,
        )
        .await
        .map(|(items, meta)| {
            json!({
                "reporting_data_annotations": items,
                "meta": meta,
            })
        })
    }

    pub async fn run_report(&self, request: RunReportRequest) -> Result<Value, AnalyticsError> {
        let property = request.property_id.to_resource_name()?;
        let payload = build_run_report_payload(
            request.date_ranges,
            request.dimensions,
            request.metrics,
            request.dimension_filter,
            request.metric_filter,
            request.order_bys,
            request.limit,
            request.offset,
            request.currency_code,
            request.return_property_quota,
        );

        self.post_json(
            &format!("{}/v1beta/{property}:runReport", self.data_base_url),
            Value::Object(payload),
        )
        .await
    }

    pub async fn check_report_compatibility(
        &self,
        property_id: &PropertyId,
        dimensions: &[String],
        metrics: &[String],
    ) -> Result<Value, AnalyticsError> {
        let property = property_id.to_resource_name()?;
        let payload = json!({
            "dimensions": dimensions
                .iter()
                .map(|name| json!({"name": name}))
                .collect::<Vec<_>>(),
            "metrics": metrics
                .iter()
                .map(|name| json!({"name": name}))
                .collect::<Vec<_>>(),
        });
        self.post_json(
            &format!(
                "{}/v1beta/{property}:checkCompatibility",
                self.data_base_url
            ),
            payload,
        )
        .await
    }

    pub async fn run_realtime_report(
        &self,
        request: RunRealtimeReportRequest,
    ) -> Result<Value, AnalyticsError> {
        let property = request.property_id.to_resource_name()?;
        let mut payload = Map::new();
        payload.insert(
            "dimensions".to_string(),
            Value::Array(as_named_entries(&request.dimensions)),
        );
        payload.insert(
            "metrics".to_string(),
            Value::Array(as_named_entries(&request.metrics)),
        );
        payload.insert(
            "returnPropertyQuota".to_string(),
            Value::Bool(request.return_property_quota),
        );
        if let Some(filter) = request.dimension_filter {
            payload.insert("dimensionFilter".to_string(), snake_to_camel_json(filter));
        }
        if let Some(filter) = request.metric_filter {
            payload.insert("metricFilter".to_string(), snake_to_camel_json(filter));
        }
        if let Some(order_bys) = request.order_bys {
            payload.insert(
                "orderBys".to_string(),
                Value::Array(order_bys.into_iter().map(snake_to_camel_json).collect()),
            );
        }
        if let Some(limit) = request.limit {
            payload.insert("limit".to_string(), Value::String(limit.to_string()));
        }
        if let Some(offset) = request.offset {
            payload.insert("offset".to_string(), Value::String(offset.to_string()));
        }

        self.post_json(
            &format!("{}/v1beta/{property}:runRealtimeReport", self.data_base_url),
            Value::Object(payload),
        )
        .await
    }

    pub async fn run_pivot_report(
        &self,
        request: RunPivotReportRequest,
    ) -> Result<Value, AnalyticsError> {
        let property = request.property_id.to_resource_name()?;
        let mut payload = Map::new();
        payload.insert(
            "dimensions".to_string(),
            Value::Array(as_named_entries(&request.dimensions)),
        );
        payload.insert(
            "metrics".to_string(),
            Value::Array(as_named_entries(&request.metrics)),
        );
        payload.insert(
            "dateRanges".to_string(),
            Value::Array(
                request
                    .date_ranges
                    .into_iter()
                    .map(snake_to_camel_json)
                    .collect(),
            ),
        );
        payload.insert(
            "pivots".to_string(),
            Value::Array(
                request
                    .pivots
                    .into_iter()
                    .map(snake_to_camel_json)
                    .collect(),
            ),
        );
        payload.insert(
            "keepEmptyRows".to_string(),
            Value::Bool(request.keep_empty_rows),
        );
        payload.insert(
            "returnPropertyQuota".to_string(),
            Value::Bool(request.return_property_quota),
        );
        if let Some(filter) = request.dimension_filter {
            payload.insert("dimensionFilter".to_string(), snake_to_camel_json(filter));
        }
        if let Some(filter) = request.metric_filter {
            payload.insert("metricFilter".to_string(), snake_to_camel_json(filter));
        }
        if let Some(order_bys) = request.order_bys {
            payload.insert(
                "orderBys".to_string(),
                Value::Array(order_bys.into_iter().map(snake_to_camel_json).collect()),
            );
        }
        if let Some(currency_code) = request.currency_code {
            payload.insert(
                "currencyCode".to_string(),
                Value::String(currency_code.trim().to_string()),
            );
        }

        self.post_json(
            &format!("{}/v1beta/{property}:runPivotReport", self.data_base_url),
            Value::Object(payload),
        )
        .await
    }

    pub async fn batch_run_reports(
        &self,
        request: BatchRunReportsRequest,
    ) -> Result<Value, AnalyticsError> {
        let property = request.property_id.to_resource_name()?;
        let requests = request
            .requests
            .into_iter()
            .map(|item| {
                Value::Object(build_run_report_payload(
                    item.date_ranges,
                    item.dimensions,
                    item.metrics,
                    item.dimension_filter,
                    item.metric_filter,
                    item.order_bys,
                    item.limit,
                    item.offset,
                    item.currency_code,
                    item.return_property_quota,
                ))
            })
            .collect::<Vec<_>>();

        self.post_json(
            &format!("{}/v1beta/{property}:batchRunReports", self.data_base_url),
            json!({ "requests": requests }),
        )
        .await
    }

    pub async fn run_property_access_report(
        &self,
        property_id: &PropertyId,
        request: RunAccessReportRequest,
    ) -> Result<Value, AnalyticsError> {
        let property = property_id.to_resource_name()?;
        let payload = build_access_report_payload(request);
        self.post_json(
            &format!("{}/v1beta/{property}:runAccessReport", self.admin_base_url),
            Value::Object(payload),
        )
        .await
    }

    pub async fn run_account_access_report(
        &self,
        account_id: &AccountId,
        request: RunAccessReportRequest,
    ) -> Result<Value, AnalyticsError> {
        let account = account_id.to_resource_name()?;
        let payload = build_access_report_payload(request);
        self.post_json(
            &format!("{}/v1beta/{account}:runAccessReport", self.admin_base_url),
            Value::Object(payload),
        )
        .await
    }

    pub async fn get_custom_dimensions_and_metrics(
        &self,
        property_id: &PropertyId,
    ) -> Result<Value, AnalyticsError> {
        let property = property_id.to_resource_name()?;
        let metadata = self
            .get_json(
                &format!("{}/v1beta/{property}/metadata", self.data_base_url),
                &[],
            )
            .await?;

        let custom_dimensions = metadata
            .get("dimensions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|dimension| {
                dimension
                    .get("customDefinition")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();

        let custom_metrics = metadata
            .get("metrics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|metric| {
                metric
                    .get("customDefinition")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();

        Ok(json!({
            "custom_dimensions": custom_dimensions,
            "custom_metrics": custom_metrics,
        }))
    }

    async fn collect_paginated(
        &self,
        url: &str,
        array_key: &str,
        pagination: PaginationOptions,
    ) -> Result<(Vec<Value>, Value), AnalyticsError> {
        let page_size = self.normalized_page_size(pagination.page_size);
        let max_pages = self.normalized_max_pages(pagination.max_pages);

        let mut pages_fetched = 0_u32;
        let mut next_page_token: Option<String> = None;
        let mut items: Vec<Value> = Vec::new();

        loop {
            let mut query = vec![("pageSize", page_size.to_string())];
            if let Some(token) = &next_page_token {
                query.push(("pageToken", token.clone()));
            }
            let payload = self.get_json(url, &query).await?;
            if let Some(rows) = payload.get(array_key).and_then(Value::as_array) {
                items.extend(rows.iter().cloned());
            }

            pages_fetched += 1;
            next_page_token = payload
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|token| !token.trim().is_empty());

            if next_page_token.is_none() || pages_fetched >= max_pages {
                break;
            }
        }

        Ok((
            items,
            json!({
                "page_size": page_size,
                "pages_fetched": pages_fetched,
                "max_pages": max_pages,
                "next_page_token": next_page_token,
            }),
        ))
    }

    async fn get_json(&self, url: &str, query: &[(&str, String)]) -> Result<Value, AnalyticsError> {
        let token = self.access_token().await?;

        let url = parse_https_upstream_url(url)?;
        let mut request = self.http.request(Method::GET, url).bearer_auth(token);
        if let Some(quota_project) = &self.quota_project {
            request = request.header("x-goog-user-project", quota_project.as_ref());
        }
        if !query.is_empty() {
            request = request.query(query);
        }

        self.send_json(request).await
    }

    async fn post_json(&self, url: &str, body: Value) -> Result<Value, AnalyticsError> {
        let token = self.access_token().await?;

        let url = parse_https_upstream_url(url)?;
        let mut request = self
            .http
            .request(Method::POST, url)
            .bearer_auth(token)
            .json(&body);
        if let Some(quota_project) = &self.quota_project {
            request = request.header("x-goog-user-project", quota_project.as_ref());
        }

        self.send_json(request).await
    }

    async fn send_json(&self, request: RequestBuilder) -> Result<Value, AnalyticsError> {
        let response = request.send().await.map_err(AnalyticsError::Transport)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(AnalyticsError::Transport)?;

        if !status.is_success() {
            let message = String::from_utf8_lossy(&bytes).trim().to_string();
            return Err(AnalyticsError::UpstreamApi {
                status: status.as_u16(),
                message: if message.is_empty() {
                    "no upstream response body".to_string()
                } else {
                    clip_message(message)
                },
            });
        }

        if bytes.is_empty() {
            return Ok(Value::Null);
        }

        serde_json::from_slice(&bytes).map_err(AnalyticsError::UpstreamJson)
    }

    fn normalized_page_size(&self, requested: Option<u32>) -> u32 {
        requested
            .unwrap_or(self.max_page_size)
            .clamp(1, self.max_page_size.max(1))
    }

    fn normalized_max_pages(&self, requested: Option<u32>) -> u32 {
        requested
            .unwrap_or(self.default_max_pages)
            .clamp(1, self.default_max_pages.max(1))
    }

    async fn token_provider(&self) -> Result<Arc<dyn TokenProvider>, AnalyticsError> {
        let provider = self
            .token_provider
            .get_or_try_init(|| async {
                match &self.auth_mode {
                    UpstreamAuthMode::RequestHeaderOnly => Err(AnalyticsError::AuthBootstrap(
                        "request_header mode does not use a configured Google credential source"
                            .to_string(),
                    )),
                    UpstreamAuthMode::Adc => gcp_auth::provider()
                        .await
                        .map_err(|err| AnalyticsError::AuthBootstrap(err.to_string())),
                    UpstreamAuthMode::AuthorizedUserAdcFile(_) => Err(
                        AnalyticsError::AuthBootstrap(
                            "server-specific authorized-user ADC is handled by the toolkit refresh-token provider".to_string(),
                        ),
                    ),
                    UpstreamAuthMode::OAuthRefresh(_) => Err(AnalyticsError::AuthBootstrap(
                        "OAuth refresh-token auth is handled by the refresh-token path".to_string(),
                    )),
                }
            })
            .await?;
        Ok(provider.clone())
    }

    async fn authorized_user_adc_provider(
        &self,
    ) -> Result<Option<Arc<RefreshTokenProvider>>, AnalyticsError> {
        if let Some(provider) = self.oauth_token_provider.get() {
            return Ok(Some(provider.clone()));
        }

        let UpstreamAuthMode::AuthorizedUserAdcFile(path) = &self.auth_mode else {
            return Err(AnalyticsError::AuthBootstrap(
                "authorized-user ADC provider requested for a non-ADC auth mode".to_string(),
            ));
        };

        let scopes = vec![self.analytics_scope.as_ref().to_string()];
        let adc = match google_authorized_user_adc_from_file(path, scopes) {
            Ok(adc) => adc,
            Err(err) if google_adc_file_missing(&err) => return Ok(None),
            Err(err) => {
                return Err(AnalyticsError::AuthBootstrap(format!(
                    "failed to load authorized-user ADC at '{}': {err}",
                    path.display()
                )));
            }
        };
        let provider = Arc::new(
            RefreshTokenProvider::new(adc.into_refresh_config()).map_err(|err| {
                AnalyticsError::AuthBootstrap(format!("invalid authorized-user ADC: {err}"))
            })?,
        );
        let provider = self
            .oauth_token_provider
            .get_or_init(|| async { provider })
            .await;
        Ok(Some(provider.clone()))
    }

    async fn access_token(&self) -> Result<String, AnalyticsError> {
        let request_token_raw = REQUEST_ACCESS_TOKEN_OVERRIDE
            .try_with(Clone::clone)
            .ok()
            .flatten();
        let request_token = parse_request_token_header_value(
            request_token_raw.as_deref(),
            self.token_header.as_ref(),
        )?;

        match self.token_source {
            UpstreamTokenSource::Config => self.configured_access_token().await,
            UpstreamTokenSource::RequestHeader => request_token.ok_or_else(|| {
                AnalyticsError::missing_request_access_token(self.token_header.to_string())
            }),
            UpstreamTokenSource::RequestHeaderOrConfig => {
                if let Some(token) = request_token {
                    Ok(token)
                } else {
                    self.configured_access_token().await
                }
            }
        }
    }

    async fn configured_access_token(&self) -> Result<String, AnalyticsError> {
        match &self.auth_mode {
            UpstreamAuthMode::RequestHeaderOnly => Err(AnalyticsError::AuthBootstrap(
                "request_header mode requires the caller to supply a Google access token on each request"
                    .to_string(),
            )),
            UpstreamAuthMode::Adc => {
                let provider = self.token_provider().await?;
                let token = provider.token(&[self.analytics_scope.as_ref()]).await?;
                Ok(token.as_str().to_string())
            }
            UpstreamAuthMode::AuthorizedUserAdcFile(path) => {
                let provider = self.authorized_user_adc_provider().await?.ok_or_else(|| {
                    AnalyticsError::AuthBootstrap(format!(
                        "server-specific GA4 ADC file was not found at '{}'; run `ga4-mcp auth login` or set GOOGLE_ANALYTICS_MCP_SHARED_ADC=true to intentionally use conventional shared ADC",
                        path.display()
                    ))
                })?;
                let token = provider
                    .access_token()
                    .await
                    .map_err(|err| AnalyticsError::AuthBootstrap(err.to_string()))?;
                Ok(token.expose_secret().to_string())
            }
            UpstreamAuthMode::OAuthRefresh(config) => {
                if let Some(cached) = self.cached_oauth_token.read().await.as_ref() {
                    if Instant::now() < cached.refresh_after {
                        return Ok(cached.value.clone());
                    }
                }

                let mut writer = self.cached_oauth_token.write().await;
                if let Some(cached) = writer.as_ref() {
                    if Instant::now() < cached.refresh_after {
                        return Ok(cached.value.clone());
                    }
                }

                let token = self.refresh_oauth_access_token(config.as_ref()).await?;
                *writer = Some(token.clone());
                Ok(token.value)
            }
        }
    }

    async fn refresh_oauth_access_token(
        &self,
        config: &OAuthRefreshConfig,
    ) -> Result<CachedAccessToken, AnalyticsError> {
        let response = self
            .http
            .request(Method::POST, &config.token_uri)
            .form(&[
                ("client_id", config.client_id.as_str()),
                ("client_secret", config.client_secret.as_str()),
                ("refresh_token", config.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(AnalyticsError::Transport)?;

        let status = response.status();
        let bytes = response.bytes().await.map_err(AnalyticsError::Transport)?;
        let parsed: OAuthRefreshResponse = serde_json::from_slice(&bytes).map_err(|err| {
            AnalyticsError::AuthBootstrap(format!("failed to parse OAuth token response: {err}"))
        })?;

        if !status.is_success() {
            let error = parsed
                .error
                .as_deref()
                .unwrap_or("unknown_error")
                .to_string();
            let detail = parsed
                .error_description
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    clip_message(String::from_utf8_lossy(&bytes).trim().to_string())
                });
            return Err(AnalyticsError::AuthBootstrap(format!(
                "oauth refresh exchange failed with status {} ({error}): {detail}",
                status.as_u16()
            )));
        }

        let Some(access_token) = parsed.access_token else {
            return Err(AnalyticsError::AuthBootstrap(
                "oauth refresh exchange succeeded without access_token".to_string(),
            ));
        };
        let expires_in = parsed.expires_in.unwrap_or(3600);
        let refresh_in = expires_in.saturating_sub(60).max(1);
        Ok(CachedAccessToken {
            value: access_token,
            refresh_after: Instant::now() + Duration::from_secs(refresh_in),
        })
    }
}

fn parse_https_upstream_url(raw: &str) -> Result<Url, AnalyticsError> {
    let url = Url::parse(raw).map_err(|err| AnalyticsError::InvalidArgument {
        field: "upstream_url",
        message: format!("invalid upstream URL: {err}"),
    })?;
    if url.scheme() != "https" {
        return Err(AnalyticsError::InvalidArgument {
            field: "upstream_url",
            message: "upstream Google API URL must use https".to_string(),
        });
    }
    Ok(url)
}

fn select_auth_mode(settings: &Settings) -> Result<UpstreamAuthMode, AnalyticsError> {
    if settings.upstream_token_source == UpstreamTokenSource::RequestHeader {
        return Ok(UpstreamAuthMode::RequestHeaderOnly);
    }

    match (
        settings.oauth_client_secret_json.as_deref(),
        settings.oauth_refresh_token.as_deref(),
    ) {
        (Some(client_secret_path), Some(refresh_token)) => {
            return Ok(UpstreamAuthMode::OAuthRefresh(Arc::new(
                parse_oauth_refresh_config(client_secret_path, refresh_token)?,
            )));
        }
        (None, None) => {}
        _ => {
            return Err(AnalyticsError::AuthBootstrap(
                "GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON and GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN must both be set or both be unset; refusing to fall back to ADC with partial OAuth configuration".to_string(),
            ));
        }
    }

    if env::var_os("GOOGLE_APPLICATION_CREDENTIALS").is_some() || settings.shared_adc {
        return Ok(UpstreamAuthMode::Adc);
    }

    if let Some(path) = server_adc_credentials_path() {
        return Ok(UpstreamAuthMode::AuthorizedUserAdcFile(path));
    }

    Err(AnalyticsError::AuthBootstrap(
        "failed to determine the GA4-specific ADC path; set HOME/XDG_CONFIG_HOME on Unix or APPDATA on Windows, or set GOOGLE_ANALYTICS_MCP_SHARED_ADC=true to intentionally use conventional shared ADC".to_string(),
    ))
}

fn google_adc_file_missing(err: &UpstreamOAuthError) -> bool {
    matches!(
        err,
        UpstreamOAuthError::Io { source, .. } if source.kind() == ErrorKind::NotFound
    )
}

fn parse_oauth_refresh_config(
    client_secret_json_path: &str,
    refresh_token: &str,
) -> Result<OAuthRefreshConfig, AnalyticsError> {
    let raw = fs::read_to_string(client_secret_json_path).map_err(|err| {
        AnalyticsError::AuthBootstrap(format!(
            "failed to read OAuth client secret JSON at '{client_secret_json_path}': {err}"
        ))
    })?;
    let parsed: OAuthClientSecretFile = serde_json::from_str(&raw).map_err(|err| {
        AnalyticsError::AuthBootstrap(format!(
            "invalid OAuth client secret JSON at '{client_secret_json_path}': {err}"
        ))
    })?;
    let client = parsed.installed.or(parsed.web).ok_or_else(|| {
        AnalyticsError::AuthBootstrap(
            "OAuth client secret JSON must contain either 'installed' or 'web' object".to_string(),
        )
    })?;

    let client_id = client.client_id.trim();
    if client_id.is_empty() {
        return Err(AnalyticsError::AuthBootstrap(
            "OAuth client secret JSON is missing client_id".to_string(),
        ));
    }
    let client_secret = client.client_secret.trim();
    if client_secret.is_empty() {
        return Err(AnalyticsError::AuthBootstrap(
            "OAuth client secret JSON is missing client_secret".to_string(),
        ));
    }
    let token_uri = client
        .token_uri
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("https://oauth2.googleapis.com/token");
    validate_oauth_token_uri(token_uri)?;

    Ok(OAuthRefreshConfig {
        token_uri: token_uri.to_string(),
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        refresh_token: refresh_token.to_string(),
    })
}

fn clip_message(message: String) -> String {
    const MAX_LEN: usize = 1_024;
    if message.len() <= MAX_LEN {
        return message;
    }
    let mut end = MAX_LEN;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &message[..end])
}

fn validate_oauth_token_uri(token_uri: &str) -> Result<(), AnalyticsError> {
    let parsed = Url::parse(token_uri).map_err(|err| {
        AnalyticsError::AuthBootstrap(format!(
            "invalid OAuth token_uri '{token_uri}' in GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON: {err}"
        ))
    })?;
    if parsed.scheme() != "https" {
        return Err(AnalyticsError::AuthBootstrap(format!(
            "OAuth token_uri '{token_uri}' must use https"
        )));
    }

    let host = parsed.host_str().unwrap_or("");
    let path = parsed.path().trim_end_matches('/');
    let allowed = (host == "oauth2.googleapis.com" && path == "/token")
        || (host == "accounts.google.com" && path == "/o/oauth2/token");
    if !allowed {
        return Err(AnalyticsError::AuthBootstrap(format!(
            "OAuth token_uri '{token_uri}' must be one of https://oauth2.googleapis.com/token or https://accounts.google.com/o/oauth2/token"
        )));
    }
    Ok(())
}

fn parse_request_token_header_value(
    raw_value: Option<&str>,
    header_name: &str,
) -> Result<Option<String>, AnalyticsError> {
    let Some(value) = raw_value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    normalize_request_token_value(value, header_name)
        .map(Some)
        .map_err(|message| AnalyticsError::malformed_request_access_token(header_name, message))
}

fn normalize_request_token_value(raw_value: &str, header_name: &str) -> Result<String, String> {
    if let Some(stripped) = strip_bearer_scheme(raw_value) {
        let token = stripped.trim();
        if token.is_empty() {
            return Err("Bearer token is empty".to_string());
        }
        return Ok(token.to_string());
    }

    if raw_value.chars().any(char::is_whitespace) {
        if header_name.eq_ignore_ascii_case("authorization") {
            return Err(
                "authorization header must use 'Bearer <token>' or a compact token value"
                    .to_string(),
            );
        }
        return Err("token header must be a compact token or use Bearer scheme".to_string());
    }

    Ok(raw_value.to_string())
}

fn strip_bearer_scheme(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        Some(rest)
    } else {
        None
    }
}

fn as_named_entries(values: &[String]) -> Vec<Value> {
    values
        .iter()
        .map(|name| json!({"name": name.trim()}))
        .collect()
}

fn as_access_named_entries(values: &[String], field_name: &'static str) -> Vec<Value> {
    values
        .iter()
        .map(|name| {
            let mut entry = Map::new();
            entry.insert(
                field_name.to_string(),
                Value::String(name.trim().to_string()),
            );
            Value::Object(entry)
        })
        .collect()
}

fn build_run_report_payload(
    date_ranges: Vec<Value>,
    dimensions: Vec<String>,
    metrics: Vec<String>,
    dimension_filter: Option<Value>,
    metric_filter: Option<Value>,
    order_bys: Option<Vec<Value>>,
    limit: Option<u64>,
    offset: Option<u64>,
    currency_code: Option<String>,
    return_property_quota: bool,
) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert(
        "dimensions".to_string(),
        Value::Array(as_named_entries(&dimensions)),
    );
    payload.insert(
        "metrics".to_string(),
        Value::Array(as_named_entries(&metrics)),
    );
    payload.insert(
        "dateRanges".to_string(),
        Value::Array(
            date_ranges
                .into_iter()
                .map(snake_to_camel_json)
                .collect::<Vec<_>>(),
        ),
    );
    payload.insert(
        "returnPropertyQuota".to_string(),
        Value::Bool(return_property_quota),
    );
    if let Some(filter) = dimension_filter {
        payload.insert("dimensionFilter".to_string(), snake_to_camel_json(filter));
    }
    if let Some(filter) = metric_filter {
        payload.insert("metricFilter".to_string(), snake_to_camel_json(filter));
    }
    if let Some(order_bys) = order_bys {
        payload.insert(
            "orderBys".to_string(),
            Value::Array(order_bys.into_iter().map(snake_to_camel_json).collect()),
        );
    }
    if let Some(limit) = limit {
        payload.insert("limit".to_string(), Value::String(limit.to_string()));
    }
    if let Some(offset) = offset {
        payload.insert("offset".to_string(), Value::String(offset.to_string()));
    }
    if let Some(currency_code) = currency_code {
        payload.insert(
            "currencyCode".to_string(),
            Value::String(currency_code.trim().to_string()),
        );
    }
    payload
}

fn build_access_report_payload(request: RunAccessReportRequest) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert(
        "dateRanges".to_string(),
        Value::Array(
            request
                .date_ranges
                .into_iter()
                .map(snake_to_camel_json)
                .collect::<Vec<_>>(),
        ),
    );
    payload.insert(
        "dimensions".to_string(),
        Value::Array(as_access_named_entries(
            &request.dimensions,
            "dimensionName",
        )),
    );
    payload.insert(
        "metrics".to_string(),
        Value::Array(as_access_named_entries(&request.metrics, "metricName")),
    );
    if let Some(filter) = request.dimension_filter {
        payload.insert("dimensionFilter".to_string(), snake_to_camel_json(filter));
    }
    if let Some(filter) = request.metric_filter {
        payload.insert("metricFilter".to_string(), snake_to_camel_json(filter));
    }
    if let Some(order_bys) = request.order_bys {
        payload.insert(
            "orderBys".to_string(),
            Value::Array(order_bys.into_iter().map(snake_to_camel_json).collect()),
        );
    }
    if let Some(offset) = request.offset {
        payload.insert("offset".to_string(), Value::String(offset.to_string()));
    }
    if let Some(limit) = request.limit {
        payload.insert("limit".to_string(), Value::String(limit.to_string()));
    }
    if let Some(time_zone) = request.time_zone {
        payload.insert(
            "timeZone".to_string(),
            Value::String(time_zone.trim().to_string()),
        );
    }
    payload
}

trait NumericResourceId {
    fn numeric_value(&self) -> Option<u64>;
    fn text_value(&self) -> Option<&str>;
}

impl NumericResourceId for PropertyId {
    fn numeric_value(&self) -> Option<u64> {
        match self {
            Self::Number(value) => Some(*value),
            Self::Text(_) => None,
        }
    }

    fn text_value(&self) -> Option<&str> {
        match self {
            Self::Number(_) => None,
            Self::Text(value) => Some(value),
        }
    }
}

impl NumericResourceId for AccountId {
    fn numeric_value(&self) -> Option<u64> {
        match self {
            Self::Number(value) => Some(*value),
            Self::Text(_) => None,
        }
    }

    fn text_value(&self) -> Option<&str> {
        match self {
            Self::Number(_) => None,
            Self::Text(value) => Some(value),
        }
    }
}

fn parse_resource_name<T: NumericResourceId>(
    value: &T,
    field_name: &'static str,
    resource_prefix: &'static str,
    expected_message: &'static str,
) -> Result<String, AnalyticsError> {
    if let Some(number) = value.numeric_value() {
        return Ok(format!("{resource_prefix}/{number}"));
    }
    let Some(raw) = value.text_value() else {
        return Err(AnalyticsError::invalid(field_name, expected_message));
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AnalyticsError::invalid(field_name, "must not be empty"));
    }
    if let Ok(number) = trimmed.parse::<u64>() {
        return Ok(format!("{resource_prefix}/{number}"));
    }
    if let Some(rest) = trimmed.strip_prefix(&format!("{resource_prefix}/")) {
        if let Ok(number) = rest.parse::<u64>() {
            return Ok(format!("{resource_prefix}/{number}"));
        }
    }
    Err(AnalyticsError::invalid(field_name, expected_message))
}

pub(crate) fn snake_to_camel_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let converted = map
                .into_iter()
                .map(|(key, value)| (snake_key_to_camel(&key), snake_to_camel_json(value)))
                .collect::<Map<String, Value>>();
            Value::Object(converted)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(snake_to_camel_json).collect()),
        other => other,
    }
}

fn snake_key_to_camel(key: &str) -> String {
    if !key.contains('_') {
        return key.to_string();
    }
    let mut out = String::with_capacity(key.len());
    let mut uppercase_next = false;
    for ch in key.chars() {
        if ch == '_' {
            uppercase_next = true;
            continue;
        }
        if uppercase_next {
            out.push(ch.to_ascii_uppercase());
            uppercase_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

pub(crate) fn sort_object(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut ordered = BTreeMap::new();
            for (key, value) in map {
                ordered.insert(key, sort_object(value));
            }
            Value::Object(ordered.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.into_iter().map(sort_object).collect()),
        other => other,
    }
}

fn adc_quota_project_id_for_auth_mode(
    auth_mode: &UpstreamAuthMode,
    shared_adc: bool,
) -> Option<String> {
    let path = match auth_mode {
        UpstreamAuthMode::RequestHeaderOnly => None,
        UpstreamAuthMode::AuthorizedUserAdcFile(path) => Some(path.clone()),
        UpstreamAuthMode::Adc if shared_adc => conventional_adc_credentials_path(),
        UpstreamAuthMode::Adc | UpstreamAuthMode::OAuthRefresh(_) => None,
    }?;
    let raw = read_adc_file(&path)?;
    parse_adc_quota_project_id(&raw)
}

fn parse_adc_quota_project_id(raw: &str) -> Option<String> {
    serde_json::from_str::<Value>(raw)
        .ok()?
        .get("quota_project_id")?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn read_adc_file(path: &std::path::Path) -> Option<String> {
    let root = server_adc_credentials_path()
        .filter(|candidate| candidate == path)
        .and_then(|_| server_cloudsdk_config_dir())
        .or_else(|| {
            conventional_adc_credentials_path()
                .filter(|candidate| candidate == path)
                .and_then(|_| conventional_cloudsdk_config_dir())
        })?;
    let root = root.canonicalize().ok()?;
    let path = path.canonicalize().ok()?;
    if !path.starts_with(root) || !path.is_file() {
        return None;
    }
    fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::config::{CapabilityProfile, CliCommand};

    #[test]
    fn property_id_accepts_numeric_and_prefixed_values() {
        let plain = PropertyId::Text("1234".to_string())
            .to_resource_name()
            .expect("plain digits should parse");
        assert_eq!(plain, "properties/1234");

        let prefixed = PropertyId::Text("properties/5678".to_string())
            .to_resource_name()
            .expect("prefixed value should parse");
        assert_eq!(prefixed, "properties/5678");

        let numeric = PropertyId::Number(9012)
            .to_resource_name()
            .expect("numeric value should parse");
        assert_eq!(numeric, "properties/9012");
    }

    #[test]
    fn property_id_rejects_invalid_values() {
        let err = PropertyId::Text("props/1".to_string())
            .to_resource_name()
            .expect_err("invalid prefix should fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
    }

    #[test]
    fn account_id_accepts_numeric_and_prefixed_values() {
        let plain = AccountId::Text("1234".to_string())
            .to_resource_name()
            .expect("plain digits should parse");
        assert_eq!(plain, "accounts/1234");

        let prefixed = AccountId::Text("accounts/5678".to_string())
            .to_resource_name()
            .expect("prefixed value should parse");
        assert_eq!(prefixed, "accounts/5678");

        let numeric = AccountId::Number(9012)
            .to_resource_name()
            .expect("numeric value should parse");
        assert_eq!(numeric, "accounts/9012");
    }

    #[test]
    fn snake_to_camel_json_converts_nested_keys() {
        let raw = json!({
            "date_ranges": [
                { "start_date": "2025-01-01", "end_date": "2025-01-31" }
            ],
            "dimension_filter": {
                "and_group": {
                    "expressions": [
                        { "field_name": "eventName" }
                    ]
                }
            }
        });

        let converted = snake_to_camel_json(raw);
        assert!(converted.get("dateRanges").is_some());
        assert!(
            converted
                .get("dimensionFilter")
                .and_then(|v| v.get("andGroup"))
                .is_some()
        );
    }

    #[test]
    fn parse_oauth_refresh_config_accepts_installed_client_secret_json() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let path = test_fixture_path(&format!("ga4-oauth-client-{nonce}.json"));
        let json = r#"{
            "installed": {
                "client_id": "client-id.apps.googleusercontent.com",
                "client_secret": "super-secret",
                "token_uri": "https://oauth2.googleapis.com/token"
            }
        }"#;
        std::fs::write(&path, json).expect("fixture should write");
        let config = parse_oauth_refresh_config(
            path.to_str().expect("temp path should be utf8"),
            "refresh-token-value",
        )
        .expect("config should parse");
        assert_eq!(config.client_id, "client-id.apps.googleusercontent.com");
        assert_eq!(config.client_secret, "super-secret");
        assert_eq!(config.refresh_token, "refresh-token-value");
        assert_eq!(config.token_uri, "https://oauth2.googleapis.com/token");
    }

    #[test]
    fn parse_oauth_refresh_config_rejects_missing_installed_or_web_object() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let path = test_fixture_path(&format!("ga4-oauth-client-invalid-{nonce}.json"));
        std::fs::write(&path, r#"{"client_id":"abc"}"#).expect("fixture should write");
        let err = parse_oauth_refresh_config(
            path.to_str().expect("temp path should be utf8"),
            "refresh-token-value",
        )
        .expect_err("missing installed/web should fail");
        assert!(err.to_string().contains("installed"));
    }

    #[test]
    fn parse_oauth_refresh_config_rejects_non_google_token_uri() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let path = test_fixture_path(&format!("ga4-oauth-client-bad-uri-{nonce}.json"));
        std::fs::write(
            &path,
            r#"{"installed":{"client_id":"client","client_secret":"secret","token_uri":"https://evil.test/token"}}"#,
        )
        .expect("fixture should write");
        let err = parse_oauth_refresh_config(
            path.to_str().expect("temp path should be utf8"),
            "refresh-token-value",
        )
        .expect_err("non-google token uri should fail");
        assert!(err.to_string().contains("must be one of"));
    }

    #[test]
    fn select_auth_mode_request_header_does_not_require_local_adc_bootstrap() {
        let mut settings = test_settings();
        settings.upstream_token_source = UpstreamTokenSource::RequestHeader;

        let auth_mode = select_auth_mode(&settings).expect("request_header mode should bootstrap");

        assert!(matches!(auth_mode, UpstreamAuthMode::RequestHeaderOnly));
    }

    #[test]
    fn select_auth_mode_request_header_ignores_partial_oauth_refresh_config() {
        let mut settings = test_settings();
        settings.upstream_token_source = UpstreamTokenSource::RequestHeader;
        settings.oauth_client_secret_json = Some("client-secret.json".to_string());

        let auth_mode = select_auth_mode(&settings)
            .expect("request_header mode should ignore server-side OAuth refresh bootstrap");

        assert!(matches!(auth_mode, UpstreamAuthMode::RequestHeaderOnly));
    }

    fn test_fixture_path(name: &str) -> PathBuf {
        let root = PathBuf::from("target").join("ga4-mcp-test-fixtures");
        std::fs::create_dir_all(&root).expect("fixture root should be writable");
        root.join(name)
    }

    fn test_settings() -> Settings {
        Settings {
            analytics_scope: DEFAULT_ANALYTICS_SCOPE.to_string(),
            admin_base_url: "https://analyticsadmin.googleapis.com".to_string(),
            data_base_url: "https://analyticsdata.googleapis.com".to_string(),
            http_timeout: Duration::from_secs(1),
            max_page_size: 200,
            max_pages: 20,
            user_agent: "test".to_string(),
            oauth_client_secret_json: None,
            oauth_refresh_token: None,
            upstream_token_source: UpstreamTokenSource::Config,
            upstream_token_header: "x-google-access-token".to_string(),
            quota_project: None,
            shared_adc: false,
            scratchpad_session_ttl: Duration::from_secs(900),
            scratchpad_max_sessions: 64,
            scratchpad_max_tables_per_session: 32,
            scratchpad_max_rows_per_session: 1_000_000,
            scratchpad_max_memory_mb: 256,
            scratchpad_query_timeout: Duration::from_secs(15),
            scratchpad_max_sql_bytes: 65_536,
            capability_profile: CapabilityProfile::ReadOnly,
            print_tools: false,
            print_tool_schema: false,
            command: Some(CliCommand::Serve),
        }
    }

    #[test]
    fn parses_adc_quota_project_id_without_exposing_credentials() {
        assert_eq!(
            parse_adc_quota_project_id(
                r#"{"client_id":"client","client_secret":"secret","refresh_token":"refresh","quota_project_id":" ga4-quota "}"#
            )
            .as_deref(),
            Some("ga4-quota")
        );
        assert_eq!(
            parse_adc_quota_project_id(r#"{"quota_project_id":"   "}"#),
            None
        );
        assert_eq!(parse_adc_quota_project_id("not-json"), None);
    }

    #[test]
    fn parse_https_upstream_url_rejects_cleartext_http() {
        let err = parse_https_upstream_url("http://analyticsdata.googleapis.com/v1beta")
            .expect_err("cleartext upstream request URL should fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(err.to_string().contains("https"));
    }

    #[test]
    fn normalize_request_token_value_accepts_bearer_scheme() {
        let token = normalize_request_token_value("Bearer ya29.a0AfH6SMBEXAMPLE", "authorization")
            .expect("bearer token should parse");
        assert_eq!(token, "ya29.a0AfH6SMBEXAMPLE");
    }

    #[test]
    fn normalize_request_token_value_rejects_non_bearer_authorization_scheme() {
        let err = normalize_request_token_value("Basic Zm9vOmJhcg==", "authorization")
            .expect_err("non-bearer authorization should fail");
        assert!(err.contains("authorization header must use"));
    }

    #[test]
    fn normalize_request_token_value_accepts_compact_custom_header_value() {
        let token = normalize_request_token_value("ya29.compact.token", "x-google-access-token")
            .expect("compact token should parse");
        assert_eq!(token, "ya29.compact.token");
    }

    #[test]
    fn clips_multibyte_messages_without_panicking() {
        let message = format!("{}a", "é".repeat(512));
        let clipped = clip_message(message);
        assert!(clipped.ends_with("..."));
        assert!(clipped.len() <= 1_027);
        assert!(clipped.chars().all(|ch| ch == 'é' || ch == '.'));
    }
}
