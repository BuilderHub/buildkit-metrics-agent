# BuildKit reporting agent — multi-stage build
# Expects src/generated/ to exist (run `make generate` first and commit, or generate in CI).
# Multi-arch: build with buildx for linux/amd64 or linux/arm64 (e.g. --platform linux/amd64,linux/arm64).

ARG TARGETOS
ARG TARGETARCH
ARG TARGETPLATFORM
FROM --platform=$TARGETPLATFORM rust:1-bookworm AS builder
WORKDIR /build
# OpenSSL dev headers/libs for openssl-sys (tonic/hyper).
RUN apt-get update && apt-get install -y --no-install-recommends libssl-dev pkg-config && rm -rf /var/lib/apt/lists/*

# Copy workspace and dependency manifests first for better layer caching.
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY proto ./proto
COPY src ./src
COPY tools ./tools

RUN cargo build --release

# Runtime: Chainguard minimal image for glibc binaries.
FROM --platform=$TARGETPLATFORM cgr.dev/chainguard/glibc-dynamic:latest
COPY --from=builder /build/target/release/buildkit-agent /usr/local/bin/buildkit-agent

ENV BUILDKIT_ADDR=unix:///run/buildkit/buildkitd.sock
ENV METRICS_ADDR=0.0.0.0:9090

EXPOSE 9090
ENTRYPOINT ["/usr/local/bin/buildkit-agent"]
