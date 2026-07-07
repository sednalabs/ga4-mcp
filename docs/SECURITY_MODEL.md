# Security Model

`ga4-mcp` is designed as a read-first Google Analytics MCP server. The
default posture is conservative: read-only Google scopes, fail-closed input
validation, no GA mutation tools, and no standard elicitation requirement for
ordinary read-only flows.

## Trust Boundaries

There are two separate auth decisions:

- Inbound MCP auth controls who may call the MCP server.
- Upstream Google auth controls which Google identity is used for GA4 API calls.

Keep these decisions separate when deploying. Inbound OAuth does not grant GA4
access by itself, and a valid Google token does not automatically authorize a
client to reach a public MCP endpoint.

## Least-Privilege Google Access

Use the narrow GA4 read scope whenever possible:

```text
https://www.googleapis.com/auth/analytics.readonly
```

The optional `cloud-platform` scope is commonly needed by `gcloud` ADC and
some quota-project flows, but the direct `ga4-mcp auth login --client-id-file
...` browser flow requests only the configured Analytics scope. The MCP tool
surface itself is read/report oriented.

## Upstream Token Sources

- `request_header`: preferred for hosted or public multi-user services. Each
  request supplies the user's Google access token.
- `request_header_or_config`: intended for loopback developer/operator services
  and migrations. Request tokens win; GA4-specific local config is fallback.
- `config`: intended for deliberate service-owned automation using ADC, a
  service account, or configured OAuth refresh-token settings.

Do not expose `request_header_or_config` as an anonymous public surface. A
missing request token could silently fall back to a server-held Google identity.
Local browser login stores a GA4-specific authorized-user ADC file under
`<user-config>/ga4-mcp/gcloud` by default. With `--client-id-file` and no
`--shared-adc`, `ga4-mcp` uses its own browser OAuth helper and does not use
gcloud's bundled OAuth app. Use `--shared-adc` and
`GOOGLE_ANALYTICS_MCP_SHARED_ADC=true` only when a deployment intentionally
uses conventional shared gcloud ADC.

## HTTP Exposure

Loopback HTTP is the safe default. Non-loopback binds require explicit opt-in,
allowed hosts, allowed CIDRs, and TLS configuration. When inbound OAuth is
enabled, strict OAuth parsing is required for non-loopback exposure and auth
metadata must use HTTPS issuer/JWKS/introspection URLs.

`/health` is not automatically public. If inbound auth is enabled,
unauthenticated health checks are denied unless a separate proxy or deployment
layer handles them.

## Capability Profiles

- `read_only` is the default and exposes GA read/report tools only.
- `scratchpad` adds local DuckDB scratchpad tools for analysis workflows.

Scratchpad tools do not add GA write capabilities. They do add local state, so
operators should treat scratchpad session contents and exported evidence bundles
as sensitive analytics data.

## Validation and Failure Behavior

- Property and account identifiers are validated before use.
- Malformed report filters and report arguments fail before upstream API calls.
- Scratchpad SQL is restricted by the SQL policy contract.
- Policy denials and validation failures return Contract V1 error envelopes.
- Secrets and bearer tokens must not be logged, committed, or included in issue
  reports.

## Redaction Guidance

Before sharing logs or public issues, redact:

- bearer tokens, refresh tokens, client secrets, private keys, and credential
  file contents;
- local usernames, absolute home paths, hostnames, IP addresses, and deployment
  identifiers;
- GA account/property ids unless they are intentionally part of the report.

Keep OAuth client JSON files and ADC files outside the repository.

## Vulnerability Reports

See [../SECURITY.md](../SECURITY.md) for how to report suspected security
issues.
