# Toolkit and RMCP migration

The migration candidate pins the currently landed toolkit revision `6720c05f66876ddaa2e3c1c12f464e1e2b8b9f20` and its RMCP 3.9.0 transport patch `31e83f4f752001d2afc386254a4d17b5eab5f419`, rather than inventing downstream replacements for shared authentication, bounded HTTP sessions, or protocol adapters.

This initial migration keeps GA4's existing provider and scratchpad behaviour intact. It uses the toolkit's bounded local Streamable HTTP service and adapts the RMCP complete-response boundary while retaining Contract V1 payloads. The subsequent scratchpad adoption must consume the landed shared toolkit implementation and preserve all analytical features and policy checks. Keep shared HTTP scratchpads contained until end-to-end verified identity and provider binding are proven.

The draft migration uses a temporary, read-only hosted workflow to generate the lockfile because the current editing environment has no Rust toolchain or network access. Commit the generated lockfile, remove the temporary workflow, and verify locked tests, schema inventory, dependency governance, coverage, CodeQL, container smoke and live credential-free transport contracts before merging. Do not suppress existing advisories merely to obtain a green check. Review any exact maintenance exception separately with a bounded expiry and scope.
