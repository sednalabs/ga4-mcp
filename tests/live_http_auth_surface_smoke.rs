use serde_json::Value;

const ENABLE_ENV: &str = "GA4_LIVE_HTTP_SMOKE";
const BASE_URL_ENV: &str = "GA4_LIVE_HTTP_BASE_URL";
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:9420";

fn live_smoke_enabled() -> bool {
    std::env::var(ENABLE_ENV)
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn base_url() -> String {
    std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

#[tokio::test]
async fn live_http_surface_reports_health_and_auth_metadata() {
    if !live_smoke_enabled() {
        eprintln!("skipping live smoke; set {ENABLE_ENV}=1 to enable");
        return;
    }

    let client = reqwest::Client::builder()
        .build()
        .expect("http client should build");
    let base = base_url();

    let unauth_mcp = client
        .get(format!("{base}/mcp"))
        .send()
        .await
        .expect("mcp unauth request should succeed");
    let auth_enabled = unauth_mcp.status() == reqwest::StatusCode::UNAUTHORIZED;

    let health = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("health request should succeed");
    if auth_enabled {
        assert_eq!(
            health.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "/health should require auth when auth surface is enabled"
        );
    } else {
        assert_eq!(
            health.status(),
            reqwest::StatusCode::OK,
            "/health should return 200 when auth surface is disabled"
        );
        let health_body: Value = health.json().await.expect("health payload should be JSON");
        assert_eq!(
            health_body.get("status"),
            Some(&Value::String("ok".to_string()))
        );
        assert_eq!(
            health_body.get("transport"),
            Some(&Value::String("streamable_http".to_string()))
        );
    }

    let prm = client
        .get(format!("{base}/.well-known/oauth-protected-resource/mcp"))
        .send()
        .await
        .expect("PRM request should succeed");
    assert_eq!(
        prm.status(),
        reqwest::StatusCode::OK,
        "PRM endpoint should return 200"
    );
    let prm_body: Value = prm.json().await.expect("PRM payload should be JSON");
    assert!(
        prm_body.get("authorization_servers").is_some(),
        "PRM payload should advertise authorization servers"
    );
    assert!(
        prm_body.get("resource").and_then(Value::as_str).is_some(),
        "PRM payload should advertise protected resource URI"
    );

    if auth_enabled {
        assert_eq!(
            unauth_mcp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "unauthenticated /mcp request should return 401 when auth is enabled"
        );
    } else {
        assert_ne!(
            unauth_mcp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "unauthenticated /mcp should not be 401 when auth is disabled"
        );
    }
}
