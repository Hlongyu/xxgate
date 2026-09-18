ARG RUST_BUILD_IMAGE=rust:1.96.0-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim
FROM ${RUST_BUILD_IMAGE} AS builder
ARG CARGO_BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
WORKDIR /build
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake pkg-config libssl-dev perl \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p xxgate-server \
    && cp target/release/xxgate-server /usr/local/bin/xxgate-server

FROM ${RUNTIME_IMAGE} AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 xxgate \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin xxgate
COPY --from=builder /usr/local/bin/xxgate-server /usr/local/bin/xxgate-server
ENV XXGATE_BIND=0.0.0.0:8787 XXGATE_SECURE_COOKIES=1
USER 10001:10001
EXPOSE 8787
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=20s --timeout=5s --start-period=20s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:8787/healthz >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/xxgate-server"]
