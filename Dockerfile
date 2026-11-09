# Build native GA4 MCP binaries without carrying toolchains or source into the runtime.
FROM rust:1-bookworm AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      cmake clang libclang-dev pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock README.md LICENSE ./
COPY src/ ./src/
COPY spec/ ./spec/

# DuckDB's bundled C++ build is memory intensive on small CI runners.
ENV CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 CMAKE_BUILD_PARALLEL_LEVEL=2
RUN cargo build --locked --release --bins

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      ca-certificates libgcc-s1 libstdc++6 libgomp1 libatomic1 libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 ga4-mcp \
    && useradd --uid 10001 --gid ga4-mcp --create-home --shell /usr/sbin/nologin ga4-mcp \
    && mkdir -p /home/ga4-mcp/.config \
    && chown -R ga4-mcp:ga4-mcp /home/ga4-mcp

COPY --from=build /build/target/release/ga4-mcp /usr/local/bin/ga4-mcp
COPY --from=build /build/target/release/ga4-mcp-http /usr/local/bin/ga4-mcp-http

ENV HOME=/home/ga4-mcp XDG_CONFIG_HOME=/home/ga4-mcp/.config
WORKDIR /home/ga4-mcp
USER 10001:10001

# Stdio MCP is the default. The HTTP executable is available explicitly.
ENTRYPOINT ["/usr/local/bin/ga4-mcp"]
