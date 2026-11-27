//! Fixed, cost-bounded aggregate queries over a verified daily GA4 export.
//! No arbitrary SQL, project selection, raw user output or ambient billing authority.
use std::{collections::HashSet, time::Duration};

use reqwest::{Client, RequestBuilder, Url};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use time::{Date, Month};

use crate::{error::AnalyticsError, ga_client::PropertyId};

const BQ: &str = "https://bigquery.googleapis.com/bigquery/v2";
const ADMIN: &str = "https://analyticsadmin.googleapis.com/v1alpha";
const MAX_ROWS: u32 = 500;
const MAX_RESPONSE: usize = 1024 * 1024;
const MAX_PAGES: usize = 5;
const MAX_LINK_PAGES: usize = 5;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExportQueryArgs {
    /// The Google Analytics property id as integer or "properties/<id>".
    pub property_id: PropertyId,
    /// daily_events, daily_sessions, event_transitions, first_user_acquisition, or item_categories.
    pub report_kind: String,
    /// First daily export date, YYYY-MM-DD.
    pub start_date: String,
    /// Last daily export date, YYYY-MM-DD (inclusive; at most 31 days).
    pub end_date: String,
    /// Caller-selected positive upper bound on bytes billed (also capped by the operator).
    pub maximum_bytes_billed: u64,
    /// Execute after a successful dry run only when explicitly enabled by the operator.
    #[serde(default)]
    pub execute: bool,
    /// Result cap from 1 through 500 (default 100).
    #[serde(default)]
    pub max_rows: Option<u32>,
}

#[derive(Clone)]
pub(crate) struct BigQueryExportClient {
    http: Client,
    export_project: String,
    export_project_number: String,
    billing_project: String,
    location: String,
    maximum_bytes: u64,
    allow_execution: bool,
    #[cfg(test)]
    fixture: Option<std::sync::Arc<std::sync::Mutex<tests::ProviderFixture>>>,
}

pub(crate) struct ExportPlan {
    property: String,
    dataset: String,
    report_kind: String,
    query: String,
    columns: Vec<&'static str>,
    start_suffix: String,
    end_suffix: String,
    maximum_bytes: u64,
    max_rows: u32,
    execute: bool,
}

fn invalid(field: &'static str, message: &'static str) -> AnalyticsError {
    AnalyticsError::invalid(field, message)
}

fn bad_provider() -> AnalyticsError {
    AnalyticsError::Internal("unexpected or unbounded BigQuery export response".to_string())
}

// The transport is HTTPS-only and refuses redirects. Build paths by adding
// encoded segments to a fixed Google origin, never by interpolating a URL
// scheme or authority supplied by the operator or provider.
fn google_endpoint(base: &'static str, segments: &[&str]) -> Result<Url, AnalyticsError> {
    let mut url = Url::parse(base).map_err(|_| bad_provider())?;
    if url.scheme() != "https" {
        return Err(bad_provider());
    }
    url.path_segments_mut()
        .map_err(|_| bad_provider())?
        .extend(segments.iter().copied());
    Ok(url)
}

fn env(name: &'static str) -> Result<String, AnalyticsError> {
    std::env::var(name)
        .map(|value| value.trim().to_string())
        .map_err(|_| invalid(name, "operator configuration is required"))
}

fn valid_project(value: &str) -> bool {
    let bytes = value.as_bytes();
    (6..=30).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn valid_segment(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn parse_date(field: &'static str, raw: &str) -> Result<Date, AnalyticsError> {
    let b = raw.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || !b
            .iter()
            .enumerate()
            .all(|(i, v)| i == 4 || i == 7 || v.is_ascii_digit())
    {
        return Err(invalid(field, "expected YYYY-MM-DD"));
    }
    let year = raw[..4]
        .parse::<i32>()
        .map_err(|_| invalid(field, "invalid year"))?;
    let month = raw[5..7]
        .parse::<u8>()
        .ok()
        .and_then(|m| Month::try_from(m).ok())
        .ok_or_else(|| invalid(field, "invalid month"))?;
    let day = raw[8..]
        .parse::<u8>()
        .map_err(|_| invalid(field, "invalid day"))?;
    if year < 2000 {
        return Err(invalid(field, "date must be in or after 2000"));
    }
    Date::from_calendar_date(year, month, day).map_err(|_| invalid(field, "invalid calendar date"))
}

fn suffix(date: Date) -> String {
    format!(
        "{:04}{:02}{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

impl BigQueryExportClient {
    pub(crate) fn from_env(timeout: Duration) -> Result<Self, AnalyticsError> {
        let export_project = env("GA4_MCP_BIGQUERY_EXPORT_PROJECT")?;
        let export_project_number = env("GA4_MCP_BIGQUERY_EXPORT_PROJECT_NUMBER")?;
        let billing_project = env("GA4_MCP_BIGQUERY_BILLING_PROJECT")?;
        let location = env("GA4_MCP_BIGQUERY_LOCATION")?;
        let maximum_bytes = env("GA4_MCP_BIGQUERY_MAX_BYTES")?
            .parse::<u64>()
            .map_err(|_| invalid("GA4_MCP_BIGQUERY_MAX_BYTES", "expected a positive byte cap"))?;
        let allow_execution = match std::env::var("GA4_MCP_BIGQUERY_ALLOW_EXECUTION").as_deref() {
            Ok("true") => true,
            Ok("false") | Err(std::env::VarError::NotPresent) => false,
            _ => {
                return Err(invalid(
                    "GA4_MCP_BIGQUERY_ALLOW_EXECUTION",
                    "expected true or false",
                ));
            }
        };
        Self::new(
            timeout,
            export_project,
            export_project_number,
            billing_project,
            location,
            maximum_bytes,
            allow_execution,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        timeout: Duration,
        export_project: String,
        export_project_number: String,
        billing_project: String,
        location: String,
        maximum_bytes: u64,
        allow_execution: bool,
    ) -> Result<Self, AnalyticsError> {
        if !valid_project(&export_project)
            || !valid_project(&billing_project)
            || export_project_number
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .is_none()
            || !export_project_number.bytes().all(|b| b.is_ascii_digit())
            || !valid_segment(&location, 40)
            || maximum_bytes == 0
            || maximum_bytes > 1024_u64.pow(4)
        {
            return Err(invalid(
                "bigquery_configuration",
                "invalid project mapping, location or byte cap (maximum 1 TiB)",
            ));
        }
        let http = Client::builder()
            .timeout(timeout.min(Duration::from_secs(20)))
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(AnalyticsError::Transport)?;
        Ok(Self {
            http,
            export_project,
            export_project_number,
            billing_project,
            location,
            maximum_bytes,
            allow_execution,
            #[cfg(test)]
            fixture: None,
        })
    }

    pub(crate) fn plan(&self, args: ExportQueryArgs) -> Result<ExportPlan, AnalyticsError> {
        let property = args.property_id.to_resource_name()?;
        let property_number = property
            .strip_prefix("properties/")
            .ok_or_else(bad_provider)?;
        if property_number
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .is_none()
        {
            return Err(invalid(
                "property_id",
                "expected a positive property number",
            ));
        }
        let dataset = format!("analytics_{property_number}");
        let start = parse_date("start_date", &args.start_date)?;
        let end = parse_date("end_date", &args.end_date)?;
        let days = (end - start).whole_days();
        if !(0..31).contains(&days) {
            return Err(invalid(
                "end_date",
                "expected a nonnegative date range of at most 31 inclusive days",
            ));
        }
        if args.maximum_bytes_billed == 0 || args.maximum_bytes_billed > self.maximum_bytes {
            return Err(invalid(
                "maximum_bytes_billed",
                "positive byte cap must not exceed operator limit",
            ));
        }
        let max_rows = args.max_rows.unwrap_or(100);
        if !(1..=MAX_ROWS).contains(&max_rows) {
            return Err(invalid("max_rows", "expected 1 through 500"));
        }
        if args.execute && !self.allow_execution {
            return Err(AnalyticsError::policy_denied(
                "bigquery_execution_disabled",
                "query_bigquery_export",
            ));
        }
        // Only validated operator project and numeric GA property enter the identifier.
        // All date and row limits are named query parameters. Intraday tables cannot match.
        let table = format!("`{}.{dataset}.events_*`", self.export_project);
        let filter = "_TABLE_SUFFIX BETWEEN @start_suffix AND @end_suffix AND REGEXP_CONTAINS(_TABLE_SUFFIX, r'^[0-9]{8}$')";
        let session_id = "(SELECT MIN(ep.value.int_value) FROM UNNEST(event_params) ep WHERE ep.key = 'ga_session_id')";
        let event = "IF(REGEXP_CONTAINS(event_name, r'^[A-Za-z][A-Za-z0-9_]{0,39}$'), event_name, '(other)')";
        let (query, columns) = match args.report_kind.as_str() {
            "daily_events" => (
                format!(
                    "SELECT event_date, {event} AS event_label, COUNT(*) AS event_count, COUNT(DISTINCT user_pseudo_id) AS observed_users FROM {table} WHERE {filter} GROUP BY event_date,event_label HAVING COUNT(*) >= 5 ORDER BY event_date,event_count DESC,event_label LIMIT @row_limit"
                ),
                vec!["event_date", "event_label", "event_count", "observed_users"],
            ),
            "daily_sessions" => (
                format!(
                    "WITH events AS (SELECT event_date,user_pseudo_id,{session_id} AS sid FROM {table} WHERE {filter}) SELECT event_date,COUNT(DISTINCT STRUCT_KEY) AS observed_sessions FROM (SELECT event_date,TO_JSON_STRING(STRUCT(user_pseudo_id,sid)) AS STRUCT_KEY FROM events WHERE user_pseudo_id IS NOT NULL AND sid IS NOT NULL) GROUP BY event_date HAVING COUNT(DISTINCT STRUCT_KEY) >= 5 ORDER BY event_date LIMIT @row_limit"
                ),
                vec!["event_date", "observed_sessions"],
            ),
            "event_transitions" => (
                format!(
                    "WITH events AS (SELECT user_pseudo_id,{session_id} AS sid,event_timestamp,{event} AS event_label FROM {table} WHERE {filter}), sequenced AS (SELECT event_label,LEAD(event_label) OVER (PARTITION BY user_pseudo_id,sid ORDER BY event_timestamp,event_label) AS next_event FROM events WHERE user_pseudo_id IS NOT NULL AND sid IS NOT NULL) SELECT event_label,next_event,COUNT(*) AS transitions FROM sequenced WHERE next_event IS NOT NULL GROUP BY event_label,next_event HAVING COUNT(*) >= 5 ORDER BY transitions DESC,event_label,next_event LIMIT @row_limit"
                ),
                vec!["event_label", "next_event", "transitions"],
            ),
            "first_user_acquisition" => (
                format!(
                    "SELECT IF(REGEXP_CONTAINS(traffic_source.medium,r'^[A-Za-z][A-Za-z0-9_-]{{0,39}}$'),traffic_source.medium,'(other)') AS first_user_medium,COUNT(*) AS event_count,COUNT(DISTINCT user_pseudo_id) AS observed_users,COUNTIF(event_name='purchase') AS purchase_events FROM {table} WHERE {filter} GROUP BY first_user_medium HAVING COUNT(*) >= 5 ORDER BY event_count DESC,first_user_medium LIMIT @row_limit"
                ),
                vec![
                    "first_user_medium",
                    "event_count",
                    "observed_users",
                    "purchase_events",
                ],
            ),
            "item_categories" => (
                format!(
                    "SELECT IF(REGEXP_CONTAINS(i.item_category,r'^[A-Za-z][A-Za-z0-9 _-]{{0,63}}$'),i.item_category,'(other)') AS item_category,COUNT(*) AS item_records,COUNTIF(event_name='purchase') AS purchase_item_records,SUM(IF(event_name='purchase',COALESCE(i.quantity,0),0)) AS purchased_quantity FROM {table},UNNEST(items) i WHERE {filter} GROUP BY item_category HAVING COUNT(*) >= 5 ORDER BY item_records DESC,item_category LIMIT @row_limit"
                ),
                vec![
                    "item_category",
                    "item_records",
                    "purchase_item_records",
                    "purchased_quantity",
                ],
            ),
            _ => return Err(invalid("report_kind", "unknown predefined export report")),
        };
        Ok(ExportPlan {
            property,
            dataset,
            report_kind: args.report_kind,
            query,
            columns,
            start_suffix: suffix(start),
            end_suffix: suffix(end),
            maximum_bytes: args.maximum_bytes_billed,
            max_rows,
            execute: args.execute,
        })
    }

    fn request_body(&self, plan: &ExportPlan, dry_run: bool) -> Value {
        json!({
            "query": plan.query, "useLegacySql": false, "dryRun": dry_run,
            "location": self.location, "maximumBytesBilled": plan.maximum_bytes.to_string(),
            "jobTimeoutMs": "30000", "timeoutMs": 5000, "maxResults": plan.max_rows.min(100),
            "parameterMode": "NAMED",
            "queryParameters": [
                {"name":"start_suffix","parameterType":{"type":"STRING"},"parameterValue":{"value":plan.start_suffix}},
                {"name":"end_suffix","parameterType":{"type":"STRING"},"parameterValue":{"value":plan.end_suffix}},
                {"name":"row_limit","parameterType":{"type":"INT64"},"parameterValue":{"value":plan.max_rows.to_string()}}
            ]
        })
    }

    async fn response(&self, request: RequestBuilder) -> Result<Value, AnalyticsError> {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            return fixture.lock().unwrap().respond(request);
        }
        response(request).await
    }

    async fn verify_link(&self, plan: &ExportPlan, token: &str) -> Result<(), AnalyticsError> {
        let expected = format!("projects/{}", self.export_project_number);
        let property_id = plan
            .property
            .strip_prefix("properties/")
            .ok_or_else(bad_provider)?;
        let url = google_endpoint(ADMIN, &["properties", property_id, "bigQueryLinks"])?;
        let mut page_token: Option<String> = None;
        let mut seen = HashSet::new();
        for _ in 0..MAX_LINK_PAGES {
            let mut req = self
                .http
                .get(url.clone())
                .bearer_auth(token)
                .query(&[("pageSize", "100")]);
            if let Some(page) = &page_token {
                req = req.query(&[("pageToken", page)]);
            }
            let page = self.response(req).await?;
            if page
                .get("bigqueryLinks")
                .and_then(Value::as_array)
                .is_some_and(|links| {
                    links.iter().any(|link| {
                        link.get("project").and_then(Value::as_str) == Some(expected.as_str())
                            && link
                                .get("datasetLocation")
                                .and_then(Value::as_str)
                                .is_some_and(|loc| loc.eq_ignore_ascii_case(&self.location))
                            && (link.get("dailyExportEnabled").and_then(Value::as_bool)
                                == Some(true)
                                || link.get("freshDailyExportEnabled").and_then(Value::as_bool)
                                    == Some(true))
                    })
                })
            {
                return Ok(());
            }
            page_token = unique_page_token(&page, &mut seen)?;
            if page_token.is_none() {
                break;
            }
        }
        Err(AnalyticsError::policy_denied(
            "bigquery_export_link_unverified",
            "query_bigquery_export",
        ))
    }

    pub(crate) async fn query(
        &self,
        plan: ExportPlan,
        token: &str,
    ) -> Result<Value, AnalyticsError> {
        self.verify_link(&plan, token).await?;
        let dataset = self
            .response(
                self.http
                    .get(google_endpoint(
                        BQ,
                        &["projects", &self.export_project, "datasets", &plan.dataset],
                    )?)
                    .bearer_auth(token),
            )
            .await?;
        let reference = dataset.get("datasetReference").ok_or_else(bad_provider)?;
        if reference.get("projectId").and_then(Value::as_str) != Some(self.export_project.as_str())
            || reference.get("datasetId").and_then(Value::as_str) != Some(plan.dataset.as_str())
            || !dataset
                .get("location")
                .and_then(Value::as_str)
                .is_some_and(|loc| loc.eq_ignore_ascii_case(&self.location))
        {
            return Err(AnalyticsError::policy_denied(
                "bigquery_dataset_unverified",
                "query_bigquery_export",
            ));
        }
        let url = google_endpoint(BQ, &["projects", &self.billing_project, "queries"])?;
        let dry = self
            .response(
                self.http
                    .post(url.clone())
                    .bearer_auth(token)
                    .json(&self.request_body(&plan, true)),
            )
            .await?;
        check_query_page(&dry)?;
        if has_query_warnings(&dry)? {
            return Err(job_failure(
                "BigQuery dry run returned diagnostics; execution was not started",
            ));
        }
        let estimate = dry
            .get("totalBytesProcessed")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(bad_provider)?;
        let mut output = json!({
            "report_kind":plan.report_kind, "property":plan.property, "dataset":plan.dataset,
            "location":self.location, "estimated_bytes_processed":estimate.to_string(),
            "maximum_bytes_billed":plan.maximum_bytes.to_string(), "executed":false,
            "source":"ga4_daily_bigquery_export", "aggregation":"observed export events, not modeled Data API metrics",
            "privacy":"aggregate groups with at least five records; not an anonymity guarantee"
        });
        if !plan.execute || estimate > plan.maximum_bytes {
            output["execution_blocked_by_estimate"] = json!(estimate > plan.maximum_bytes);
            return Ok(output);
        }
        let mut page = self
            .response(
                self.http
                    .post(url)
                    .bearer_auth(token)
                    .json(&self.request_body(&plan, false)),
            )
            .await?;
        check_query_page(&page)?;
        let job = page.get("jobReference").ok_or_else(bad_provider)?.clone();
        let job_id = job
            .get("jobId")
            .and_then(Value::as_str)
            .filter(|s| valid_segment(s, 1024))
            .ok_or_else(bad_provider)?;
        if job.get("projectId").and_then(Value::as_str) != Some(self.billing_project.as_str())
            || !job
                .get("location")
                .and_then(Value::as_str)
                .is_some_and(|loc| loc.eq_ignore_ascii_case(&self.location))
        {
            return Err(bad_provider());
        }
        let results_url =
            google_endpoint(BQ, &["projects", &self.billing_project, "queries", job_id])?;
        let mut rows = Vec::new();
        let mut complete = false;
        let mut truncated = false;
        let mut fetched = 0;
        let mut verified = false;
        let mut warnings = false;
        let mut seen = HashSet::new();
        for i in 0..MAX_PAGES {
            check_query_page(&page)?;
            warnings |= has_query_warnings(&page)?;
            complete = page
                .get("jobComplete")
                .and_then(Value::as_bool)
                .ok_or_else(bad_provider)?;
            if complete {
                if !verified {
                    let status = self
                        .response(
                            self.http
                                .get(google_endpoint(
                                    BQ,
                                    &["projects", &self.billing_project, "jobs", job_id],
                                )?)
                                .bearer_auth(token)
                                .query(&[("location", self.location.as_str())]),
                        )
                        .await?;
                    verify_completed_job(&status, &job)?;
                    verified = true;
                }
                append_rows(&page, &plan, &mut rows)?;
                fetched += 1;
            }
            let next = if complete {
                unique_page_token(&page, &mut seen)?
            } else {
                None
            };
            if complete && next.is_none() {
                break;
            }
            if rows.len() >= plan.max_rows as usize || i + 1 == MAX_PAGES {
                truncated = complete && next.is_some();
                break;
            }
            let mut req = self
                .http
                .get(results_url.clone())
                .bearer_auth(token)
                .query(&[("location", self.location.as_str()), ("timeoutMs", "3000")])
                .query(&[("maxResults", (plan.max_rows - rows.len() as u32).min(100))]);
            if let Some(next) = &next {
                req = req.query(&[("pageToken", next)]);
            }
            // A failed getQueryResults request is not an empty successful report.
            // Propagate the sanitized error; never submit a replacement job.
            page = self.response(req).await?;
        }
        output["executed"] = json!(true);
        output["job_reference"] = json!({
            "jobId": job_id,
            "projectId": self.billing_project,
            "location": self.location,
        });
        output["job_complete"] = json!(complete);
        output["job_status_verified"] = json!(verified);
        output["provider_diagnostics_present"] = json!(warnings);
        output["truncated"] = json!(truncated);
        output["pages_fetched"] = json!(fetched);
        output["rows"] = json!(rows);
        output["columns"] = json!(plan.columns);
        output["remote_cancellation"] =
            json!("read-only scope cannot cancel jobs; 30-second job timeout is best effort");
        Ok(output)
    }
}

fn job_failure(message: &'static str) -> AnalyticsError {
    AnalyticsError::UpstreamApi {
        status: 400,
        message: message.to_string(),
    }
}

// QueryResponse and GetQueryResultsResponse expose errors[] (which can include
// warnings). A fatal errorResult, if supplied, always fails closed. Do not echo
// provider messages: they may contain sensitive query or resource details.
fn check_query_page(page: &Value) -> Result<(), AnalyticsError> {
    if !page.is_object() {
        return Err(bad_provider());
    }
    if page
        .get("errorResult")
        .is_some_and(|value| !value.is_null())
        || page
            .pointer("/status/errorResult")
            .is_some_and(|value| !value.is_null())
    {
        return Err(job_failure(
            "BigQuery job failed; inspect the job before retrying",
        ));
    }
    Ok(())
}

fn has_query_warnings(page: &Value) -> Result<bool, AnalyticsError> {
    match page.get("errors") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Array(errors)) => Ok(!errors.is_empty()),
        _ => Err(bad_provider()),
    }
}

// jobs.get is authoritative for terminal failure (status.errorResult), while
// errors[] in query-result responses is not by itself a terminal verdict.
fn verify_completed_job(status: &Value, expected: &Value) -> Result<(), AnalyticsError> {
    let actual = status.get("jobReference").ok_or_else(bad_provider)?;
    for key in ["jobId", "projectId"] {
        if actual.get(key).and_then(Value::as_str) != expected.get(key).and_then(Value::as_str) {
            return Err(bad_provider());
        }
    }
    if !actual
        .get("location")
        .and_then(Value::as_str)
        .is_some_and(|location| {
            expected
                .get("location")
                .and_then(Value::as_str)
                .is_some_and(|expected| location.eq_ignore_ascii_case(expected))
        })
    {
        return Err(bad_provider());
    }
    check_query_page(status)?;
    if status.pointer("/status/state").and_then(Value::as_str) != Some("DONE") {
        return Err(bad_provider());
    }
    Ok(())
}

fn bounded_page_token(page: &Value) -> Result<Option<String>, AnalyticsError> {
    let token = page.get("nextPageToken").or_else(|| page.get("pageToken"));
    match token {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) if s.len() <= 4096 => Ok(Some(s.clone())),
        _ => Err(bad_provider()),
    }
}

fn unique_page_token(
    page: &Value,
    seen: &mut HashSet<String>,
) -> Result<Option<String>, AnalyticsError> {
    let token = bounded_page_token(page)?;
    if token
        .as_ref()
        .is_some_and(|token| !seen.insert(token.clone()))
    {
        return Err(bad_provider());
    }
    Ok(token)
}

fn append_rows(
    page: &Value,
    plan: &ExportPlan,
    rows: &mut Vec<Value>,
) -> Result<(), AnalyticsError> {
    if let Some(schema) = page.pointer("/schema/fields") {
        let names = schema.as_array().ok_or_else(bad_provider)?;
        if names.len() != plan.columns.len()
            || !names
                .iter()
                .zip(&plan.columns)
                .all(|(field, name)| field.get("name").and_then(Value::as_str) == Some(*name))
        {
            return Err(bad_provider());
        }
    } else if rows.is_empty() {
        return Err(bad_provider());
    }
    let Some(raw) = page.get("rows") else {
        return Ok(());
    };
    for row in raw.as_array().ok_or_else(bad_provider)? {
        if rows.len() >= plan.max_rows as usize {
            return Err(bad_provider());
        }
        let fields = row
            .get("f")
            .and_then(Value::as_array)
            .ok_or_else(bad_provider)?;
        if fields.len() != plan.columns.len() {
            return Err(bad_provider());
        }
        let mut values = serde_json::Map::new();
        for (field, name) in fields.iter().zip(&plan.columns) {
            let value = field.get("v").ok_or_else(bad_provider)?;
            if !value.is_null() && !value.as_str().is_some_and(|s| s.len() <= 128) {
                return Err(bad_provider());
            }
            values.insert((*name).to_string(), value.clone());
        }
        rows.push(Value::Object(values));
    }
    Ok(())
}

async fn response(request: RequestBuilder) -> Result<Value, AnalyticsError> {
    let mut result = request.send().await.map_err(|_| AnalyticsError::Internal(
        "BigQuery export request interrupted; a submitted job may still run; inspect billing project before retrying".to_string()))?;
    if !result.status().is_success() {
        return Err(AnalyticsError::UpstreamApi {status:result.status().as_u16(),message:"Google rejected the bounded export request; check dataset IAM, region, export link and cost limit".to_string()});
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = result.chunk().await.map_err(|_| bad_provider())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
            return Err(bad_provider());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| bad_provider())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    struct ExpectedRequest {
        method: &'static str,
        path: &'static str,
        dry_run: Option<bool>,
        reply: Result<Value, AnalyticsError>,
    }

    pub(super) struct ProviderFixture(VecDeque<ExpectedRequest>);

    impl ProviderFixture {
        pub(super) fn respond(&mut self, builder: RequestBuilder) -> Result<Value, AnalyticsError> {
            let expected = self
                .0
                .pop_front()
                .expect("unexpected additional provider call");
            let request = builder.build().unwrap();
            assert_eq!(request.method().as_str(), expected.method);
            assert_eq!(request.url().scheme(), "https");
            assert_eq!(request.url().path(), expected.path);
            assert_eq!(
                request.headers().get("authorization").unwrap(),
                "Bearer fixture-token"
            );
            if let Some(dry) = expected.dry_run {
                let body: Value = serde_json::from_slice(
                    request.body().and_then(reqwest::Body::as_bytes).unwrap(),
                )
                .unwrap();
                assert_eq!(body["dryRun"], dry);
                assert_eq!(body["maximumBytesBilled"], "100000");
                assert_eq!(body["location"], "US");
                assert_eq!(body["useLegacySql"], false);
                assert_eq!(body["parameterMode"], "NAMED");
                assert_eq!(
                    body["queryParameters"][0]["parameterValue"]["value"],
                    "20261101"
                );
            }
            expected.reply
        }
    }

    fn step(
        method: &'static str,
        path: &'static str,
        dry_run: Option<bool>,
        reply: Value,
    ) -> ExpectedRequest {
        ExpectedRequest {
            method,
            path,
            dry_run,
            reply: Ok(reply),
        }
    }

    fn provider_fixture(dry: Value) -> Vec<ExpectedRequest> {
        vec![
            step(
                "GET",
                "/v1alpha/properties/42/bigQueryLinks",
                None,
                json!({"bigqueryLinks":[{"project":"projects/12345","datasetLocation":"US","dailyExportEnabled":true}]}),
            ),
            step(
                "GET",
                "/bigquery/v2/projects/export-fixture/datasets/analytics_42",
                None,
                json!({"datasetReference":{"projectId":"export-fixture","datasetId":"analytics_42"},"location":"US"}),
            ),
            step(
                "POST",
                "/bigquery/v2/projects/billing-fixture/queries",
                Some(true),
                dry,
            ),
        ]
    }

    async fn query_fixture(steps: Vec<ExpectedRequest>) -> Result<Value, AnalyticsError> {
        let mut client = fixture(true);
        let provider = Arc::new(Mutex::new(ProviderFixture(steps.into())));
        client.fixture = Some(provider.clone());
        let plan = client.plan(args("daily_sessions", true)).unwrap();
        let result = client.query(plan, "fixture-token").await;
        assert!(
            provider.lock().unwrap().0.is_empty(),
            "missing expected provider call"
        );
        result
    }

    #[tokio::test]
    async fn provider_fixture_blocks_over_budget_and_diagnostic_dry_runs() {
        let output = query_fixture(provider_fixture(json!({"totalBytesProcessed":"100001"})))
            .await
            .unwrap();
        assert_eq!(output["executed"], false);
        assert_eq!(output["execution_blocked_by_estimate"], true);
        assert!(
            query_fixture(provider_fixture(
                json!({"totalBytesProcessed":"1","errors":[{"reason":"warning"}]})
            ))
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn provider_fixture_terminal_failure_never_retries_or_returns_rows() {
        let mut steps = provider_fixture(json!({"totalBytesProcessed":"1"}));
        let reference = json!({"jobId":"job_1","projectId":"billing-fixture","location":"US"});
        steps.push(step(
            "POST",
            "/bigquery/v2/projects/billing-fixture/queries",
            Some(false),
            json!({"jobReference":reference,"jobComplete":true}),
        ));
        steps.push(step("GET", "/bigquery/v2/projects/billing-fixture/jobs/job_1", None,
            json!({"jobReference":reference,"status":{"state":"DONE","errorResult":{"message":"private fixture"}}})));
        let error = query_fixture(steps).await.unwrap_err();
        assert_eq!(error.code(), "UPSTREAM_REJECTED");
        assert!(!error.to_string().contains("private fixture"));
    }

    #[test]
    fn repeated_provider_page_tokens_fail_closed() {
        let mut seen = HashSet::new();
        let page = json!({"pageToken":"repeat"});
        assert_eq!(
            unique_page_token(&page, &mut seen).unwrap().as_deref(),
            Some("repeat")
        );
        assert!(unique_page_token(&page, &mut seen).is_err());
        assert!(unique_page_token(&json!({}), &mut seen).unwrap().is_none());
    }

    fn fixture(execute: bool) -> BigQueryExportClient {
        BigQueryExportClient::new(
            Duration::from_secs(2),
            "export-fixture".into(),
            "12345".into(),
            "billing-fixture".into(),
            "US".into(),
            1_000_000,
            execute,
        )
        .unwrap()
    }

    fn args(kind: &str, execute: bool) -> ExportQueryArgs {
        ExportQueryArgs {
            property_id: PropertyId::Number(42),
            report_kind: kind.into(),
            start_date: "2026-11-01".into(),
            end_date: "2026-11-30".into(),
            maximum_bytes_billed: 100_000,
            max_rows: Some(10),
            execute,
        }
    }

    #[test]
    fn export_urls_are_https_and_path_segments_cannot_change_origin() {
        let url =
            google_endpoint(BQ, &["projects", "billing-fixture", "queries", "job_1"]).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("bigquery.googleapis.com"));
        assert_eq!(
            url.path(),
            "/bigquery/v2/projects/billing-fixture/queries/job_1"
        );
        let url = google_endpoint(ADMIN, &["properties", "42", "bigQueryLinks"]).unwrap();
        assert_eq!(url.host_str(), Some("analyticsadmin.googleapis.com"));
        assert_eq!(url.path(), "/v1alpha/properties/42/bigQueryLinks");
        assert!(google_endpoint("http://example.invalid", &[]).is_err());
        let injected = google_endpoint(BQ, &["projects", "//example.invalid"]).unwrap();
        assert_eq!(injected.host_str(), Some("bigquery.googleapis.com"));
        assert!(injected.path().contains("%2F%2F"));
    }

    #[test]
    fn templates_are_fixed_parameterized_and_budgeted() {
        let client = fixture(true);
        for kind in [
            "daily_events",
            "daily_sessions",
            "event_transitions",
            "first_user_acquisition",
            "item_categories",
        ] {
            let plan = client.plan(args(kind, true)).unwrap();
            assert!(
                plan.query
                    .contains("`export-fixture.analytics_42.events_*`")
            );
            assert!(
                plan.query
                    .contains("_TABLE_SUFFIX BETWEEN @start_suffix AND @end_suffix")
            );
            assert!(plan.query.contains("LIMIT @row_limit"));
            assert!(!plan.query.contains("20261101"));
            if kind == "first_user_acquisition" {
                assert!(plan.query.contains("{0,39}"));
            } else if kind == "item_categories" {
                assert!(plan.query.contains("{0,63}"));
            }
            let dry = client.request_body(&plan, true);
            let run = client.request_body(&plan, false);
            assert_eq!(dry["dryRun"], true);
            assert_eq!(run["dryRun"], false);
            assert_eq!(run["maximumBytesBilled"], "100000");
            assert_eq!(run["jobTimeoutMs"], "30000");
            assert_eq!(run["timeoutMs"], 5000);
            assert_eq!(dry["queryParameters"], run["queryParameters"]);
        }
    }

    #[test]
    fn rejects_ambient_billing_and_invalid_ranges() {
        let client = fixture(false);
        assert!(client.plan(args("daily_events", true)).is_err());
        assert!(client.plan(args("SELECT *", false)).is_err());
        let mut a = args("daily_events", false);
        a.end_date = "2026-12-02".into();
        assert!(client.plan(a).is_err());
        let mut a = args("daily_events", false);
        a.maximum_bytes_billed = 1_000_001;
        assert!(client.plan(a).is_err());
        assert!(parse_date("start_date", "2026-02-29").is_err());
        assert!(!valid_project("a-b`evil"));
    }

    #[test]
    fn distinguishes_terminal_job_errors_from_query_diagnostics() {
        let reference = json!({"jobId":"job_1","projectId":"billing-fixture","location":"US"});
        let fatal = json!({"jobComplete":true,"errorResult":{"message":"sensitive fixture"}});
        let error = check_query_page(&fatal).unwrap_err();
        assert_eq!(error.code(), "UPSTREAM_REJECTED");
        assert!(!error.to_string().contains("sensitive fixture"));
        let warning = json!({"jobComplete":true,"errors":[{"reason":"warning"}]});
        assert!(check_query_page(&warning).is_ok());
        assert!(has_query_warnings(&warning).unwrap());
        assert!(has_query_warnings(&json!({"errors":{}})).is_err());
        let done = json!({"jobReference":reference,"status":{"state":"DONE"}});
        assert!(verify_completed_job(&done, &reference).is_ok());
        let failed = json!({"jobReference":reference,"status":{"state":"DONE","errorResult":{"reason":"stopped","message":"private details"}}});
        assert_eq!(
            verify_completed_job(&failed, &reference)
                .unwrap_err()
                .code(),
            "UPSTREAM_REJECTED"
        );
        let running = json!({"jobReference":reference,"status":{"state":"RUNNING"}});
        assert!(verify_completed_job(&running, &reference).is_err());
        let mismatched = json!({"jobReference":{"jobId":"other","projectId":"billing-fixture","location":"US"},"status":{"state":"DONE"}});
        assert!(verify_completed_job(&mismatched, &reference).is_err());
    }

    #[test]
    fn validates_aggregate_schema_and_bounded_scalar_rows() {
        let plan = fixture(false).plan(args("daily_sessions", false)).unwrap();
        let mut rows = Vec::new();
        append_rows(&json!({"schema":{"fields":[{"name":"event_date"},{"name":"observed_sessions"}]},"rows":[{"f":[{"v":"20261101"},{"v":"42"}]}]}),&plan,&mut rows).unwrap();
        assert_eq!(rows[0]["observed_sessions"], "42");
        assert!(
            append_rows(
                &json!({"schema":{"fields":[{"name":"event_date"},{"name":"user_pseudo_id"}]}}),
                &plan,
                &mut rows
            )
            .is_err()
        );
        assert!(
            append_rows(
                &json!({"rows":[{"f":[{"v":"20261101"},{"v":{"raw":"identifier"}}]}]}),
                &plan,
                &mut rows
            )
            .is_err()
        );
    }
}
