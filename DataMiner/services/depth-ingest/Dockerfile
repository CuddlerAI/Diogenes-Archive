# syntax=docker/dockerfile:1
# Build context: DataMiner repository root.
FROM rust:1.88-bookworm AS build
WORKDIR /repo
COPY . .
RUN cargo build --locked --release --manifest-path services/depth-ingest/Cargo.toml

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin depth-ingest
WORKDIR /app
COPY --from=build /repo/services/depth-ingest/target/release/dataminer-depth-ingest /usr/local/bin/dataminer-depth-ingest
ENV DEPTH_INGEST_BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/dataminer-depth-ingest"]
