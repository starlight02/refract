# syntax=docker/dockerfile:1.7

FROM node:24.12.0-bookworm-slim AS web-builder
WORKDIR /build/web
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml web/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm run build

FROM rust:1.90-bookworm AS rust-builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY --from=web-builder /build/web/dist ./web/dist
RUN cargo build --locked --release -p refract-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 refract \
    && useradd --uid 10001 --gid refract --no-create-home --shell /usr/sbin/nologin refract \
    && install -d -o refract -g refract /data

COPY --from=rust-builder /build/target/release/refract-server /usr/local/bin/refract-server

USER refract
WORKDIR /data
ENV REFRACT_LISTEN=0.0.0.0:3939 \
    REFRACT_DATABASE=/data/refract.db \
    REFRACT_REQUIRE_AUTH=true
EXPOSE 3939
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["curl", "--fail", "--silent", "--show-error", "http://127.0.0.1:3939/health/ready"]
ENTRYPOINT ["/usr/local/bin/refract-server"]
