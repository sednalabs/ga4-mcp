# Getting Started

This guide gets `ga4-mcp` running with a least-privilege Google identity and
the default read-only tool profile.

## Prerequisites

- Rust toolchain compatible with edition 2024.
- Google Cloud SDK (`gcloud`) when using Application Default Credentials (ADC).
- Access to the GA4 account or property you want to inspect.
- Google Analytics Admin API and Google Analytics Data API enabled in the
  Google Cloud project used for OAuth or quota.

## Choose an Auth Mode

All modes require the Google Analytics read-only scope:

```text
https://www.googleapis.com/auth/analytics.readonly
```

Use `request_header` for hosted or multi-user deployments. Each request carries
the Google access token that should be used upstream:

```bash
export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header
export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=authorization
```

Use `request_header_or_config` for a loopback developer/operator service. A
request token wins when present; otherwise the server falls back to ADC or the
configured refresh-token source:

```bash
ga4-mcp auth login --quota-project YOUR_PROJECT
ga4-mcp auth status --verify-token

export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header_or_config
export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER=authorization
```

The login command writes to a GA4-specific ADC file by default:
`<user-config>/ga4-mcp/gcloud/application_default_credentials.json`. Add
`--shared-adc` only when you intentionally want the conventional shared gcloud
ADC file.

On SSH or a headless host, use `ga4-mcp auth login --headless --quota-project
YOUR_PROJECT`. If Google rejects the Analytics scope or blocks the default
OAuth app, create a Desktop OAuth client and rerun with
`ga4-mcp auth login --quota-project YOUR_PROJECT --client-id-file /path/to/oauth-client.json`.

If verification says local ADC needs a quota project, run:

```bash
gcloud services enable analyticsadmin.googleapis.com analyticsdata.googleapis.com --project YOUR_PROJECT
ga4-mcp auth login --quota-project YOUR_PROJECT
ga4-mcp auth status --verify-token
```

Use `config` for deliberate service-owned automation:

```bash
export GOOGLE_APPLICATION_CREDENTIALS=/path/to/adc-or-service-account.json
export GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=config
```

For more detail, see [auth-modes.md](auth-modes.md).

## Run the Stdio Server

```bash
cargo run --release --bin ga4-mcp
```

The default capability profile is `read_only`, which exposes GA read/report
tools and blocks scratchpad tools.

## Run the HTTP Server

Loopback is the default safe shape:

```bash
GA4_MCP_BIND_ADDR=127.0.0.1:9420 \
cargo run --release --bin ga4-mcp-http
```

Non-loopback exposure requires an explicit network allowlist and TLS:

```bash
GA4_MCP_BIND_ADDR=0.0.0.0:9420 \
GA4_MCP_ALLOW_NON_LOOPBACK=1 \
GA4_MCP_ALLOWED_CIDRS=192.168.1.0/24,127.0.0.1/32,::1/128 \
GA4_MCP_ALLOWED_HOSTS=ga4-mcp.example.com,localhost,127.0.0.1 \
GA4_MCP_TLS_CERT_PATH=/etc/ga4-mcp/tls/fullchain.pem \
GA4_MCP_TLS_KEY_PATH=/etc/ga4-mcp/tls/privkey.pem \
cargo run --release --bin ga4-mcp-http
```

Enable inbound OAuth separately when the HTTP server is reachable outside a
trusted loopback or private boundary. See [SECURITY_MODEL.md](SECURITY_MODEL.md).

## First Calls

Start with low-cost discovery and validation:

1. `ga4_get_started` if the client exposes setup helper tools.
2. `ga4_auth_status` with `verify_token=true` if you need to inspect auth from
   inside MCP.
3. `get_account_summaries` to confirm the Google identity can see GA accounts.
4. `get_property_details` for the property you plan to query.
5. `check_report_compatibility` before running a report with new
   dimension/metric combinations.
6. `preview_report_request` to validate and normalize a report payload without
   calling GA.
7. `run_report` or `run_realtime_report` after the request shape is confirmed.

## Optional Scratchpad Profile

Scratchpad tools are disabled in the default `read_only` profile. Enable them
only when you want local DuckDB-backed analysis:

```bash
GOOGLE_ANALYTICS_MCP_CAPABILITY_PROFILE=scratchpad \
cargo run --release --bin ga4-mcp
```

Scratchpad tools still use GA read APIs upstream. They add local session state
for ingesting, querying, summarizing, and exporting evidence bundles.

## Local Verification

For code changes, the full developer sequence is:

```bash
cargo fmt --all
cargo check
cargo test
./scripts/sql_policy_toolkit_conformance.sh
cargo run -- --print-tool-schema > spec/tool_schema_snapshot.v1.json
```

Live Google tests are opt-in and require real credentials. Do not run them for
docs-only changes unless you are intentionally validating a live auth surface.
