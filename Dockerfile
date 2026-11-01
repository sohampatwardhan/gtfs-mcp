# Multi-arch image. The release workflow publishes
# ghcr.io/sohampatwardhan/gtfs-mcp for linux/amd64 and linux/arm64.
# A plain `docker build` produces the architecture of the machine it runs on.

FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
COPY src ./src
COPY tests/fixtures ./tests/fixtures
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home gtfs
COPY --from=build /src/target/release/gtfs-mcp /usr/local/bin/gtfs-mcp
LABEL org.opencontainers.image.source="https://github.com/sohampatwardhan/gtfs-mcp" \
      org.opencontainers.image.title="gtfs-mcp" \
      org.opencontainers.image.description="MCP server for GTFS static schedules and optional GTFS-Realtime feeds" \
      org.opencontainers.image.licenses="MIT"
USER 10001
EXPOSE 8091
ENTRYPOINT ["gtfs-mcp"]
CMD ["http", "--bind", "0.0.0.0:8091"]
