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

Scratchpad sessions are not yet isolated by authenticated principal. Use stdio
for scratchpad workflows, or a dedicated single-user loopback HTTP process with
`GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=config`, inbound auth disabled,
and `GA4_MCP_ALLOW_LOCAL_SINGLE_USER_SCRATCHPAD_HTTP=1`. Never share or proxy
that loopback scratchpad endpoint to other users. Other HTTP scratchpad
configurations fail at startup; the read-only profile is unaffected.

This restriction is containment, not a claim of per-principal isolation.

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

## Toolkit and MCP transport dependency custody

The runtime migration uses an immutable, previously validated toolkit revision
and the same reviewed RMCP patch as its root workspace. The HTTP service uses
the toolkit's bounded local session runtime, rather than creating an unbounded
raw session manager. Live discovery is capability-profile filtered; the CLI
schema snapshot remains a full developer catalogue. The RMCP response adapter
preserves Contract V1 complete results. The migration must pass fresh lockfile
resolution, dependency governance, legacy/current protocol and container tests
before production use. Scratchpad HTTP containment is unchanged.

## Shared scratchpad substrate

GA4 consumes the landed toolkit scratchpad revision `b2c2de4336cd1031e57bc63fcfdbfa17f54f7595`. Its shared
manager provides private per-generation database storage, quotas and optional
exact identity scopes. GA4 keeps the configured policy-runtime decision as a
second query gate and preserves its Contract V1 error taxonomy and existing
provider-specific column normalization. The old GA4 copy of the lifecycle
manager is removed. HTTP multi-user scratchpads remain disabled until the
request-bound MCP principal and independently verified Google account can be
bound to the scoped manager for every operation; the scoped-store integration
test is not evidence that hosted identity binding is complete.

### Time-bounded dependency maintenance decision

The migration adopts the existing toolkit decision for the identical locked
Chrono 0.4.45 informational unmaintained notice (RUSTSEC-2026-0543), not a
vulnerability waiver. Scope and expiry are enforced with the unchanged toolkit
validator and tests. The exception expires at 2026-12-18 00:00 UTC, with no
extension beyond the existing toolkit decision; package/version changes or
additional ignored advisories fail validation. Removal is tracked in Ops
w19008 and w19594. All vulnerability, license, source and yanked-package checks
remain enabled. No production hosted scratchpad enablement is implied.

## Opt-in stateless read-only HTTP

Set `GA4_MCP_STATELESS_HTTP=1`, capability profile `read_only`, and
`GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header`. The pinned rmcp
3.9.0 server then disables MCP transport sessions and sends JSON responses
for requests. Clients must send their own Google token on every API call.
If inbound MCP OAuth is enabled, use a distinct configured Google-token
header (for example `x-ga4-google-authorization`) rather than forwarding the
MCP bearer token to Google. There is no server credential fallback.

The default remains stateful HTTP. Stateless mode rejects scratchpad and other
non-read-only profiles at startup. It does not share DuckDB state, transport
sessions or OAuth authorisation state between replicas. Continue to enforce
native TLS, allowed hosts/CIDRs and inbound auth for shared deployments;
loopback must not be exposed through an untrusted reverse proxy. Replicas must
use consistent configuration and credential boundaries. Two-replica live
qualification is required before claiming production horizontal scaling.

## Elevated audit profile

The `audit` profile requires an explicit `analytics.edit` scope for the
configuration-history API. It does not authorize GA mutations or scratchpad
operations. Use a separate credential and deployment when practical. Live MCP
tool discovery and dispatch enforce the profile; the developer schema snapshot
remains the full registered catalogue. The audit tool does not return raw
provider snapshots: actor email is opt-in and scalar snapshot fields are
allowlisted. Treat even projected audit data as sensitive.

The audit profile requires the exact single configured edit scope. Google
still validates the effective request credential and account permissions.
Do not widen the deployment's grant to unrelated scopes for convenience.
Provider errors retain status-level diagnostics, not raw response bodies.

## BigQuery event-export boundary

The dedicated `bigquery` capability is currently local stdio only with configured credentials, official API origins, exact read-only scopes, operator-defined export project ID/number, billing project, region and byte limit. Before any dry run or execution, the same token must establish the configured GA property link and matching dataset identity/location. The only query templates are fixed aggregate SELECTs over daily tables; no arbitrary query or raw user identifiers. Provider scalar responses, pages and bytes are bounded. Execution is disabled by default and requires explicit caller opt-in plus operator enablement, a successful dry run and provider-enforced maximum bytes billed. A read-only credential cannot remotely cancel a job; timeout is best effort. Do not use these tools on exports with PII in custom event, medium or item labels. IAM and tenant-scoped credentials remain the operator's responsibility.

## Guarded configuration creations

The optional `mutation` profile permits three create-only operations: reporting
annotations, custom dimensions and key events. The default read-only, audit and
scratchpad profiles cannot discover or dispatch these writes. The mutation profile
requires the exact edit scope, config-only Google credentials, and the official
Google Admin API origin. HTTP startup rejects the profile. Use a dedicated trusted
local stdio client that presents approval requests to its human operator.

The server validates and displays the exact property, operation and provider
payload, then requires an explicit accepted client elicitation. MCP versions
before 2026-07-28 use the existing request/response elicitation mechanism;
newer versions use multi-round-trip requests and the toolkit's scoped, bounded,
120-second, single-use continuation store. Tool arguments cannot approve their
own mutation. Wrong payload, cancellation, replay, unsupported clients and missing
approval fail closed. The local trusted client is responsible for truthful human
approval; a client that fabricates approval cannot be trusted. No raw credential
is included in approval state. The creation client disables retry and redirect,
uses the same token for create and bounded readback, and reports an unconfirmed
readback without retrying creation. Inspect resources after ambiguous failures.
No deletion, patching, arbitrary Admin API paths or live write tests are provided.
