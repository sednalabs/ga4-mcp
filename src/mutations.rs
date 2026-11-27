//! Guarded GA4 create-only configuration actions. No ambient mutation authority.
use std::time::{Duration, Instant};

use mcp_toolkit_core::guarded_action::{GuardedActionPosture, GuardedActionRuntimeMode};
use mcp_toolkit_core::scoped_cursor::{CursorLimits, CursorScope, ScopedCursorStore};
use rmcp::model::{
    CallToolResponse, ElicitRequest, ElicitRequestParams, ElicitResult, ElicitationAction,
    InputRequest, InputRequests, InputRequiredResult, InputResponses, ProtocolVersion,
};
use rmcp::service::{RequestContext, RoleServer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::{Date, Month};

use crate::config::{ANALYTICS_EDIT_SCOPE, CapabilityProfile, UpstreamTokenSource};
use crate::contract;
use crate::error::AnalyticsError;
use crate::ga_client::PropertyId;
use crate::server::AnalyticsMcp;

const APPROVAL_TTL: Duration = Duration::from_secs(120);
const APPROVAL_KEY: &str = "approve_ga4_create";

pub(crate) fn approval_store() -> ScopedCursorStore {
    ScopedCursorStore::new(CursorLimits {
        ttl: APPROVAL_TTL,
        max_entries: 16,
        max_payload_bytes: 8192,
    })
    .expect("fixed valid approval store limits")
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnnotationArgs {
    /// The Google Analytics property id as integer or "properties/<id>".
    pub property_id: PropertyId,
    /// Annotation title (nonblank, up to 100 characters).
    pub title: String,
    /// Optional description (up to 500 characters).
    #[serde(default)]
    pub description: Option<String>,
    /// One of PURPLE, BROWN, BLUE, GREEN, RED or CYAN.
    pub color: String,
    /// Inclusive Gregorian start date, YYYY-MM-DD.
    pub start_date: String,
    /// Optional inclusive end date, YYYY-MM-DD.
    #[serde(default)]
    pub end_date: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustomDimensionArgs {
    /// The Google Analytics property id as integer or "properties/<id>".
    pub property_id: PropertyId,
    /// Tagging parameter name.
    pub parameter_name: String,
    /// Dimension display name.
    pub display_name: String,
    /// Optional description (up to 150 characters).
    #[serde(default)]
    pub description: Option<String>,
    /// Dimension scope: EVENT, USER or ITEM.
    pub scope: String,
    /// Optional ads personalization restriction, supported only for USER scope.
    #[serde(default)]
    pub disallow_ads_personalization: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyEventArgs {
    /// The Google Analytics property id as integer or "properties/<id>".
    pub property_id: PropertyId,
    /// GA4 event name.
    pub event_name: String,
    /// ONCE_PER_EVENT or ONCE_PER_SESSION.
    pub counting_method: String,
    /// Optional finite default numeric value; requires default_currency_code.
    #[serde(default)]
    pub default_numeric_value: Option<f64>,
    /// Optional three-letter ISO 4217 currency code; requires default_numeric_value.
    #[serde(default)]
    pub default_currency_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationPlan {
    pub tool: String,
    pub parent: String,
    pub collection: String,
    pub version: String,
    pub body: Value,
}

pub(crate) struct ApprovedMutation(MutationPlan);

impl ApprovedMutation {
    pub(crate) fn into_plan(self) -> MutationPlan {
        self.0
    }
}

fn invalid(field: &'static str, message: &'static str) -> AnalyticsError {
    AnalyticsError::invalid(field, message)
}

fn validate_text(
    field: &'static str,
    text: &str,
    max: usize,
    required: bool,
) -> Result<(), AnalyticsError> {
    if (required && text.trim().is_empty())
        || text.chars().count() > max
        || text.chars().any(char::is_control)
    {
        return Err(invalid(field, "invalid or excessive text"));
    }
    Ok(())
}

fn validate_identifier(field: &'static str, text: &str, max: usize) -> Result<(), AnalyticsError> {
    if text.is_empty()
        || text.len() > max
        || !text.as_bytes()[0].is_ascii_alphabetic()
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(invalid(
            field,
            "expected a bounded ASCII identifier beginning with a letter",
        ));
    }
    Ok(())
}

fn parse_date(field: &'static str, text: &str) -> Result<Date, AnalyticsError> {
    let bytes = text.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        return Err(invalid(field, "expected YYYY-MM-DD"));
    }
    let year = text[..4]
        .parse::<i32>()
        .map_err(|_| invalid(field, "invalid year"))?;
    let month = text[5..7]
        .parse::<u8>()
        .ok()
        .and_then(|m| Month::try_from(m).ok())
        .ok_or_else(|| invalid(field, "invalid month"))?;
    let day = text[8..]
        .parse::<u8>()
        .map_err(|_| invalid(field, "invalid day"))?;
    if !(1..=9999).contains(&year) {
        return Err(invalid(field, "invalid year"));
    }
    Date::from_calendar_date(year, month, day).map_err(|_| invalid(field, "invalid calendar date"))
}

fn date_json(date: Date) -> Value {
    json!({"year": date.year(), "month": u8::from(date.month()), "day": date.day()})
}

impl AnnotationArgs {
    pub(crate) fn plan(self) -> Result<MutationPlan, AnalyticsError> {
        validate_text("title", &self.title, 100, true)?;
        if let Some(description) = &self.description {
            validate_text("description", description, 500, false)?;
        }
        if !["PURPLE", "BROWN", "BLUE", "GREEN", "RED", "CYAN"].contains(&self.color.as_str()) {
            return Err(invalid("color", "unsupported user annotation color"));
        }
        let start = parse_date("start_date", &self.start_date)?;
        let mut body = json!({"title": self.title, "color": self.color});
        if let Some(description) = self.description {
            body["description"] = json!(description);
        }
        if let Some(end) = self.end_date {
            let end = parse_date("end_date", &end)?;
            if end < start {
                return Err(invalid("end_date", "end date precedes start date"));
            }
            body["annotationDateRange"] =
                json!({"startDate":date_json(start), "endDate":date_json(end)});
        } else {
            body["annotationDate"] = date_json(start);
        }
        Ok(MutationPlan {
            tool: "create_reporting_data_annotation".into(),
            parent: self.property_id.to_resource_name()?,
            collection: "reportingDataAnnotations".into(),
            version: "v1alpha".into(),
            body,
        })
    }
}

impl CustomDimensionArgs {
    pub(crate) fn plan(self) -> Result<MutationPlan, AnalyticsError> {
        let max = match self.scope.as_str() {
            "USER" => 24,
            "EVENT" | "ITEM" => 40,
            _ => return Err(invalid("scope", "unsupported dimension scope")),
        };
        validate_identifier("parameter_name", &self.parameter_name, max)?;
        validate_text("display_name", &self.display_name, 82, true)?;
        if !self
            .display_name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
            || !self
                .display_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b' ' || byte == b'_')
        {
            return Err(invalid(
                "display_name",
                "expected letters, digits, spaces or underscores, beginning with a letter",
            ));
        }
        if let Some(description) = &self.description {
            validate_text("description", description, 150, false)?;
        }
        if self.disallow_ads_personalization.is_some() && self.scope != "USER" {
            return Err(invalid(
                "disallow_ads_personalization",
                "only supported for USER dimensions",
            ));
        }
        let mut body = json!({"parameterName": self.parameter_name, "displayName": self.display_name, "scope": self.scope});
        if let Some(value) = self.description {
            body["description"] = json!(value);
        }
        if let Some(value) = self.disallow_ads_personalization {
            body["disallowAdsPersonalization"] = json!(value);
        }
        Ok(MutationPlan {
            tool: "create_custom_dimension".into(),
            parent: self.property_id.to_resource_name()?,
            collection: "customDimensions".into(),
            version: "v1beta".into(),
            body,
        })
    }
}

impl KeyEventArgs {
    pub(crate) fn plan(self) -> Result<MutationPlan, AnalyticsError> {
        validate_identifier("event_name", &self.event_name, 40)?;
        if !["ONCE_PER_EVENT", "ONCE_PER_SESSION"].contains(&self.counting_method.as_str()) {
            return Err(invalid("counting_method", "unsupported counting method"));
        }
        let mut body =
            json!({"eventName": self.event_name, "countingMethod": self.counting_method});
        match (self.default_numeric_value, self.default_currency_code) {
            (None, None) => {}
            (Some(value), Some(currency))
                if value.is_finite()
                    && currency.len() == 3
                    && currency.bytes().all(|b| b.is_ascii_uppercase()) =>
            {
                body["defaultValue"] = json!({"numericValue": value, "currencyCode": currency});
            }
            _ => {
                return Err(invalid(
                    "default_numeric_value",
                    "supply a finite value and a three-letter uppercase currency code together",
                ));
            }
        }
        Ok(MutationPlan {
            tool: "create_key_event".into(),
            parent: self.property_id.to_resource_name()?,
            collection: "keyEvents".into(),
            version: "v1beta".into(),
            body,
        })
    }
}

#[derive(Deserialize, Serialize, JsonSchema)]
struct UserApproval {
    /// Explicitly approve the exact displayed GA4 change.
    approved: bool,
}
rmcp::elicit_safe!(UserApproval);

fn scope(plan: &MutationPlan) -> Result<CursorScope, AnalyticsError> {
    CursorScope::new("trusted-local-stdio", &plan.tool, "configured-credential")
        .map_err(|_| AnalyticsError::Internal("invalid local approval scope".to_string()))
}

fn approved_response(responses: Option<&InputResponses>) -> bool {
    responses
        .filter(|responses| responses.len() == 1)
        .and_then(|responses| responses.get(APPROVAL_KEY))
        .and_then(|value| serde_json::from_value::<ElicitResult>(value.clone()).ok())
        .is_some_and(|result| {
            result.action == ElicitationAction::Accept
                && result
                    .content
                    .as_ref()
                    .and_then(|content| content.get("approved"))
                    .and_then(Value::as_bool)
                    == Some(true)
        })
}

fn consume_approval(
    store: &ScopedCursorStore,
    plan: MutationPlan,
    state: &str,
    responses: Option<&InputResponses>,
) -> Result<ApprovedMutation, AnalyticsError> {
    let expected = serde_json::to_value(&plan)
        .map_err(|_| AnalyticsError::Internal("approval serialization failed".to_string()))?;
    let stored = store
        .consume(state, &scope(&plan)?)
        .map_err(|_| AnalyticsError::policy_denied("mutation_approval_required", &plan.tool))?;
    if stored != expected || !approved_response(responses) {
        return Err(AnalyticsError::policy_denied(
            "mutation_approval_required",
            &plan.tool,
        ));
    }
    Ok(ApprovedMutation(plan))
}

pub(crate) async fn execute(
    server: &AnalyticsMcp,
    plan: Result<MutationPlan, AnalyticsError>,
    context: RequestContext<RoleServer>,
    state: Option<String>,
    responses: Option<InputResponses>,
) -> CallToolResponse {
    let started = Instant::now();
    match execute_inner(server, plan, context, state, responses).await {
        Ok(result) => result,
        Err(error) => contract::error(error, contract::elapsed_ms(started)).into(),
    }
}

async fn execute_inner(
    server: &AnalyticsMcp,
    plan: Result<MutationPlan, AnalyticsError>,
    context: RequestContext<RoleServer>,
    state: Option<String>,
    responses: Option<InputResponses>,
) -> Result<CallToolResponse, AnalyticsError> {
    let started = Instant::now();
    let plan = plan?;
    let runtime = if server.capability_profile() == CapabilityProfile::Mutation {
        GuardedActionRuntimeMode::Enabled
    } else {
        GuardedActionRuntimeMode::ReadOnly
    };
    runtime
        .assert_allowed(&plan.tool, GuardedActionPosture::guarded_apply())
        .map_err(|_| {
            AnalyticsError::policy_denied(server.capability_profile().as_str(), &plan.tool)
        })?;
    if context
        .extensions
        .get::<axum::http::request::Parts>()
        .is_some()
        || server.upstream_token_source() != UpstreamTokenSource::Config
        || server.client.upstream_token_source() != UpstreamTokenSource::Config
        || server.client.analytics_scope() != ANALYTICS_EDIT_SCOPE
        || context.ct.is_cancelled()
    {
        return Err(AnalyticsError::policy_denied(
            "mutation_local_stdio_only",
            &plan.tool,
        ));
    }
    if !context
        .client_capabilities()
        .is_some_and(|capabilities| capabilities.elicitation.is_some())
    {
        return Err(AnalyticsError::policy_denied(
            "mutation_client_approval_required",
            &plan.tool,
        ));
    }
    let message = format!(
        "Approve the following one-time creation in Google Analytics? This changes live configuration. Property: {}. Tool: {}. Exact payload: {}",
        plan.parent, plan.tool, plan.body
    );
    let modern = context
        .protocol_version()
        .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
    let approved = if modern {
        if let Some(state) = state {
            consume_approval(&server.mutation_approvals, plan, &state, responses.as_ref())?
        } else {
            if responses.is_some() {
                return Err(AnalyticsError::policy_denied(
                    "mutation_approval_required",
                    &plan.tool,
                ));
            }
            let token = uuid::Uuid::new_v4().to_string();
            let payload = serde_json::to_value(&plan).map_err(|_| {
                AnalyticsError::Internal("approval serialization failed".to_string())
            })?;
            server
                .mutation_approvals
                .insert(&token, scope(&plan)?, payload)
                .map_err(|_| {
                    AnalyticsError::Internal("approval capacity unavailable".to_string())
                })?;
            let mut requests = InputRequests::new();
            requests.insert(APPROVAL_KEY.to_string(), InputRequest::Elicitation(ElicitRequest::new(
                ElicitRequestParams::FormElicitationParams {
                    meta: None,
                    message,
                    requested_schema: serde_json::from_value(json!({"type":"object","properties":{"approved":{"type":"boolean","description":"Approve this exact change"}},"required":["approved"]}))
                        .expect("valid approval schema"),
                },
            )));
            return Ok(InputRequiredResult::new(Some(requests), Some(token)).into());
        }
    } else {
        if state.is_some() || responses.is_some() {
            return Err(AnalyticsError::policy_denied(
                "mutation_approval_required",
                &plan.tool,
            ));
        }
        let approval =
            tokio::time::timeout(APPROVAL_TTL, context.peer.elicit::<UserApproval>(message)).await;
        if !matches!(approval, Ok(Ok(Some(UserApproval { approved: true })))) {
            return Err(AnalyticsError::policy_denied(
                "mutation_approval_required",
                &plan.tool,
            ));
        }
        ApprovedMutation(plan)
    };
    if context.ct.is_cancelled() {
        return Err(AnalyticsError::policy_denied(
            "mutation_cancelled",
            "create",
        ));
    }
    let data = server.client.apply_approved_mutation(approved).await?;
    Ok(contract::success(data, contract::elapsed_ms(started)).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event(name: &str) -> MutationPlan {
        KeyEventArgs {
            property_id: PropertyId::Number(123),
            event_name: name.into(),
            counting_method: "ONCE_PER_EVENT".into(),
            default_numeric_value: None,
            default_currency_code: None,
        }
        .plan()
        .expect("valid event")
    }

    fn response(approved: bool) -> InputResponses {
        [(
            APPROVAL_KEY.into(),
            json!({"action":"accept","content":{"approved":approved}}),
        )]
        .into()
    }

    #[test]
    fn exact_plan_approval_is_single_use_and_tamper_resistant() {
        let store = approval_store();
        let plan = key_event("purchase");
        let token = "00000000-0000-4000-8000-000000000001";
        store
            .insert(
                token,
                scope(&plan).unwrap(),
                serde_json::to_value(&plan).unwrap(),
            )
            .unwrap();
        assert!(
            consume_approval(&store, key_event("other"), token, Some(&response(true))).is_err()
        );
        assert!(consume_approval(&store, plan.clone(), token, Some(&response(true))).is_err());
        store
            .insert(
                token,
                scope(&plan).unwrap(),
                serde_json::to_value(&plan).unwrap(),
            )
            .unwrap();
        assert!(consume_approval(&store, plan.clone(), token, Some(&response(false))).is_err());
        store
            .insert(
                token,
                scope(&plan).unwrap(),
                serde_json::to_value(&plan).unwrap(),
            )
            .unwrap();
        assert!(consume_approval(&store, plan.clone(), token, Some(&response(true))).is_ok());
        assert!(consume_approval(&store, plan, token, Some(&response(true))).is_err());
        assert!(!approved_response(None));
        assert!(!approved_response(Some(
            &[(
                APPROVAL_KEY.into(),
                json!({"action":"decline","content":{"approved":true}})
            )]
            .into()
        )));
    }

    #[test]
    fn payload_validation_is_bounded_and_provider_shaped() {
        let args: AnnotationArgs = serde_json::from_value(json!({"property_id":123,"title":"Release","color":"BLUE","start_date":"2026-11-27","end_date":"2026-11-28"})).unwrap();
        let plan = args.plan().unwrap();
        assert_eq!(plan.parent, "properties/123");
        assert_eq!(
            plan.body["annotationDateRange"]["startDate"],
            json!({"year":2026,"month":11,"day":27})
        );
        assert!(parse_date("date", "2026-02-29").is_err());
        assert!(parse_date("date", "0000-01-01").is_err());
        let args: CustomDimensionArgs = serde_json::from_value(json!({"property_id":123,"parameter_name":"campaign_code","display_name":"Campaign code","scope":"EVENT","disallow_ads_personalization":true})).unwrap();
        assert!(args.plan().is_err());
        assert_eq!(
            key_event("purchase").body,
            json!({"eventName":"purchase","countingMethod":"ONCE_PER_EVENT"})
        );
    }

    async fn offline_mutation_server() -> AnalyticsMcp {
        use crate::config::{Cli, Settings};
        use crate::ga_client::AnalyticsApiClient;
        use crate::scratchpad::{
            DuckDbEngine, ScratchpadSessionConfig, ScratchpadSessionManager, SharedScratchpadEngine,
        };
        use clap::Parser;
        use std::sync::Arc;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut settings = Settings::from_cli(Cli::parse_from([
            "ga4-mcp",
            "--capability-profile",
            "mutation",
            "--analytics-scope",
            ANALYTICS_EDIT_SCOPE,
            "--upstream-token-source",
            "config",
        ]))
        .unwrap();
        settings.capability_profile = CapabilityProfile::ReadOnly;
        settings.admin_base_url = "https://fixture.invalid".to_string();
        let client = Arc::new(AnalyticsApiClient::from_settings(&settings).await.unwrap());
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().unwrap());
        let sessions = Arc::new(
            ScratchpadSessionManager::new(
                engine,
                ScratchpadSessionConfig::new(Duration::from_secs(60), 2, 2, 100, 128),
            )
            .unwrap(),
        );
        AnalyticsMcp::new(
            client,
            sessions,
            CapabilityProfile::Mutation,
            UpstreamTokenSource::Config,
            "authorization".to_string(),
        )
    }

    // Exercise the actual RMCP line transport and tool router. The client is
    // deliberately constructed without a mutation transport and with a
    // non-Google Admin origin after CLI validation, so even an approved
    // request must stop before credential resolution or network I/O. Separate provider fixtures
    // exercise the exact POST and readback request shapes.
    #[tokio::test]
    async fn modern_stdio_approval_is_required_bound_and_single_use() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let server = offline_mutation_server().await;
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = rmcp::serve_server(server, server_io).await.unwrap();
            service.waiting().await.unwrap();
        });
        let (read, mut write) = tokio::io::split(client_io);
        let mut read = BufReader::new(read);

        async fn exchange(
            read: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
            write: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
            id: u64,
            params: Value,
        ) -> Value {
            let request = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": params
            });
            write
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            write.flush().await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let mut line = String::new();
                    assert_ne!(read.read_line(&mut line).await.unwrap(), 0);
                    let message: Value = serde_json::from_str(&line).unwrap();
                    if message["id"] == json!(id) {
                        assert!(message.get("error").is_none(), "{message}");
                        return message["result"].clone();
                    }
                }
            })
            .await
            .expect("bounded MCP response")
        }

        fn params(event: &str, supports_approval: bool) -> Value {
            let capabilities = if supports_approval {
                json!({"elicitation":{"form":{}}})
            } else {
                json!({})
            };
            json!({
                "name": "create_key_event",
                "arguments": {
                    "property_id": 123,
                    "event_name": event,
                    "counting_method": "ONCE_PER_EVENT"
                },
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {"name":"approval-fixture","version":"1"},
                    "io.modelcontextprotocol/clientCapabilities": capabilities
                }
            })
        }

        fn continuation(event: &str, state: &str, approved: bool) -> Value {
            let mut request = params(event, true);
            request["requestState"] = json!(state);
            request["inputResponses"] = json!({
                "approve_ga4_create": {"action":"accept","content":{"approved":approved}}
            });
            request
        }

        fn assert_denied(result: &Value, profile: &str) {
            assert_eq!(result["structuredContent"]["ok"], false, "{result}");
            assert_eq!(
                result["structuredContent"]["error"]["code"], "POLICY_DENIED",
                "{result}"
            );
            assert!(
                result["structuredContent"]["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(profile),
                "{result}"
            );
        }

        fn approval_state(result: &Value) -> &str {
            assert_eq!(result["resultType"], "input_required", "{result}");
            let prompt = &result["inputRequests"]["approve_ga4_create"];
            assert_eq!(prompt["method"], "elicitation/create");
            let message = prompt["params"]["message"].as_str().unwrap();
            assert!(message.contains("properties/123"));
            assert!(message.contains("purchase"));
            assert!(message.contains("ONCE_PER_EVENT"));
            result["requestState"]
                .as_str()
                .expect("opaque approval state")
        }

        let unsupported = exchange(&mut read, &mut write, 1, params("purchase", false)).await;
        assert_denied(&unsupported, "mutation_client_approval_required");

        let mut unsolicited = params("purchase", true);
        unsolicited["inputResponses"] = json!({
            "approve_ga4_create": {"action":"accept","content":{"approved":true}}
        });
        let denied = exchange(&mut read, &mut write, 2, unsolicited).await;
        assert_denied(&denied, "mutation_approval_required");

        let first = exchange(&mut read, &mut write, 3, params("purchase", true)).await;
        let state = approval_state(&first);
        let tampered = exchange(&mut read, &mut write, 4, continuation("other", state, true)).await;
        assert_denied(&tampered, "mutation_approval_required");
        let consumed = exchange(
            &mut read,
            &mut write,
            5,
            continuation("purchase", state, true),
        )
        .await;
        assert_denied(&consumed, "mutation_approval_required");

        let second = exchange(&mut read, &mut write, 6, params("purchase", true)).await;
        let state = approval_state(&second);
        let declined = exchange(
            &mut read,
            &mut write,
            7,
            continuation("purchase", state, false),
        )
        .await;
        assert_denied(&declined, "mutation_approval_required");

        let third = exchange(&mut read, &mut write, 8, params("purchase", true)).await;
        let state = approval_state(&third);
        let accepted = exchange(
            &mut read,
            &mut write,
            9,
            continuation("purchase", state, true),
        )
        .await;
        // Only a correctly approved call can reach the independent provider gate.
        assert_denied(&accepted, "mutation_disabled");
        let replay = exchange(
            &mut read,
            &mut write,
            10,
            continuation("purchase", state, true),
        )
        .await;
        assert_denied(&replay, "mutation_approval_required");

        drop(write);
        drop(read);
        tokio::time::timeout(Duration::from_secs(10), server_task)
            .await
            .expect("MCP service terminates")
            .unwrap();
    }

    #[tokio::test]
    async fn legacy_stdio_approval_requires_a_fresh_server_prompt() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let server = offline_mutation_server().await;
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = rmcp::serve_server(server, server_io).await.unwrap();
            service.waiting().await.unwrap();
        });
        let (read, mut write) = tokio::io::split(client_io);
        let mut read = BufReader::new(read);

        async fn send(write: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, message: Value) {
            write
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
            write.flush().await.unwrap();
        }

        async fn receive(
            read: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        ) -> Value {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut line = String::new();
                assert_ne!(read.read_line(&mut line).await.unwrap(), 0);
                serde_json::from_str(&line).unwrap()
            })
            .await
            .expect("bounded legacy MCP response")
        }

        send(
            &mut write,
            json!({
                "jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{
                    "protocolVersion":"2025-11-25",
                    "capabilities":{"elicitation":{"form":{}}},
                    "clientInfo":{"name":"legacy-approval-fixture","version":"1"}
                }
            }),
        )
        .await;
        let initialized = receive(&mut read).await;
        assert_eq!(initialized["id"], 1);
        assert!(initialized.get("error").is_none(), "{initialized}");
        assert_ne!(initialized["result"]["protocolVersion"], "2026-07-28");
        send(
            &mut write,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;

        for (id, event, answer, expected) in [
            (
                2,
                "purchase",
                json!({"action":"decline"}),
                "mutation_approval_required",
            ),
            (
                3,
                "purchase",
                json!({"action":"accept","content":{"approved":false}}),
                "mutation_approval_required",
            ),
            (
                4,
                "purchase",
                json!({"action":"accept","content":{"approved":true}}),
                "mutation_disabled",
            ),
            (
                5,
                "other",
                json!({"action":"cancel"}),
                "mutation_approval_required",
            ),
        ] {
            send(
                &mut write,
                json!({
                    "jsonrpc":"2.0","id":id,"method":"tools/call",
                    "params":{"name":"create_key_event","arguments":{
                        "property_id":123,"event_name":event,
                        "counting_method":"ONCE_PER_EVENT"
                    }}
                }),
            )
            .await;

            let prompt = receive(&mut read).await;
            assert_eq!(prompt["method"], "elicitation/create", "{prompt}");
            assert!(prompt.get("id").is_some(), "{prompt}");
            let message = prompt["params"]["message"].as_str().unwrap();
            assert!(message.contains("properties/123"));
            assert!(message.contains(event));
            assert!(message.contains("ONCE_PER_EVENT"));
            send(
                &mut write,
                json!({"jsonrpc":"2.0","id":prompt["id"],"result":answer}),
            )
            .await;

            let result = receive(&mut read).await;
            assert_eq!(result["id"], id, "{result}");
            assert_eq!(
                result["result"]["structuredContent"]["ok"], false,
                "{result}"
            );
            let error = &result["result"]["structuredContent"]["error"];
            assert_eq!(error["code"], "POLICY_DENIED", "{result}");
            assert!(
                error["message"].as_str().unwrap().contains(expected),
                "{result}"
            );
        }

        drop(write);
        drop(read);
        tokio::time::timeout(Duration::from_secs(10), server_task)
            .await
            .expect("legacy MCP service terminates")
            .unwrap();
    }
}
