# Contributing

Thanks for improving `ga4-mcp`. This repository is intended to stay small,
auditable, and read-oriented.

## Development Principles

- Preserve the default read-only GA4 posture.
- Do not add GA write or mutation tools without explicit maintainer approval.
- Keep OAuth scopes least-privilege; prefer
  `https://www.googleapis.com/auth/analytics.readonly`.
- Keep response envelopes in Contract V1 form: `ok/data/meta` or
  `ok/error/meta`.
- Match existing Rust module boundaries and naming conventions.
- Do not commit tokens, OAuth client secrets, ADC files, private keys, hostnames,
  local usernames, or deployment-specific runbooks.

## Local Checks

For behavior changes, run the narrowest relevant subset first, then the full
sequence when preparing a release:

```bash
cargo fmt --all
cargo check
cargo test
./scripts/sql_policy_toolkit_conformance.sh
cargo run -- --print-tool-schema > spec/tool_schema_snapshot.v1.json
```

Docs-only changes should at least pass:

```bash
git diff --check
```

Live Google tests are opt-in. Run them only when intentionally validating a real
credential or HTTP auth deployment surface.

## Documentation

Update public docs when changing tool names, argument semantics, auth settings,
capability profiles, or security behavior. Keep deployment-specific notes in
operator-local documentation rather than tracked public docs.

## Pull Requests

Keep pull requests focused and reviewable. Include the validation commands you
ran and note any checks intentionally skipped.
