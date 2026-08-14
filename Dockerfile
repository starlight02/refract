# syntax=docker/dockerfile:1.7

FROM node:24-alpine AS web-builder
WORKDIR /build/web
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml web/pnpm-workspace.yaml ./
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm run build

FROM rust:1.90-alpine AS rust-builder
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY --from=web-builder /build/web/dist ./web/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release -p refract-server && \
    cp /build/target/release/refract-server /build/refract-server

FROM alpine:3.21 AS runtime
RUN apk add --no-cache ca-certificates curl tzdata \
    && addgroup -g 10001 refract \
    && adduser -u 10001 -G refract -s /sbin/nologin -D refract \
    && install -d -o refract -g refract /data

COPY --from=rust-builder /build/refract-server /usr/local/bin/refract-server

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
