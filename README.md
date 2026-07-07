# ga4-mcp

Rust MCP server for Google Analytics 4 (GA4), built for low-latency agent workflows.

This project keeps parity with the core read/report intent of the official Google Analytics MCP server, then extends it with stricter contracts, policy gating, and an embedded DuckDB scratchpad workflow for analysis-heavy use cases.

## Documentation

- [Getting started](docs/GETTING_STARTED.md)
- [Security model](docs/SECURITY_MODEL.md)
- [Tool guide](docs/TOOL_GUIDE.md)
- [Auth modes](docs/auth-modes.md)
- [Payload Contract V1](docs/payload-contract-v1.md)

## Upstream Alignment

Upstream reference: <https://github.com/googleanalytics/google-analytics-mcp>

As of 2026-02-24, the upstream README describes a smaller core tool set (`get_account_summaries`, `get_property_details`, `list_google_ads_links`, `run_report`, `run_realtime_report`, `get_custom_dimensions_and_metrics`).

`ga4-mcp` keeps those core capabilities and adds:

- additional GA Admin/Data API coverage (pivot, batch, access reports, annotations, retention, sharing settings)
- local request preflight helpers (`preview_report_request`, `check_report_compatibility`)
- Contract V1-only envelopes (`ok/data/meta` and `ok/error/meta`)
- profile-gated DuckDB scratchpad tools (`read_only` vs `scratchpad`)
- streamable HTTP transport with host/IP/TLS guardrails and optional inbound OAuth resource-server mode

## Current Tool Surface

### Auth and setup helpers

- `ga4_get_started`
- `ga4_auth_status`
- `ga4_auth_login_command`

### Core GA read/report tools

- `get_account_summaries`
- `get_account_data_sharing_settings`
- `get_property_details`
- `get_property_data_retention_settings`
- `list_google_ads_links`
- `list_property_annotations`
- `get_custom_dimensions_and_metrics`
- `run_report`
- `run_realtime_report`
- `run_pivot_report`
- `batch_run_reports`
- `run_property_access_report`
- `run_account_access_report`

### Request preflight and compatibility tools

- `preview_report_request`
- `check_report_compatibility`

### Scratchpad tools (requires `scratchpad` profile)

- `scratchpad_open_session`
- `scratchpad_close_session`
- `scratchpad_list_sessions`
- `scratchpad_get_runtime_limits`
- `scratchpad_set_runtime_limits`
- `scratchpad_list_tables`
- `scratchpad_drop_table`
- `scratchpad_ingest_report`
- `scratchpad_ingest_realtime_report`
- `scratchpad_query`
- `scratchpad_describe_table`
- `scratchpad_summarize_table`
- `scratchpad_release_regression_report`
- `scratchpad_landing_param_shift_report`
- `scratchpad_export_evidence_bundle`

## Quick Start

### 1) Configure Google authentication

Straight answer:

- Local user-level service on your own machine: log in once with Google ADC
  and set a quota project. This is the benchmark happy path.
- Hosted/public/multi-user service: use `request_header`; every client request
  must carry that user's Google bearer token.
- Non-interactive automation: use `config` with ADC, a service account, or an
  explicit refresh-token config.

Required Google scope for all modes:

- `https://www.googleapis.com/auth/analytics.readonly`

#### Local user setup: login once with ADC

Use this for a loopback user service on your own machine.

```bash
ga4-mcp auth login --quota-project YOUR_PROJECT
ga4-mcp auth status --verify-token

export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header_or_config
export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=authorization
```

By default `ga4-mcp auth login` writes Application Default Credentials to a
GA4-specific gcloud config directory:
`<user-config>/ga4-mcp/gcloud/application_default_credentials.json`. Use
`--shared-adc` only when you intentionally want the conventional shared gcloud
ADC file.

`YOUR_PROJECT` should be a Google Cloud project where both
`analyticsadmin.googleapis.com` and `analyticsdata.googleapis.com` are enabled
and where your Google account is allowed to use the project for quota.

Result: if a client sends `Authorization: Bearer <google_access_token>`, the
server uses that token. If the client sends no token, the server uses the local
GA4-specific ADC identity from the one-time login. Conventional shared ADC is
used only when `GOOGLE_ANALYTICS_MCP_SHARED_ADC=true` or the server starts with
`--shared-adc`.

If Google blocks the default `gcloud` OAuth client for Analytics scopes, create
a Google OAuth desktop client, download the JSON locally, and run:

```bash
ga4-mcp auth login --quota-project YOUR_PROJECT --client-id-file /path/to/oauth-client.json
```

Headless or SSH login:

```bash
ga4-mcp auth login --headless --quota-project YOUR_PROJECT
```

The headless flow asks `gcloud` not to launch a browser. Complete the printed
Google consent flow from a trusted machine and keep the resulting ADC file
private.

Useful auth commands:

```bash
ga4-mcp auth command --headless --quota-project YOUR_PROJECT
ga4-mcp auth doctor --verify-token
ga4-mcp auth status --json --verify-token
```

If verification says local ADC needs a quota project, enable the Analytics
Admin and Data APIs on a Google Cloud project, then rerun login with
`--quota-project` or run `ga4-mcp auth command --quota-project YOUR_PROJECT`
and execute the printed quota-project command:

```bash
gcloud services enable analyticsadmin.googleapis.com analyticsdata.googleapis.com --project YOUR_PROJECT
ga4-mcp auth login --quota-project YOUR_PROJECT
ga4-mcp auth status --verify-token
```

#### Server-side credential mode

```bash
ga4-mcp auth login --quota-project YOUR_PROJECT
ga4-mcp auth status --verify-token
```

Or point directly at a credentials file:

```bash
export GOOGLE_APPLICATION_CREDENTIALS=/path/to/adc-or-service-account.json
```

Optional server-side refresh-token mode:

```bash
export GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON=/path/to/client_secret.json
export GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN=your_refresh_token
```

Set this explicitly when you want to force server-held credentials only:

```bash
export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=config
```

#### Hosted/public per-user OAuth mode

```bash
export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header
export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=authorization
```

In this mode, each MCP request must carry a Google access token (usually
`Authorization: Bearer <token>`), acquired by the client's interactive OAuth flow.

#### Hybrid local mode

```bash
export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header_or_config
export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=authorization
```

Use this for loopback user-level services when you want painless local ADC
fallback without breaking clients that already send per-request Google tokens.
Do not expose this mode publicly unless inbound MCP auth is enabled and
configured deliberately.

Google OAuth endpoints for client configuration:

- Authorization URL: `https://accounts.google.com/o/oauth2/v2/auth`
- Token URL: `https://oauth2.googleapis.com/token`
- Scope: `https://www.googleapis.com/auth/analytics.readonly`

Notes:

- Use a pre-registered Google OAuth client (web/desktop as appropriate for the MCP client).
- Google does not expose a general dynamic client registration endpoint for this integration pattern.

### 2) Run stdio MCP server

```bash
cargo run --release --bin ga4-mcp
```

Inside MCP, call `ga4_get_started` first after install. Use
`ga4_auth_status` to inspect credentials, and `ga4_auth_login_command` when an
MCP client needs a copyable `gcloud` command without running the CLI wrapper.
That tool also targets the GA4-specific ADC file by default; pass
`shared_adc=true` only for the conventional shared ADC file. To make the running server use that
shared ADC file, also set `GOOGLE_ANALYTICS_MCP_SHARED_ADC=true` or start the binary with
`--shared-adc`.

### 3) Optional: run streamable HTTP server

Loopback-only (default-safe):

```bash
GA4_MCP_BIND_ADDR=127.0.0.1:9420 \
cargo run --release --bin ga4-mcp-http
```

Non-loopback requires explicit opt-in plus TLS:

```bash
GA4_MCP_BIND_ADDR=0.0.0.0:9420 \
GA4_MCP_ALLOW_NON_LOOPBACK=1 \
GA4_MCP_ALLOWED_CIDRS=192.168.1.0/24,127.0.0.1/32,::1/128 \
GA4_MCP_ALLOWED_HOSTS=ga4-mcp.example.com,localhost,127.0.0.1 \
GA4_MCP_TLS_CERT_PATH=/etc/ga4-mcp/tls/fullchain.pem \
GA4_MCP_TLS_KEY_PATH=/etc/ga4-mcp/tls/privkey.pem \
cargo run --release --bin ga4-mcp-http
```

## Capability Profiles

Set `GOOGLE_ANALYTICS_MCP_CAPABILITY_PROFILE` (or `--capability-profile`) to control tool authorization:

- `read_only` (default): GA read/report tools only; all `scratchpad_*` tools are blocked.
- `scratchpad`: full GA + scratchpad surface.

Example enabling scratchpad:

```bash
GOOGLE_ANALYTICS_MCP_CAPABILITY_PROFILE=scratchpad \
cargo run --release --bin ga4-mcp
```

If you enable inbound OAuth (`GA4_MCP_AUTH_ENABLED=1`), auth config is fail-closed:

- `GA4_MCP_AUTH_MODE=jwks` requires `GA4_MCP_AUTH_JWKS_URL`, `GA4_MCP_AUTH_ISSUER`, and `GA4_MCP_AUTH_AUDIENCE`.
- `GA4_MCP_AUTH_MODE=introspection` requires `GA4_MCP_AUTH_INTROSPECTION_URL`, `GA4_MCP_AUTH_INTROSPECTION_CLIENT_ID`, `GA4_MCP_AUTH_INTROSPECTION_CLIENT_SECRET`, `GA4_MCP_AUTH_ISSUER`, and `GA4_MCP_AUTH_AUDIENCE`.
- `GA4_MCP_AUTH_MODE=delegation` requires `GA4_MCP_AUTH_DELEGATION_SECRET`, `GA4_MCP_AUTH_DELEGATION_ISSUER`, and `GA4_MCP_AUTH_DELEGATION_AUDIENCE`.
- `GA4_MCP_AUTH_REQUIRED_SCOPES` must include at least one scope.
- `GA4_MCP_AUTH_ACTOR_CLAIM` must not be empty.
- `GA4_MCP_AUTH_INTROSPECTION_URL` is rejected in `delegation` mode.

## Scratchpad Workflow (Recommended)

1. Open session with `scratchpad_open_session`.
2. Validate payload with `preview_report_request` before pulling data.
3. Optionally preflight metrics/dimensions with `check_report_compatibility`.
4. Ingest rows with `scratchpad_ingest_report` or `scratchpad_ingest_realtime_report`.
5. Analyze with `scratchpad_query`, `scratchpad_describe_table`, `scratchpad_summarize_table`.
6. Clean up slot/quota with `scratchpad_drop_table` or close session.

Notes:

- Ingest tools auto-page upstream GA responses.
- `max_rows` on ingest controls upstream page size; `limit` caps total ingested rows.
- `append=true` reuses an existing table without consuming another table slot, but schema/type compatibility is enforced.
- `scratchpad_drop_table` reclaims both table slot and row quota immediately.

Detailed runbook: [`docs/scratchpad-operator-guide.md`](docs/scratchpad-operator-guide.md)

## Report Input Semantics

- `date_ranges` must be an array of objects with non-empty `start_date`/`startDate` and `end_date`/`endDate`.
- `dimension_filter` accepts:
  - GA `FilterExpression` objects
  - JSON-object strings
  - shorthand expressions (`field==value`)
- `metric_filter` accepts GA `FilterExpression` objects or JSON-object strings.
- Shorthand values containing spaces must be quoted, for example:
  - `sessionDefaultChannelGroup=="Paid Other"`

Invalid shorthand is rejected before any upstream API call.

## Contract and Response Shape

This server is Contract V1-only:

- success: `ok=true` with `data` + `meta`
- error: `ok=false` with `error` + `meta`
- no legacy/compat envelope mode

Contract docs:

- [`docs/payload-contract-v1.md`](docs/payload-contract-v1.md)
- [`spec/payload_contract_v1.schema.json`](spec/payload_contract_v1.schema.json)

## Configuration Reference

All settings are available as CLI flags and env vars.

### Core GA/runtime

- `--analytics-scope` / `GOOGLE_ANALYTICS_MCP_SCOPE`
- `--admin-base-url` / `GOOGLE_ANALYTICS_MCP_ADMIN_BASE_URL`
- `--data-base-url` / `GOOGLE_ANALYTICS_MCP_DATA_BASE_URL`
- `--http-timeout-ms` / `GOOGLE_ANALYTICS_MCP_HTTP_TIMEOUT_MS`
- `--max-page-size` / `GOOGLE_ANALYTICS_MCP_MAX_PAGE_SIZE`
- `--max-pages` / `GOOGLE_ANALYTICS_MCP_MAX_PAGES`
- `--user-agent` / `GOOGLE_ANALYTICS_MCP_USER_AGENT`
- `--oauth-client-secret-json` / `GOOGLE_ANALYTICS_MCP_OAUTH_CLIENT_SECRET_JSON`
- `--oauth-refresh-token` / `GOOGLE_ANALYTICS_MCP_OAUTH_REFRESH_TOKEN`
- `--upstream-token-source` / `GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE`
- `--upstream-token-header` / `GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER`
- `--quota-project` / `GOOGLE_ANALYTICS_MCP_QUOTA_PROJECT`

### Scratchpad controls

- `--scratchpad-session-ttl-secs` / `GOOGLE_ANALYTICS_MCP_SCRATCHPAD_SESSION_TTL_SECS`
- `--scratchpad-max-sessions` / `GOOGLE_ANALYTICS_MCP_SCRATCHPAD_MAX_SESSIONS`
- `--scratchpad-max-tables-per-session` / `GOOGLE_ANALYTICS_MCP_SCRATCHPAD_MAX_TABLES_PER_SESSION`
- `--scratchpad-max-rows-per-session` / `GOOGLE_ANALYTICS_MCP_SCRATCHPAD_MAX_ROWS_PER_SESSION`
- `--scratchpad-max-memory-mb` / `GOOGLE_ANALYTICS_MCP_SCRATCHPAD_MAX_MEMORY_MB`
- `--scratchpad-query-timeout-ms` / `GOOGLE_ANALYTICS_MCP_SCRATCHPAD_QUERY_TIMEOUT_MS`
- `--scratchpad-max-sql-bytes` / `GOOGLE_ANALYTICS_MCP_SCRATCHPAD_MAX_SQL_BYTES`

### Capability and schema output

- `--capability-profile` / `GOOGLE_ANALYTICS_MCP_CAPABILITY_PROFILE`
- `--print-tools`
- `--print-tool-schema`

### Streamable HTTP and inbound auth mode

- `GA4_MCP_BIND_ADDR`
- `GA4_MCP_ALLOW_NON_LOOPBACK`
- `GA4_MCP_ALLOWED_HOSTS`
- `GA4_MCP_ALLOWED_CIDRS`
- `GA4_MCP_TLS_CERT_PATH`
- `GA4_MCP_TLS_KEY_PATH`
- `GA4_MCP_AUTH_ENABLED`
- `GA4_MCP_AUTH_MODE` (`jwks` | `introspection` | `delegation`)
- `GA4_MCP_AUTH_REALM`
- `GA4_MCP_AUTH_RESOURCE_URL`
- `GA4_MCP_AUTH_ISSUER`
- `GA4_MCP_AUTH_AUDIENCE`
- `GA4_MCP_AUTH_JWKS_URL`
- `GA4_MCP_AUTH_REQUIRED_SCOPES`
- `GA4_MCP_AUTH_SCOPES_SUPPORTED`
- `GA4_MCP_AUTH_ALLOWED_CLIENT_IDS`
- `GA4_MCP_AUTH_ACTOR_CLAIM`
- `GA4_MCP_AUTH_INTROSPECTION_URL`
- `GA4_MCP_AUTH_INTROSPECTION_CLIENT_ID`
- `GA4_MCP_AUTH_INTROSPECTION_CLIENT_SECRET`
- `GA4_MCP_AUTH_INTROSPECTION_AUTH_METHOD`
- `GA4_MCP_AUTH_INTROSPECTION_CACHE_TTL_S`
- `GA4_MCP_AUTH_INTROSPECTION_FORCE`
- `GA4_MCP_AUTH_DELEGATION_SECRET`
- `GA4_MCP_AUTH_DELEGATION_ISSUER`
- `GA4_MCP_AUTH_DELEGATION_AUDIENCE`
- `GA4_MCP_AUTH_JTI_TTL_S`
- `GA4_MCP_AUTH_JTI_CACHE_SIZE`
- `GA4_MCP_AUTH_JTI_ENFORCE_BEARER`
- `GA4_MCP_AUTH_CLOCK_SKEW_S`
- `GA4_MCP_AUTH_STRICT_OAUTH`

## Security Posture

- Read-only GA scope by default.
- No GA mutation/write tools.
- Input validation fails closed on malformed ids/arguments.
- Capability profile gates scratchpad access.
- Non-loopback HTTP requires explicit allow + TLS.
- When `GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=config`, auth headers on `/mcp` are rejected if inbound auth is off.
- When `GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header` (or `request_header_or_config`), request tokens are accepted and used per call.
- `request_header_or_config` is the local convenience mode: request token first, ADC/OAuth-refresh fallback second.
- `/health` is not auto-whitelisted by the auth surface; when inbound auth is enabled, unauthenticated health checks are denied unless you front the route separately.
- Inbound OAuth verification (when enabled) is for MCP access control. Upstream GA token source is independently controlled by `GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE`.
- For non-loopback exposure with auth enabled, strict OAuth parsing is required and configured auth URLs must use `https://`.

## Developer Verification

```bash
cargo fmt --all
cargo check
cargo test
./scripts/sql_policy_toolkit_conformance.sh
cargo run -- --print-tools
cargo run -- --print-tool-schema > spec/tool_schema_snapshot.v1.json
```

### Live smoke verification (production-facing)

Live smoke tests are opt-in and validate real network/auth flows.

1. HTTP auth surface smoke (PRM + unauthenticated `/mcp` behavior, plus `/health` behavior based on whether inbound auth is enabled):

```bash
GA4_LIVE_HTTP_SMOKE=1 \
GA4_LIVE_HTTP_BASE_URL=http://127.0.0.1:9420 \
cargo test --test live_http_auth_surface_smoke -- --nocapture
```

2. Google auth + GA API smoke (account/property/report reads):

```bash
GA4_LIVE_SMOKE=1 \
GA4_LIVE_SMOKE_PROPERTY_ID=properties/<YOUR_PROPERTY_ID> \
GOOGLE_ANALYTICS_MCP_QUOTA_PROJECT=<YOUR_GCP_PROJECT_ID> \
cargo test --test live_google_auth_smoke -- --nocapture
```

Build-helper presets:

- `ga4-mcp.sql-policy-toolkit-conformance`
- `ga4-mcp.build-release-restart-http`
- `ga4-mcp.live-http-auth-smoke`
- `ga4-mcp.live-google-auth-smoke`

## Related Docs

- [`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md)
- [`docs/SECURITY_MODEL.md`](docs/SECURITY_MODEL.md)
- [`docs/TOOL_GUIDE.md`](docs/TOOL_GUIDE.md)
- [`docs/auth-modes.md`](docs/auth-modes.md)
- [`docs/decision-0001-v1-contract-and-duckdb.md`](docs/decision-0001-v1-contract-and-duckdb.md)
- [`docs/payload-contract-v1.md`](docs/payload-contract-v1.md)
- [`docs/scratchpad-operator-guide.md`](docs/scratchpad-operator-guide.md)
- [`docs/observability-events.md`](docs/observability-events.md)
- [`docs/release-readiness-v1.md`](docs/release-readiness-v1.md)
- [`docs/sql-policy-contract.md`](docs/sql-policy-contract.md)
- [`docs/dependency-governance.md`](docs/dependency-governance.md)
