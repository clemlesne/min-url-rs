# syntax=docker/dockerfile:1
# check=skip=UndefinedVar

# Rust version
ARG RUST_VERSION=1.86.0

# Build container
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

# Source platform from buildx "platform" argument
ARG TARGETPLATFORM

# Service to package
ARG SERVICE_NAME

# Build directory
WORKDIR /app-root

# Copy sources
COPY . .

# Build
RUN --mount=target=/app-root/target/,type=cache,id=build-${TARGETPLATFORM},sharing=locked --mount=target=/usr/local/cargo/registry/,type=cache,id=cargo-${TARGETPLATFORM},sharing=locked \
    cargo build --release --package ${SERVICE_NAME} \
    && mv target/release/${SERVICE_NAME} service

# Output container
FROM debian:bookworm-slim AS final

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
COPY --from=builder /app-root/service /usr/local/bin
RUN chown appuser /usr/local/bin/service

# Setup environment
ENTRYPOINT ["service"]
EXPOSE 8080/tcp
USER appuser

# Configure logging
ENV RUST_LOG="hello_rs=debug,info"
