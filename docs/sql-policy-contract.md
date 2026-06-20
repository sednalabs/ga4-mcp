# SQL Policy Contract and Conformance

This document defines SQL-policy alignment for `ga4-mcp`.

## Authority model

Canonical SQL restricted-policy semantics are defined in `mcp-policy-kernel`:

- `spec/sql_restricted_policy_contract.source.json`
- `spec/generated/sql_restricted_policy_contract.v1.json`
- `vectors/sql_restricted_policy.json`

`mcp-toolkit-policy-core` is a consumer implementation of that canonical policy,
and `ga4-mcp` routes the canonical allow/deny decision through the shared
`mcp-toolkit-policy-runtime` restricted SQL authority. Runtime decisions carry
the authority provenance fields `decision_source`, `runtime_mode`, and
`policy_contract_version`.

`ga4-mcp` adds DuckDB-specific scratchpad overlays (for example
`DESCRIBE`/`SUMMARIZE` allowance and external scan/read deny rules) on top of
the canonical restricted SQL authority.

GA4-specific size limits run before authority evaluation. DuckDB-specific
overlays run after the canonical decision so external scan/read functions,
extension keywords, and GA4-only `DESCRIBE`/`SUMMARIZE` allowances remain local
to the scratchpad boundary.

## Toolkit conformance gate

Run from the repository root:

```bash
./scripts/sql_policy_toolkit_conformance.sh
```

This command validates `mcp-toolkit-policy-core` SQL classifier behavior against
canonical kernel SQL vectors and writes:

- `.tmp/sql_policy_conformance/sql_policy_core_vs_kernel_report.json`

## GA4 overlay conformance gate

Run targeted scratchpad safety contract checks:

```bash
cargo test --test contract_safety_conformance sql_safety_rejects_duckdb_extension_and_external_scan -- --exact
```

This confirms DuckDB overlay restrictions remain active and stable while
preserving kernel-aligned baseline behavior.

## Recommended release sequence

1. `./scripts/sql_policy_toolkit_conformance.sh`
2. `cargo test --test contract_safety_conformance`
3. full `cargo test`

Treat any mismatch in toolkit conformance as a policy-core regression unless
kernel vectors/contracts were intentionally updated first.
