# check=skip=SecretsUsedInArgOrEnv
ARG NPM_REGISTRY=https://registry.npmjs.org
ARG ALPINE_MIRROR=https://dl-cdn.alpinelinux.org/alpine
# 构建源可用 --build-arg + --build-context 切换（CI/release 加速用）：
#   web_src: web-builder（镜像内构建前端，默认）/ web-dist（预构建产物）
#   bin_src: rust-builder（镜像内编译，默认）/ bin（预构建二进制，
#            布局 bin/<arch>/refract-server，多平台按 $TARGETARCH 选取）
# 不传参数时行为与从前完全一致：前端与后端都在镜像内现场构建。
ARG web_src=web-builder
ARG bin_src=rust-builder

FROM node:26-alpine AS web-builder
ARG NPM_REGISTRY
WORKDIR /build/web
RUN npm install -g pnpm@11.21.0 --registry="$NPM_REGISTRY" && \
    pnpm config set registry "$NPM_REGISTRY"
COPY web/package.json web/pnpm-lock.yaml web/pnpm-workspace.yaml ./
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile --registry="$NPM_REGISTRY"
COPY web/ ./
RUN pnpm run build
# 统一输出位置：无论选哪个 web 源，dist 都取 /web/dist。
RUN mkdir -p /web && cp -r /build/web/dist /web/dist
FROM $web_src AS web-final

FROM rust:1.97-alpine AS rust-builder
ARG ALPINE_MIRROR
ARG TARGETARCH
# Alpine package revisions rotate out of stable indexes; pin the release branch instead.
# hadolint ignore=DL3018
RUN sed -i "s|https://dl-cdn.alpinelinux.org/alpine|${ALPINE_MIRROR}|g" /etc/apk/repositories \
    && apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY --from=web-final /web/dist ./web/dist
# 产物统一放 /<arch>/refract-server，与外部 bin context 布局一致。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release -p refract-server && \
    mkdir -p "/$TARGETARCH" && \
    cp /build/target/release/refract-server "/$TARGETARCH/refract-server"

FROM $bin_src AS bin-final

FROM alpine:3.24 AS runtime
ARG ALPINE_MIRROR
ARG TARGETARCH
# Alpine package revisions rotate out of stable indexes; pin the release branch instead.
# hadolint ignore=DL3018
RUN sed -i "s|https://dl-cdn.alpinelinux.org/alpine|${ALPINE_MIRROR}|g" /etc/apk/repositories \
    && apk add --no-cache ca-certificates curl tzdata \
    && addgroup -g 10001 refract \
    && adduser -u 10001 -G refract -s /sbin/nologin -D refract \
    && install -d -o refract -g refract /data

# 预构建二进制（--build-context bin=...）或 rust-builder 编译产物，
# 统一在 /<arch>/refract-server，按目标平台架构选取。
COPY --from=bin-final "/$TARGETARCH/refract-server" /usr/local/bin/refract-server

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
