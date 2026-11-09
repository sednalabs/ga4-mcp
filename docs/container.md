# Container build and runtime

This image is the bounded Docker build for Ops `w2089`. It builds the stdio
`ga4-mcp` and optional `ga4-mcp-http` executables in a Rust builder, then
copies only those binaries to a minimal Debian runtime. The runtime uses UID/GID
`10001:10001`, has system CA roots, and contains no toolchain.

```sh
docker build -t ga4-mcp-rs:hardening .
docker run --rm \
  -e GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header \
  ga4-mcp-rs:hardening --print-tools
```

The tool-list smoke does not need Google credentials or make a Google Analytics
API call. To use stdio MCP, keep stdin attached (`docker run --rm -i ...`)
and configure Google authentication according to `docs/auth-modes.md`.
The default capability profile remains `read_only`; scratchpad access must
be explicitly enabled. The HTTP executable is also present at
`/usr/local/bin/ga4-mcp-http`, but exposing it beyond loopback still requires
the repository's explicit host/network, TLS and authentication controls.
This leaf does not provide or change the separate Compose deployment profile
owned by `w2090`.

For a more restrictive local smoke, run with:

```sh
docker run --rm --read-only --cap-drop=ALL \
  --security-opt=no-new-privileges \
  --tmpfs /tmp:rw,nosuid,nodev,size=64m \
  -e GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE=request_header \
  ga4-mcp-rs:hardening --print-tools
```

**Credentials:** Never put tokens, ADC JSON, refresh tokens or private keys in
the Dockerfile, image build arguments or image layers. The allowlisted
`.dockerignore` admits only the Rust manifests, source, public spec and
licence/readme. For actual authenticated use, provide environment-specific
credentials at *runtime* through a read-only secret mount or appropriately
scoped secret manager. For example, mount a service-account JSON file under
`/run/secrets/google.json` and set `GOOGLE_APPLICATION_CREDENTIALS` to that
path; ensure UID 10001 can read it without making the file world-readable.
Use a writable temporary mount for any DuckDB operations that spill to disk.
Do not bake user-level Google ADC files into a distributable image.

The focused `container-smoke` GitHub workflow checks build, presence of
both binaries, non-root runtime, and unauthenticated tool enumeration on the
exact PR candidate. Successful CI is required before acceptance; a source
review alone is not evidence that the image actually builds.
