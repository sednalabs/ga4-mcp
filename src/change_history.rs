//! Bounded GA Admin configuration audit requests and secret-safe response projection.

use chrono::DateTime;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::AnalyticsError;
use crate::ga_client::{AccountId, PropertyId};

#[derive(Deserialize, JsonSchema)]
pub(crate) struct ChangeHistoryArgs {
    /// The Google Analytics account id as integer or "accounts/<id>".
    pub account_id: AccountId,
    /// Optional child property filter, as an integer or resource name.
    #[serde(default)]
    pub property_id: Option<PropertyId>,
    /// Optional GA v1beta change-history resource types.
    #[serde(default)]
    pub resource_types: Option<Vec<String>>,
    /// Optional action types: CREATED, UPDATED or DELETED.
    #[serde(default)]
    pub actions: Option<Vec<String>>,
    /// Optional actor email filters (sensitive; never logged).
    #[serde(default)]
    pub actor_emails: Option<Vec<String>>,
    /// Optional inclusive RFC3339 start timestamp.
    #[serde(default)]
    pub earliest_change_time: Option<String>,
    /// Optional inclusive RFC3339 end timestamp.
    #[serde(default)]
    pub latest_change_time: Option<String>,
    /// Optional provider page size, between 1 and 200 (default 50).
    #[serde(default)]
    pub page_size: Option<u32>,
    /// Optional number of pages, between 1 and 10 (default 1).
    #[serde(default)]
    pub max_pages: Option<u32>,
    /// Optional provider continuation token from an earlier identical search.
    #[serde(default)]
    pub page_token: Option<String>,
    /// Include actor email addresses in results; defaults to false.
    #[serde(default)]
    pub include_actor_email: bool,
    /// Include allowlisted configuration snapshot fields; defaults to false.
    #[serde(default)]
    pub include_resource_snapshots: bool,
}

const RESOURCE_TYPES: &[&str] = &[
    "ACCOUNT",
    "PROPERTY",
    "FIREBASE_LINK",
    "GOOGLE_ADS_LINK",
    "GOOGLE_SIGNALS_SETTINGS",
    "CONVERSION_EVENT",
    "MEASUREMENT_PROTOCOL_SECRET",
    "CUSTOM_DIMENSION",
    "CUSTOM_METRIC",
    "DATA_RETENTION_SETTINGS",
    "DISPLAY_VIDEO_360_ADVERTISER_LINK",
    "DISPLAY_VIDEO_360_ADVERTISER_LINK_PROPOSAL",
    "DATA_STREAM",
    "ATTRIBUTION_SETTINGS",
];

fn validate_enum_filter(
    field: &'static str,
    values: &[String],
    allowed: &[&str],
) -> Result<(), AnalyticsError> {
    if values.len() > 32 || values.iter().any(|value| !allowed.contains(&value.as_str())) {
        return Err(AnalyticsError::invalid(
            field,
            "invalid or excessive enumeration filters",
        ));
    }
    Ok(())
}

impl ChangeHistoryArgs {
    pub(crate) fn validated_request(&self) -> Result<(String, Map<String, Value>), AnalyticsError> {
        let account = self.account_id.to_resource_name()?;
        let mut body = Map::new();
        if let Some(property) = &self.property_id {
            body.insert("property".to_string(), json!(property.to_resource_name()?));
        }
        if let Some(types) = &self.resource_types {
            validate_enum_filter("resource_types", types, RESOURCE_TYPES)?;
            if !types.is_empty() {
                body.insert("resourceType".to_string(), json!(types));
            }
        }
        if let Some(actions) = &self.actions {
            validate_enum_filter("actions", actions, &["CREATED", "UPDATED", "DELETED"])?;
            if !actions.is_empty() {
                body.insert("action".to_string(), json!(actions));
            }
        }
        if let Some(emails) = &self.actor_emails {
            if emails.len() > 25
                || emails.iter().any(|email| {
                    email.is_empty()
                        || email.len() > 254
                        || email.chars().any(|ch| ch.is_whitespace() || ch.is_control())
                        || !email.contains('@')
                })
            {
                return Err(AnalyticsError::invalid(
                    "actor_emails",
                    "invalid or excessive actor email filters",
                ));
            }
            if !emails.is_empty() {
                body.insert("actorEmail".to_string(), json!(emails));
            }
        }
        let parse_time = |field, value: &Option<String>| {
            value
                .as_ref()
                .map(|time| {
                    if time.len() > 64 {
                        return Err(AnalyticsError::invalid(field, "invalid RFC3339 timestamp"));
                    }
                    DateTime::parse_from_rfc3339(time)
                        .map_err(|_| AnalyticsError::invalid(field, "invalid RFC3339 timestamp"))
                })
                .transpose()
        };
        let earliest = parse_time("earliest_change_time", &self.earliest_change_time)?;
        let latest = parse_time("latest_change_time", &self.latest_change_time)?;
        if matches!((&earliest, &latest), (Some(start), Some(end)) if start > end) {
            return Err(AnalyticsError::invalid(
                "earliest_change_time",
                "start timestamp must not be after end timestamp",
            ));
        }
        if let Some(value) = &self.earliest_change_time {
            body.insert("earliestChangeTime".to_string(), json!(value));
        }
        if let Some(value) = &self.latest_change_time {
            body.insert("latestChangeTime".to_string(), json!(value));
        }
        if !(1..=200).contains(&self.page_size.unwrap_or(50)) {
            return Err(AnalyticsError::invalid(
                "page_size",
                "must be between 1 and 200",
            ));
        }
        if !(1..=10).contains(&self.max_pages.unwrap_or(1)) {
            return Err(AnalyticsError::invalid("max_pages", "must be between 1 and 10"));
        }
        if let Some(token) = &self.page_token {
            if token.trim().is_empty() || token.len() > 8192 || token.chars().any(char::is_control) {
                return Err(AnalyticsError::invalid("page_token", "invalid continuation token"));
            }
        }
        Ok((account, body))
    }
}

// Explicit field allowlists rather than denylisting known secrets: unknown
// future resource types and nested provider objects are never passed through.
fn scalar_fields(value: &Value, fields: &[&str]) -> Map<String, Value> {
    fields
        .iter()
        .filter_map(|key| {
            value.get(*key).and_then(|value| {
                (value.is_string() || value.is_boolean() || value.is_number())
                    .then(|| ((*key).to_string(), value.clone()))
            })
        })
        .collect()
}

fn safe_snapshot(snapshot: &Value) -> Value {
    let mut result = Map::new();
    for (resource, fields) in [
        ("account", &["name", "displayName", "regionCode", "gmpOrganization"][..]),
        ("property", &["name", "displayName", "timeZone", "currencyCode", "industryCategory", "serviceLevel", "propertyType", "parent", "account"][..]),
        ("dataRetentionSettings", &["name", "eventDataRetention", "userDataRetention", "resetUserDataOnNewActivity"][..]),
        ("conversionEvent", &["name", "eventName", "countingMethod", "custom", "deletable"][..]),
        ("customDimension", &["name", "parameterName", "displayName", "scope", "disallowAdsPersonalization"][..]),
        ("customMetric", &["name", "parameterName", "displayName", "scope", "measurementUnit"][..]),
        ("googleAdsLink", &["name", "customerId", "adsPersonalizationEnabled", "canManageClients"][..]),
        ("firebaseLink", &["name", "project"][..]),
        ("dataStream", &["name", "displayName", "type"][..]),
    ] {
        if let Some(value) = snapshot.get(resource) {
            let projected = scalar_fields(value, fields);
            if !projected.is_empty() {
                result.insert(resource.to_string(), Value::Object(projected));
            }
        }
    }
    Value::Object(result)
}

pub(crate) fn project_event(event: &Value, args: &ChangeHistoryArgs) -> Value {
    let mut result = scalar_fields(event, &["id", "changeTime", "actorType", "changesFiltered"]);
    if args.include_actor_email {
        result.extend(scalar_fields(event, &["userActorEmail"]));
    }
    let changes = event
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|change| {
            let mut projected = scalar_fields(change, &["resource", "action"]);
            if args.include_resource_snapshots {
                for key in ["resourceBeforeChange", "resourceAfterChange"] {
                    if let Some(value) = change.get(key) {
                        projected.insert(key.to_string(), safe_snapshot(value));
                    }
                }
            }
            Value::Object(projected)
        })
        .collect();
    result.insert("changes".to_string(), Value::Array(changes));
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: Value) -> ChangeHistoryArgs {
        serde_json::from_value(value).expect("valid argument shape")
    }

    #[test]
    fn request_normalizes_identifiers_and_provider_field_names() {
        let request = args(json!({
            "account_id": 123,
            "property_id": 456,
            "resource_types": ["PROPERTY"],
            "actions": ["UPDATED"],
            "actor_emails": ["user@example.com"],
            "earliest_change_time": "2026-11-01T00:00:00Z",
            "latest_change_time": "2026-11-02T00:00:00+11:00",
            "page_size": 100,
            "max_pages": 2
        }));
        let (account, body) = request.validated_request().expect("valid request");
        assert_eq!(account, "accounts/123");
        assert_eq!(
            Value::Object(body),
            json!({
                "property": "properties/456",
                "resourceType": ["PROPERTY"],
                "action": ["UPDATED"],
                "actorEmail": ["user@example.com"],
                "earliestChangeTime": "2026-11-01T00:00:00Z",
                "latestChangeTime": "2026-11-02T00:00:00+11:00"
            })
        );
    }

    #[test]
    fn invalid_inputs_fail_before_request() {
        for invalid in [
            json!({"account_id":"properties/1"}),
            json!({"account_id":1,"property_id":"accounts/2"}),
            json!({"account_id":1,"actions":["PATCH"]}),
            json!({"account_id":1,"resource_types":["UNKNOWN"]}),
            json!({"account_id":1,"actor_emails":["bad email"]}),
            json!({"account_id":1,"page_size":201}),
            json!({"account_id":1,"max_pages":11}),
            json!({"account_id":1,"page_token":" "}),
            json!({"account_id":1,"earliest_change_time":"invalid"}),
            json!({"account_id":1,"earliest_change_time":"2026-11-02T00:00:00Z","latest_change_time":"2026-11-01T00:00:00Z"}),
        ] {
            assert!(args(invalid).validated_request().is_err());
        }
    }

    #[test]
    fn secrets_and_unknown_fields_never_leave_projection() {
        let event = json!({
            "id": "1",
            "actorType": "USER",
            "userActorEmail": "person@example.com",
            "changes": [{
                "resource": "properties/1/measurementProtocolSecrets/2",
                "action": "UPDATED",
                "resourceBeforeChange": {
                    "measurementProtocolSecret": {"secretValue": "credential"},
                    "unknownResource": {"secret": "credential"}
                },
                "resourceAfterChange": {
                    "property": {"timeZone": "Australia/Melbourne", "secretValue": "credential"}
                }
            }]
        });
        let default = project_event(&event, &args(json!({"account_id":1})));
        assert!(default.get("userActorEmail").is_none());
        assert!(default["changes"][0].get("resourceAfterChange").is_none());
        let explicit = project_event(&event, &args(json!({
            "account_id":1,"include_actor_email":true,"include_resource_snapshots":true
        })));
        assert_eq!(explicit["userActorEmail"], "person@example.com");
        assert_eq!(explicit["changes"][0]["resourceBeforeChange"], json!({}));
        assert_eq!(
            explicit["changes"][0]["resourceAfterChange"],
            json!({"property":{"timeZone":"Australia/Melbourne"}})
        );
        assert!(!explicit.to_string().contains("credential"));
    }
}
