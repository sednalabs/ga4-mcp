# Security Policy

## Supported Versions

Security fixes are accepted against the current `main` branch.

## Reporting a Vulnerability

Please report suspected vulnerabilities through a private maintainer channel
when available. If you must use a public issue, redact sensitive details first
and provide a minimal reproduction that does not expose credentials or private
deployment identifiers.

Do not include:

- bearer tokens, refresh tokens, client secrets, private keys, ADC files, or
  service-account JSON;
- GA account/property ids unless they are required for the report;
- local usernames, hostnames, IP addresses, absolute home paths, or internal
  deployment names.

## Project Security Posture

- The default Google scope is read-only:
  `https://www.googleapis.com/auth/analytics.readonly`.
- The default capability profile is `read_only`.
- GA mutation tools are out of scope unless explicitly approved by maintainers.
- Hosted or public multi-user deployments should use per-request Google tokens
  with `GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header`.
- Loopback developer services may use `request_header_or_config`, but that mode
  should not be exposed as an anonymous public surface.

See [docs/SECURITY_MODEL.md](docs/SECURITY_MODEL.md) for details.
