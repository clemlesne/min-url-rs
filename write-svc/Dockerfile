# syntax=docker/dockerfile:1
# check=skip=UndefinedVar

# Rust version
ARG RUST_VERSION=1.86.0

# Build container
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

# Source platform from buildx "platform" argument
ARG TARGETPLATFORM

# Build directory
WORKDIR /app

# Copy sources
COPY . .

# Build
RUN --mount=target=/app/target/,type=cache,id=build-write-svc-${TARGETPLATFORM},sharing=locked --mount=target=/usr/local/cargo/registry/,type=cache,id=cargo-${TARGETPLATFORM},sharing=locked \
    cargo build --release \
    && cp ./target/release/write-svc /write-svc

# Output container
FROM debian:bookworm-slim AS final

# Source platform from buildx "platform" argument
ARG TARGETPLATFORM

# Setup user
RUN adduser \
    --disabled-password \
    --gecos "" \
    --home "/nonexistent" \
    --no-create-home \
    --shell "/sbin/nologin" \
    --uid "10001" \
    appuser

# Copy binary
COPY --from=builder /write-svc /usr/local/bin
RUN chown appuser /usr/local/bin/write-svc

# Configure logging
ENV RUST_LOG="hello_rs=debug,info"

# Setup environment
ENTRYPOINT ["write-svc"]
EXPOSE 8080/tcp
USER appuser
