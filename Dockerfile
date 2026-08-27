# check=skip=SecretsUsedInArgOrEnv
ARG NPM_REGISTRY=https://registry.npmjs.org
ARG ALPINE_MIRROR=https://dl-cdn.alpinelinux.org/alpine
# 构建源可用 --build-arg + --build-context 切换（CI/release 加速用）：
#   admin_src: admin-builder（镜像内构建后台，默认）/ admin-dist（预构建产物）
#   bin_src: rust-builder（镜像内编译，默认）/ bin（预构建二进制，
#            布局 bin/<arch>/refract-server，多平台按 $TARGETARCH 选取）
# 不传参数时：后台与后端都在镜像内现场构建；homepage 是独立部署产物，
# 不会被嵌入网关二进制。
ARG admin_src=admin-builder
ARG bin_src=rust-builder

FROM node:24.19-alpine AS admin-builder
ARG NPM_REGISTRY
WORKDIR /build
RUN npm install -g pnpm@11.22.0 --registry="$NPM_REGISTRY" && \
    pnpm config set registry "$NPM_REGISTRY"
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY apps/admin/package.json ./apps/admin/package.json
COPY packages/contracts/package.json ./packages/contracts/package.json
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile --filter @refract/admin... --registry="$NPM_REGISTRY"
COPY apps/admin/ ./apps/admin/
COPY packages/contracts/ ./packages/contracts/
RUN pnpm --filter @refract/admin build
# 统一输出位置：无论选哪个 admin 源，dist 都取 /admin/dist。
RUN mkdir -p /admin && cp -r /build/apps/admin/dist /admin/dist
FROM $admin_src AS admin-final

FROM rust:1.98-alpine AS rust-builder
ARG ALPINE_MIRROR
ARG TARGETARCH
# Alpine package revisions rotate out of stable indexes; pin the release branch instead.
# hadolint ignore=DL3018
RUN sed -i "s|https://dl-cdn.alpinelinux.org/alpine|${ALPINE_MIRROR}|g" /etc/apk/repositories \
    && apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY --from=admin-final /admin/dist ./apps/admin/dist
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
    && apk add --no-cache ca-certificates tzdata \
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
    CMD ["wget", "-q", "-O", "/dev/null", "http://127.0.0.1:3939/health/ready"]
ENTRYPOINT ["/usr/local/bin/refract-server"]
