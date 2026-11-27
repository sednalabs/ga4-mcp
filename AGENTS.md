# AGENTS.md — ga4-mcp

## Scope

- Applies to this repository.

## Operating intent

- Keep GA4 integration modular and read-only.
- Keep the canonical public agent skill at `skills/ga4-mcp/SKILL.md`; keep portable Power manifests consistent with the tool contract.
- Preserve parity of core tool names with the Google Analytics MCP implementation where practical.
- Keep runtime fast for spawn-per-request workflows.
- Keep response envelopes in Contract V1 form (`ok/data/meta` or `ok/error/meta`) for all tools.

## Toolkit-first development

- Inspect current `sednalabs/mcp-toolkit-rs` capabilities and its published contracts before implementing common authentication, HTTP transport, session lifecycle, SQL policy, scratchpad, cursor, observability or testing machinery.
- Reuse suitable toolkit components via an immutable, validated revision. Upstream provider-neutral improvements first; consume their landed revisions in GA4 and keep provider-specific behaviour here.
- Treat toolkit upgrades as explicit compatibility work: qualify RMCP and dependency pins, regenerate the lockfile and schemas, and run full validation. Do not duplicate a generic subsystem to avoid migration effort or weaken safety gates.

## Architecture boundaries

- `main.rs`: process bootstrap and transport wiring only.
- `server.rs`: MCP protocol handler and tool router integration.
- `tools.rs`: tool argument contracts, validation, and response contracts.
- `ga_client.rs`: Google API adapter/authenticated HTTP logic.
- `config.rs`: CLI/env settings and validation.
- `error.rs`: shared domain error model.

## Safety

- Do not add write/mutate GA operations without explicit approval.
- Keep OAuth scope read-only by default.
- Fail closed on invalid property ids and malformed filter/report arguments.
- Respect capability profile gates (`read_only` default, `scratchpad` elevated) and fail closed on blocked tools.
- Do not require interactive elicitation for standard read-only flows; reserve elicitation for explicitly approved high-risk operations.
- Never log bearer tokens or raw credentials.

## Quality bar

- Prefer small, reversible diffs.
- Add tests for validation and negative paths when changing safety-sensitive code.
- Keep README and docs updated if tool contracts or env expectations change.
- Keep private research notes out of tracked public-release docs.
